//! `declarativeNetRequest`: an extension's static rulesets, dynamic rules and
//! session rules, compiled into one WebKit content-blocker list that every
//! tab of the profile carries.
//!
//! Reading rulesets and translating them can take a moment for a large
//! blocker, so it runs off the main thread; WebKit compiles and caches the
//! list. A newer change supersedes a list still compiling.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use zephium_core::ids::ProfileId;
use zephium_webext::manifest::Manifest;

use crate::platform::imp::{
    add_content_policy_to_view, compile_content_policy, remove_content_policy_from_view, NativeContentPolicy,
};

/// Chrome's limits, which extensions read and plan by.
const MAX_ENABLED_STATIC_RULESETS: usize = 50;
const MAX_DYNAMIC_RULES: usize = 30_000;
const MAX_SESSION_RULES: usize = 5_000;
const GUARANTEED_STATIC_RULES: usize = 30_000;
/// What the extension keeps across restarts, in its storage folder.
const STATE_FILE: &str = "declarative-net-request.json";

pub(super) struct Rules {
    rulesets: Vec<Ruleset>,
    enabled: Vec<String>,
    dynamic: Vec<Value>,
    session: Vec<Value>,
    state_file: Option<PathBuf>,
    /// Bumped by every change, so only the newest list is applied.
    revision: u64,
    /// The list the profile's tabs carry.
    pub(super) applied: Option<NativeContentPolicy>,
}

struct Ruleset {
    id: String,
    path: String,
}

impl Rules {
    pub(super) fn from_manifest(manifest: &Manifest, storage: Option<&Path>) -> Self {
        // Without the permission, the manifest's rulesets do nothing.
        let permitted = manifest
            .permissions()
            .iter()
            .any(|permission| permission == "declarativeNetRequest" || permission == "declarativeNetRequestWithHostAccess");
        let resources = if permitted {
            manifest.raw()["declarative_net_request"]["rule_resources"].as_array().cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        let rulesets: Vec<Ruleset> = resources
            .iter()
            .filter_map(|resource| Some(Ruleset { id: resource["id"].as_str()?.to_owned(), path: resource["path"].as_str()?.to_owned() }))
            .collect();
        let mut rules = Self {
            enabled: resources
                .iter()
                .filter(|resource| resource["enabled"] == true)
                .filter_map(|resource| resource["id"].as_str().map(str::to_owned))
                .collect(),
            rulesets,
            dynamic: Vec::new(),
            session: Vec::new(),
            state_file: storage.map(|dir| dir.join(STATE_FILE)),
            revision: 0,
            applied: None,
        };
        let saved = rules.state_file.as_ref().and_then(|file| std::fs::read(file).ok());
        if let Some(saved) = saved.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) {
            if let Some(enabled) = saved["enabled"].as_array() {
                rules.enabled = enabled.iter().filter_map(|id| id.as_str().map(str::to_owned)).filter(|id| rules.has_ruleset(id)).collect();
            }
            rules.dynamic = saved["dynamic"].as_array().cloned().unwrap_or_default();
        }
        rules
    }

    fn has_ruleset(&self, id: &str) -> bool {
        self.rulesets.iter().any(|ruleset| ruleset.id == id)
    }

    /// Whether there is anything to compile at all.
    fn is_empty(&self) -> bool {
        self.enabled.is_empty() && self.dynamic.is_empty() && self.session.is_empty()
    }

