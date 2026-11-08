//! `scripting.registerContentScripts` and its siblings: content scripts an
//! extension adds at run time. They run beside the manifest's, through the
//! same plugin data; those registered to persist are kept in the extension's
//! storage folder and come back at the next start.

use std::path::{Path, PathBuf};

use serde_json::Value;
use zephium_core::ids::ProfileId;
use zephium_webext_linux::content::{self, Group};

/// Where persisting registrations are kept, in the storage folder.
const STATE_FILE: &str = "registered-content-scripts.json";
/// The API layer, first in every content script list as in the manifest's.
const API_LAYER: &str = "__zephium__/compat.js";

pub(super) struct Registered {
    scripts: Vec<Value>,
    state_file: Option<PathBuf>,
}

impl Registered {
    pub(super) fn open(storage: Option<&Path>) -> Self {
        let state_file = storage.map(|dir| dir.join(STATE_FILE));
        let scripts = state_file
            .as_ref()
            .and_then(|file| std::fs::read(file).ok())
            .and_then(|bytes| serde_json::from_slice::<Vec<Value>>(&bytes).ok())
            .unwrap_or_default();
        Self { scripts, state_file }
    }

    /// The registrations as plugin groups; one whose files are gone is left out.
    pub(super) fn groups(&self, package: &Path) -> Vec<Group> {
        self.scripts.iter().filter_map(|script| group(package, script).ok()).collect()
    }

    fn save(&self) {
        let Some(file) = &self.state_file else {
            return;
        };
        let kept: Vec<&Value> = self.scripts.iter().filter(|script| script["persistAcrossSessions"] != false).collect();
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file, Value::from(kept.into_iter().cloned().collect::<Vec<_>>()).to_string());
    }

    /// One API call; `Ok((answer, changed))`.
    fn call(&mut self, package: &Path, method: &str, args: &[Value]) -> Result<(Value, bool), String> {
        let first = args.first().cloned().unwrap_or(Value::Null);
        match method {
            "registerContentScripts" => {
                let scripts = first.as_array().cloned().ok_or("registerContentScripts takes a list of scripts.")?;
                let mut ids: Vec<&str> = self.scripts.iter().filter_map(|script| script["id"].as_str()).collect();
                for script in &scripts {
                    let id = script_id(script)?;
                    if ids.contains(&id) {
                        return Err(format!("Duplicate script ID '{id}'"));
                    }
                    ids.push(id);
                    group(package, script)?;
                }
                self.scripts.extend(scripts);
                Ok((Value::Null, true))
            }
            "updateContentScripts" => {
                let changes = first.as_array().cloned().ok_or("updateContentScripts takes a list of scripts.")?;
                let mut scripts = self.scripts.clone();
                for change in &changes {
                    let id = script_id(change)?;
                    let script = scripts.iter_mut().find(|script| script["id"] == id).ok_or(format!("No script with ID '{id}'"))?;
                    for (key, value) in change.as_object().into_iter().flatten() {
                        script[key] = value.clone();
                    }
                    group(package, script)?;
                }
                self.scripts = scripts;
                Ok((Value::Null, true))
            }
            "getRegisteredContentScripts" => {
                let ids = first["ids"].as_array();
                let scripts = self.scripts.iter().filter(|script| ids.is_none_or(|ids| ids.contains(&script["id"]))).cloned();
                Ok((Value::Array(scripts.collect()), false))
            }
            "unregisterContentScripts" => {
                match first["ids"].as_array() {
                    Some(ids) => {
                        if let Some(missing) = ids.iter().find(|id| !self.scripts.iter().any(|script| script["id"] == **id)) {
                            return Err(format!("Nonexistent script ID '{}'", missing.as_str().unwrap_or_default()));
                        }
                        self.scripts.retain(|script| !ids.contains(&script["id"]));
                    }
                    None => self.scripts.clear(),
                }
                Ok((Value::Null, true))
            }
            _ => Err(format!("scripting.{method} is not available on Linux yet.")),
        }
    }
}