    fn save(&self) {
        let Some(file) = &self.state_file else {
            return;
        };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file, json!({ "enabled": self.enabled, "dynamic": self.dynamic }).to_string());
    }

    /// What a worker needs to build the list: the enabled rulesets' files,
    /// then the dynamic and session rules.
    fn sources(&self, root: &Path) -> (Vec<PathBuf>, Vec<Value>) {
        let files = self
            .rulesets
            .iter()
            .filter(|ruleset| self.enabled.contains(&ruleset.id))
            .filter_map(|ruleset| zephium_webext::manifest::normalize_resource(&ruleset.path).map(|path| root.join(path)))
            .collect();
        (files, self.dynamic.iter().chain(&self.session).cloned().collect())
    }

    fn call(&mut self, method: &str, args: &[Value]) -> Result<(Value, bool), String> {
        let options = args.first().cloned().unwrap_or(Value::Null);
        Ok(match method {
            "getEnabledRulesets" => (json!(self.enabled), false),
            "updateEnabledRulesets" => {
                let ids = |key: &str| -> Vec<String> {
                    options[key].as_array().map(|list| list.iter().filter_map(|id| id.as_str().map(str::to_owned)).collect()).unwrap_or_default()
                };
                let (enable, disable) = (ids("enableRulesetIds"), ids("disableRulesetIds"));
                if let Some(unknown) = enable.iter().chain(&disable).find(|id| !self.has_ruleset(id)) {
                    return Err(format!("Invalid ruleset id: {unknown}."));
                }
                let mut enabled: Vec<String> = self.enabled.iter().filter(|id| !disable.contains(id)).cloned().collect();
                for id in enable {
                    if !enabled.contains(&id) {
                        enabled.push(id);
                    }
                }
                if enabled.len() > MAX_ENABLED_STATIC_RULESETS {
                    return Err("The number of enabled static rulesets exceeds the limit.".into());
                }
                self.enabled = enabled;
                (Value::Null, true)
            }
            "getDynamicRules" => (filtered(&self.dynamic, &options), false),
            "getSessionRules" => (filtered(&self.session, &options), false),
            "updateDynamicRules" => {
                self.dynamic = updated(&self.dynamic, &options, MAX_DYNAMIC_RULES)?;
                (Value::Null, true)
            }
            "updateSessionRules" => {
                self.session = updated(&self.session, &options, MAX_SESSION_RULES)?;
                (Value::Null, true)
            }
            "isRegexSupported" => {
                let regex = options["regex"].as_str().unwrap_or_default();
                if zephium_webext_linux::dnr::regex_supported(regex) {
                    (json!({ "isSupported": true }), false)
                } else {
                    (json!({ "isSupported": false, "reason": "syntaxError" }), false)
                }
            }
            "getAvailableStaticRuleCount" => (json!(GUARANTEED_STATIC_RULES), false),
            "getDisabledRuleIds" => (json!([]), false),
            "getMatchedRules" => (json!({ "rulesMatchedInfo": [] }), false),
            "setExtensionActionOptions" | "updateStaticRules" => (Value::Null, false),
            _ => return Err(format!("declarativeNetRequest.{method} is not available on Linux yet.")),
        })
    }
}

/// `getDynamicRules({ ruleIds })` and its session twin.
fn filtered(rules: &[Value], options: &Value) -> Value {
    match options["ruleIds"].as_array() {
        Some(ids) => Value::Array(rules.iter().filter(|rule| ids.contains(&rule["id"])).cloned().collect()),
        None => Value::Array(rules.to_vec()),
    }
}

/// `updateDynamicRules({ removeRuleIds, addRules })`: removals first, and an
/// added rule may not reuse a kept rule's ID.
fn updated(rules: &[Value], options: &Value, limit: usize) -> Result<Vec<Value>, String> {
    let remove = options["removeRuleIds"].as_array().cloned().unwrap_or_default();
    let mut kept: Vec<Value> = rules.iter().filter(|rule| !remove.contains(&rule["id"])).cloned().collect();
    for rule in options["addRules"].as_array().cloned().unwrap_or_default() {
        if !rule["id"].is_u64() || rule["id"] == 0 {
            return Err("Rule with an invalid id.".into());
        }
        if kept.iter().any(|existing| existing["id"] == rule["id"]) {
            return Err(format!("Rule with id {} does not have a unique ID.", rule["id"]));
        }
        kept.push(rule);
    }
    if kept.len() > limit {
        return Err("The number of rules exceeds the limit.".into());
    }
    Ok(kept)
}

/// The text of one compiled list, for WebKit's compiler.
struct ListText(Arc<str>);

impl zephium_core::blocker::DeclarativeRuleText for ListText {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn load(&self) -> Option<Arc<str>> {
        Some(self.0.clone())
    }
}

/// Read and translate on a worker; the rules the files hold first.
fn build(files: Vec<PathBuf>, extra: Vec<Value>) -> (String, usize) {
    let mut rules: Vec<Value> = files
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .filter_map(|text| zephium_webext::manifest::parse_lenient(&text).ok())
        .filter_map(|value| value.as_array().cloned())
        .flatten()
        .collect();
    rules.extend(extra);
    zephium_webext_linux::dnr::translate(&rules)
}

impl super::super::EngineHost {
    /// A `declarativeNetRequest` call; a change rebuilds the list.
    pub(super) fn linux_dnr(&mut self, profile: ProfileId, extension_id: &str, method: &str, args: &[Value]) -> Result<Value, String> {
        let install = self
            .linux_extensions
            .profiles
            .get_mut(&profile)
            .and_then(|state| state.install_mut(extension_id))
            .ok_or("The extension is not running.")?;
        let (answer, changed) = install.dnr.call(method, args)?;
        if changed {
            if method != "updateSessionRules" {
                install.dnr.save();
            }
            self.linux_dnr_rebuild(profile, extension_id);
        }
        Ok(answer)
    }

    /// Build `extension_id`'s list afresh and put it on the profile's tabs.
    pub(super) fn linux_dnr_rebuild(&mut self, profile: ProfileId, extension_id: &str) {
        let Some(install) = self.linux_extensions.profiles.get_mut(&profile).and_then(|state| state.install_mut(extension_id)) else {
            return;
        };
        install.dnr.revision += 1;
        let revision = install.dnr.revision;
        if install.dnr.is_empty() {
            self.linux_dnr_compiled(profile, extension_id.to_owned(), revision, None);
            return;
        }
        let (files, extra) = install.dnr.sources(&install.root);
        let extension_id = extension_id.to_owned();
        let cache = crate::erasure::prepare_profile_directory(&self.profiles_root, profile).ok().map(|dir| dir.join("webext-dnr"));
        std::thread::spawn(move || {
            let (text, skipped) = build(files, extra);
            gtk::glib::MainContext::default().invoke(move || {
                super::super::dispatch::best_effort_with(move |host| {
                    host.linux_dnr_translated(profile, extension_id, revision, text, skipped, cache);
                });
            });
        });
    }