fn script_id(script: &Value) -> Result<&str, String> {
    match script["id"].as_str() {
        Some(id) if !id.is_empty() && !id.starts_with('_') => Ok(id),
        _ => Err("A content script needs an ID that does not start with '_'.".into()),
    }
}

/// A registration as a plugin group, its files read from `package`. Scripts
/// for the page's own world wait, as the manifest's do.
fn group(package: &Path, script: &Value) -> Result<Group, String> {
    if script["world"] == "MAIN" {
        return Err("Content scripts in the page's own world are not available on Linux yet.".into());
    }
    let strings = |key: &str| -> Vec<String> {
        script[key].as_array().map(|list| list.iter().filter_map(|item| item.as_str().map(|text| text.trim_start_matches('/').to_owned())).collect()).unwrap_or_default()
    };
    let (js, css) = (strings("js"), strings("css"));
    if js.is_empty() && css.is_empty() {
        return Err(format!("Script with ID '{}' must specify at least one js or css file.", script["id"].as_str().unwrap_or_default()));
    }
    let matches = strings("matches");
    if matches.is_empty() {
        return Err(format!("Script with ID '{}' must specify 'matches'.", script["id"].as_str().unwrap_or_default()));
    }
    let run_at = match script["runAt"].as_str() {
        Some("document_start") => "document_start",
        Some("document_end") => "document_end",
        _ => "document_idle",
    };
    let js: Vec<String> = if js.is_empty() { js } else { std::iter::once(API_LAYER.to_owned()).chain(js).collect() };
    content::group(package, matches, strings("excludeMatches"), &js, &css, run_at, script["allFrames"] == true)
}

impl super::super::EngineHost {
    pub(super) fn linux_registered(&mut self, profile: ProfileId, extension_id: &str, method: &str, args: &[Value]) -> Result<Value, String> {
        let install = self
            .linux_extensions
            .profiles
            .get_mut(&profile)
            .and_then(|state| state.install_mut(extension_id))
            .ok_or("The extension is not running.")?;
        let (answer, changed) = install.registered.call(&install.root, method, args)?;
        if changed {
            install.registered.save();
            install.refresh_groups();
            self.linux_extensions_sync(profile);
        }
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn package() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("__zephium__")).unwrap();
        std::fs::write(dir.path().join("__zephium__/compat.js"), "api()").unwrap();
        std::fs::write(dir.path().join("a.js"), "one()").unwrap();
        dir
    }

    #[test]
    fn registrations_run_after_the_api_layer_and_can_be_listed_and_removed() {
        let dir = package();
        let mut registered = Registered::open(None);
        let script = json!({ "id": "a", "matches": ["<all_urls>"], "js": ["a.js"], "runAt": "document_start" });
        assert!(registered.call(dir.path(), "registerContentScripts", &[json!([script])]).unwrap().1);
        let groups = registered.groups(dir.path());
        assert_eq!(groups[0].js, ["api()", "one()"]);
        assert_eq!(groups[0].run_at, "document_start");
        assert!(registered.call(dir.path(), "registerContentScripts", &[json!([{ "id": "a", "matches": ["<all_urls>"], "js": ["a.js"] }])]).is_err());
        let listed = registered.call(dir.path(), "getRegisteredContentScripts", &[json!({ "ids": ["a"] })]).unwrap().0;
        assert_eq!(listed.as_array().unwrap().len(), 1);
        registered.call(dir.path(), "unregisterContentScripts", &[json!({})]).unwrap();
        assert!(registered.groups(dir.path()).is_empty());
    }

    #[test]
    fn a_registration_with_a_missing_file_is_refused() {
        let dir = package();
        let mut registered = Registered::open(None);
        let script = json!({ "id": "b", "matches": ["<all_urls>"], "js": ["gone.js"] });
        assert!(registered.call(dir.path(), "registerContentScripts", &[json!([script])]).is_err());
        assert!(registered.scripts.is_empty());
    }
}