    fn linux_dnr_translated(&mut self, profile: ProfileId, extension_id: String, revision: u64, text: String, skipped: usize, cache: Option<PathBuf>) {
        super::trace!("{extension_id}: network rules translated, {} bytes, {skipped} left out", text.len());
        let Some(cache) = cache else {
            return;
        };
        let _ = std::fs::create_dir_all(&cache);
        let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        let text: Arc<dyn zephium_core::blocker::DeclarativeRuleText> = Arc::new(ListText(text.into()));
        // The compile runs in WebKit; its handle may go, the result still arrives.
        let _ = compile_content_policy(&cache, text, digest, move |result| {
            super::super::dispatch::best_effort_with(move |host| match result {
                Ok(policy) => host.linux_dnr_compiled(profile, extension_id, revision, Some(policy)),
                Err(error) => super::trace!("{extension_id}: network rules did not compile: {error:?}"),
            });
        });
    }

    fn linux_dnr_compiled(&mut self, profile: ProfileId, extension_id: String, revision: u64, policy: Option<NativeContentPolicy>) {
        let views = self.linux_tab_views(profile);
        let Some(install) = self.linux_extensions.profiles.get_mut(&profile).and_then(|state| state.install_mut(&extension_id)) else {
            return;
        };
        if install.dnr.revision != revision {
            return;
        }
        let previous = std::mem::replace(&mut install.dnr.applied, policy);
        for view in &views {
            if let Some(previous) = &previous {
                remove_content_policy_from_view(view, previous);
            }
            if let Some(current) = &install.dnr.applied {
                add_content_policy_to_view(view, current);
            }
        }
        super::trace!("{extension_id}: network rules on {} tab(s)", views.len());
    }

    /// Every installed extension's list on every tab of the profile, as
    /// after a new tab; adding a list twice keeps it once.
    pub(super) fn linux_dnr_sync(&self, profile: ProfileId) {
        let Some(state) = self.linux_extensions.profiles.get(&profile) else {
            return;
        };
        let views = self.linux_tab_views(profile);
        for policy in state.installs.values().filter_map(|install| install.dnr.applied.as_ref()) {
            for view in &views {
                add_content_policy_to_view(view, policy);
            }
        }
    }

    /// An extension stops: its list comes off the profile's tabs.
    pub(super) fn linux_dnr_remove(&self, profile: ProfileId, policy: &NativeContentPolicy) {
        for view in self.linux_tab_views(profile) {
            remove_content_policy_from_view(&view, policy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_updates_remove_first_and_refuse_duplicate_ids() {
        let rules = vec![json!({ "id": 1 }), json!({ "id": 2 })];
        let next = updated(&rules, &json!({ "removeRuleIds": [1], "addRules": [{ "id": 1 }, { "id": 3 }] }), 10).unwrap();
        assert_eq!(next.iter().map(|rule| rule["id"].as_u64().unwrap()).collect::<Vec<_>>(), [2, 1, 3]);
        assert!(updated(&rules, &json!({ "addRules": [{ "id": 2 }] }), 10).is_err());
        assert!(updated(&rules, &json!({ "addRules": [{ "id": 3 }] }), 2).is_err());
    }

    #[test]
    fn rulesets_follow_the_manifest_and_updates() {
        let manifest = Manifest::from_value(json!({ "permissions": ["declarativeNetRequest"], "declarative_net_request": { "rule_resources": [
            { "id": "ads", "enabled": true, "path": "ads.json" },
            { "id": "privacy", "enabled": false, "path": "privacy.json" } ] } }));
        let mut rules = Rules::from_manifest(&manifest, None);
        assert_eq!(rules.call("getEnabledRulesets", &[]).unwrap().0, json!(["ads"]));
        let change = json!({ "enableRulesetIds": ["privacy"], "disableRulesetIds": ["ads"] });
        assert!(rules.call("updateEnabledRulesets", &[change]).unwrap().1);
        assert_eq!(rules.enabled, ["privacy"]);
        assert!(rules.call("updateEnabledRulesets", &[json!({ "enableRulesetIds": ["nope"] })]).is_err());
    }

    #[test]
    fn without_the_permission_the_rulesets_do_nothing() {
        let manifest = Manifest::from_value(json!({ "declarative_net_request": { "rule_resources": [
            { "id": "ads", "enabled": true, "path": "ads.json" } ] } }));
        assert!(Rules::from_manifest(&manifest, None).is_empty());
    }
}
