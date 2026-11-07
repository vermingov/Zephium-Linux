//! The browser's side of each API call an extension context makes. Requests
//! arrive as `{"id", "api", "args"}` (the protocol in docs/extensions.md); a
//! call either answers at once or hands its request to the routing, which
//! answers when the receivers have.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use webkit2gtk::{UserMessageExt, WebView, WebViewExt};
use zephium_core::extensions::{
    ExtensionBrowserRequest, ExtensionBrowserRequestAction, ExtensionBrowserRequestId, ExtensionBrowserSurface,
    ExtensionBrowserTab, ExtensionBrowserWindow,
};
use zephium_core::ids::{ItemId, ProfileId};
use zephium_core::ports::engine::EngineEvent;
use zephium_webext_linux::storage::{Area, Changes, LOCAL_QUOTA};
use zephium_webext_process::matching::any_matches;

use super::routing::{error, ok, raise, Endpoint};
use crate::platform::imp::extension_channel::{Message, SenderKind};

/// Chrome's quota for `storage.sync`.
const SYNC_QUOTA: usize = 100 * 1024;
/// Chrome's shortest alarm period for installed extensions.
const SHORTEST_ALARM: Duration = Duration::from_secs(30);

/// An extension's three storage areas.
pub(super) struct Storage {
    local: Area,
    sync: Area,
    session: Area,
}

impl Storage {
    /// On disk under `dir` when the profile has one, in memory otherwise.
    pub(super) fn open(dir: Option<PathBuf>) -> Result<Self, String> {
        let area = |name: &str| match &dir {
            Some(dir) => Area::on_disk(dir.join(format!("{name}.json"))).map_err(|error| error.to_string()),
            None => Ok(Area::in_memory()),
        };
        Ok(Self { local: area("local")?, sync: area("sync")?, session: Area::in_memory() })
    }

    fn area(&mut self, name: &str) -> Result<&mut Area, String> {
        match name {
            "local" => Ok(&mut self.local),
            "sync" => Ok(&mut self.sync),
            "session" => Ok(&mut self.session),
            other => Err(format!("storage.{other} is not available")),
        }
    }
}

/// How a call ends: answered now, or its request went to the routing.
enum Outcome {
    Answer(Result<Value, String>),
    Routed,
}

impl super::super::EngineHost {
    /// One request from extension code; see the module comment.
    pub(super) fn linux_extension_message(&mut self, profile: ProfileId, message: Message) {
        let Ok(request) = serde_json::from_str::<Value>(&message.body) else {
            return;
        };
        let id = request["id"].as_u64().unwrap_or(0);
        let api = request["api"].as_str().unwrap_or_default().to_owned();
        let args = request["args"].as_array().cloned().unwrap_or_default();
        let known = self
            .linux_extensions
            .profiles
            .get(&profile)
            .is_some_and(|state| state.install(&message.extension_id).is_some());
        if !known {
            return message.reply(&error(id, "The extension is not running."));
        }
        let from = Endpoint { view: message.view.clone(), frame: message.frame_id };
        let reply_with = |message: Message, result: Result<Value, String>| match result {
            Ok(value) => message.reply(&ok(id, value)),
            Err(reason) => message.reply(&error(id, &reason)),
        };
        // The calls that answer later take the request itself.
        match api.as_str() {
            "runtime.sendMessage" => {
                let payload = message_argument(&args);
                let sender = self.linux_sender(profile, &message);
                let targets = self.linux_extension_pages(profile, &message.extension_id, Some(&from));
                return self.linux_send_message(profile, message, id, payload, sender, targets);
            }
            "scripting.executeScript" | "scripting.insertCSS" | "scripting.removeCSS" | "tabs.executeScript" | "tabs.insertCSS" | "tabs.removeCSS" => {
                return self.linux_inject(profile, message, id, &api, &args);
            }
            "tabs.sendMessage" => {
                let options = args.get(2).cloned().unwrap_or(Value::Null);
                let target = arg_i64(&args, 0).and_then(|tab| self.linux_tab_view(profile, tab));
                let Some(view) = target else {
                    return reply_with(message, Err("No tab with that id.".into()));
                };
                let frame = options["frameId"].as_u64().unwrap_or(0);
                let payload = args.get(1).cloned().unwrap_or(Value::Null);
                let sender = self.linux_sender(profile, &message);
                return self.linux_send_message(profile, message, id, payload, sender, vec![Endpoint { view, frame }]);
            }
            _ => {}
        }
        match self.linux_call(profile, &message, &from, &api, &args) {
            Outcome::Answer(result) => reply_with(message, result),
            Outcome::Routed => {}
        }
    }

    /// `scripting.*` and MV2's `tabs.executeScript` / `insertCSS`: code or CSS
    /// run in the extension's world of a tab it may touch.
    fn linux_inject(&mut self, profile: ProfileId, message: Message, id: u64, api: &str, args: &[Value]) {
        let mv2 = api.starts_with("tabs.");
        let (tab, details) = if mv2 {
            match args.first() {
                Some(Value::Number(_)) => (arg_i64(args, 0), args.get(1).cloned().unwrap_or(Value::Null)),
                _ => (None, args.first().cloned().unwrap_or(Value::Null)),
            }
        } else {
            let injection = args.first().cloned().unwrap_or(Value::Null);
            (injection["target"]["tabId"].as_i64(), injection)
        };
        let prepared = self.linux_injection(profile, &message.extension_id, tab, api, &details);
        let (view, code) = match prepared {
            Ok(prepared) => prepared,
            Err(reason) => return message.reply(&error(id, &reason)),
        };
        let run = webkit2gtk::UserMessage::new(
            zephium_webext_process::RUN_MESSAGE,
            Some(&gtk::glib::ToVariant::to_variant(&(message.extension_id.as_str(), code.as_str()))),
        );
        view.send_message_to_page(&run, None::<&gtk::gio::Cancellable>, move |reply| {
            let answer = reply
                .ok()
                .and_then(|reply| reply.parameters())
                .and_then(|parameters| parameters.get::<(String,)>())
                .and_then(|(text,)| serde_json::from_str::<Value>(&text).ok())
                .unwrap_or_else(|| json!({ "error": "The page could not be reached." }));
            match answer.get("error").and_then(Value::as_str) {
                Some(reason) => message.reply(&error(id, reason)),
                None if mv2 => message.reply(&ok(id, json!([answer["ok"]]))),
                None => message.reply(&ok(id, json!([{ "frameId": 0, "result": answer["ok"] }]))),
            }
        });
    }

    /// The tab's view and the code to run there, once the extension is
    /// allowed to: host permission for the tab's address, or `activeTab`.
    fn linux_injection(&mut self, profile: ProfileId, extension_id: &str, tab: Option<i64>, api: &str, details: &Value) -> Result<(WebView, String), String> {
        let item = match tab {
            Some(number) => self.linux_tab_item(profile, number),
            None => self.linux_focused_tab(profile),
        }
        .ok_or("No tab with that id.")?;
        let view = {
            use wry::WebViewExtUnix;
            self.views.get(&item).map(|observed| observed.view.webview()).ok_or("The tab is not loaded.")?
        };
        let url = view.uri().map(|uri| uri.to_string()).unwrap_or_default();
        let install = self.linux_extensions.profiles.get(&profile).and_then(|state| state.install(extension_id)).ok_or("The extension is not running.")?;
        let allowed = install.active_tabs.contains(&item) || any_matches(&install.hosts, &url);
        if !allowed {
            return Err("Cannot access contents of the page. Extension manifest must request permission to access the respective host.".into());
        }
        let files: Vec<String> = match &details["files"] {
            Value::Array(files) => files.iter().filter_map(|file| file.as_str().map(str::to_owned)).collect(),
            _ => details["file"].as_str().map(str::to_owned).into_iter().collect(),
        };
        let mut source = files.iter().map(|file| read_package_text(&install.root, file)).collect::<Result<Vec<_>, _>>()?.join("\n;");
        let inline = details["code"].as_str().or(details["css"].as_str()).unwrap_or_default();
        if !inline.is_empty() {
            source.push_str(inline);
        }
        let code = if api.ends_with("insertCSS") {
            insert_css(&source)
        } else if api.ends_with("removeCSS") {
            remove_css(&source)
        } else {
            source
        };
        // The API layer goes first, so injected code can use chrome.*.
        Ok((view, format!("{}\n;{code}", zephium_webext_linux::SCRIPT)))
    }

    fn linux_call(&mut self, profile: ProfileId, message: &Message, from: &Endpoint, api: &str, args: &[Value]) -> Outcome {
        let extension_id = message.extension_id.as_str();
        let (namespace, method) = api.split_once('.').unwrap_or((api, ""));
        let result = match namespace {
            "runtime" => self.linux_runtime(profile, message, from, method, args),
            "port" => match method {
                "post" => self.linux_port_post(profile, from, arg_u64(args, 0), args.get(1).cloned().unwrap_or(Value::Null)),
                "disconnect" => self.linux_port_disconnect(profile, from, arg_u64(args, 0)),
                _ => unsupported(api),
            },
            "storage" => self.linux_storage(profile, extension_id, method, args),
            "tabs" => self.linux_tabs(profile, message, from, method, args),
            "windows" => self.linux_windows(profile, message, method, args),
            "action" => self.linux_action_call(profile, extension_id, method, args),
            "alarms" => self.linux_alarms(profile, extension_id, method, args),
            "permissions" => self.linux_permissions(profile, extension_id, method, args),
            "i18n" if method == "getAcceptLanguages" => Ok(json!(["en-US", "en"])),
            "extension" if method == "isAllowedIncognitoAccess" => Ok(json!(false)),
            "commands" if method == "getAll" => Ok(self.linux_commands(profile, extension_id)),
            "notifications" => linux_notification(extension_id, method, args),
            "contextMenus" => Ok(Value::Null),
            "cookies" => Ok(if method == "getAll" { json!([]) } else { Value::Null }),
            "webNavigation" => Ok(Value::Null),
            _ => unsupported(api),
        };
        Outcome::Answer(result)
    }

    fn linux_runtime(&mut self, profile: ProfileId, message: &Message, from: &Endpoint, method: &str, args: &[Value]) -> Result<Value, String> {
        match method {
            "answer" => {
                self.linux_message_answered(profile, arg_u64(args, 0), args.get(1).cloned().unwrap_or(Value::Null), args.get(2).and_then(Value::as_bool).unwrap_or(false));
                Ok(Value::Null)
            }
            "connect" => {
                let info = args.iter().find(|arg| arg.is_object()).cloned().unwrap_or(Value::Null);
                let name = info["name"].as_str().unwrap_or_default().to_owned();
                let sender = self.linux_sender(profile, message);
                let receivers = self.linux_extension_pages(profile, &message.extension_id, Some(from));
                self.linux_connect(profile, &message.extension_id, from.clone(), &name, sender, receivers)
            }
            "getPlatformInfo" => Ok(json!({ "os": "linux", "arch": "x86-64", "nacl_arch": "x86-64" })),
            "openOptionsPage" => {
                self.open_web_extension_options(profile, &message.extension_id);
                Ok(Value::Null)
            }
            "setUninstallURL" => Ok(Value::Null),
            "reload" => {
                if let Some(install) = self.linux_extensions.profiles.get_mut(&profile).and_then(|state| state.install_mut(&message.extension_id)) {
                    if let Some(background) = install.background.take() {
                        background.try_close();
                    }
                }
                self.linux_extensions_sync(profile);
                Ok(Value::Null)
            }
            _ => unsupported(&format!("runtime.{method}")),
        }
    }

    fn linux_storage(&mut self, profile: ProfileId, extension_id: &str, method: &str, args: &[Value]) -> Result<Value, String> {
        let area_name = args.first().and_then(Value::as_str).unwrap_or("local").to_owned();
        let argument = args.get(1).cloned().unwrap_or(Value::Null);
        let install = self.linux_extensions.profiles.get_mut(&profile).and_then(|state| state.install_mut(extension_id)).ok_or("The extension is not running.")?;
        let unlimited = install.permissions.iter().any(|permission| permission == "unlimitedStorage");
        let quota = match area_name.as_str() {
            "local" if !unlimited => Some(LOCAL_QUOTA),
            "sync" => Some(SYNC_QUOTA),
            _ => None,
        };
        let area = install.storage.area(&area_name)?;
        let changes = match method {
            "get" => return Ok(Value::Object(area.get(&argument))),
            "getBytesInUse" => return Ok(json!(area.bytes_in_use(&argument))),
            "set" => area.set(argument.as_object().cloned().unwrap_or_default(), quota)?,
            "remove" => area.remove(&argument)?,
            "clear" => area.clear()?,
            _ => return unsupported(&format!("storage.{method}")),
        };
        if !changes.is_empty() {
            self.linux_storage_changed(profile, extension_id, &area_name, changes);
        }
        Ok(Value::Null)
    }

    /// `storage.onChanged` in every context of the extension.
    fn linux_storage_changed(&mut self, profile: ProfileId, extension_id: &str, area: &str, changes: Changes) {
        let changes = Value::Object(changes);
        for endpoint in self.linux_every_context(profile, extension_id) {
            raise(&endpoint, extension_id, "storage.onChanged", json!([changes.clone(), area]));
            raise(&endpoint, extension_id, &format!("storage.{area}.onChanged"), json!([changes.clone()]));
        }
    }

    fn linux_tabs(&mut self, profile: ProfileId, message: &Message, from: &Endpoint, method: &str, args: &[Value]) -> Result<Value, String> {
        match method {
            "query" => Ok(Value::Array(self.linux_query_tabs(profile, message, args.first().unwrap_or(&Value::Null)))),
            "get" => {
                let wanted = arg_i64(args, 0).ok_or("No tab with that id.")?;
                self.linux_all_tabs(profile).into_iter().find(|tab| tab["id"] == wanted).ok_or_else(|| "No tab with that id.".into())
            }
            "getCurrent" => Ok(self.linux_sender_tab(profile, message).unwrap_or(Value::Null)),
            "create" => {
                let properties = args.first().cloned().unwrap_or(Value::Null);
                let url = properties["url"].as_str().map(|url| self.linux_absolute(profile, &message.extension_id, url));
                self.linux_request_tab(profile, url, properties["active"].as_bool().unwrap_or(true));
                Ok(Value::Null)
            }
            "update" => {
                let (tab, properties) = match args.first() {
                    Some(Value::Number(_)) => (arg_i64(args, 0).and_then(|tab| self.linux_tab_item(profile, tab)), args.get(1).cloned().unwrap_or(Value::Null)),
                    _ => (self.linux_focused_tab(profile), args.first().cloned().unwrap_or(Value::Null)),
                };
                let tab = tab.ok_or("No tab with that id.")?;
                if let Some(url) = properties["url"].as_str() {
                    let url = self.linux_absolute(profile, &message.extension_id, url);
                    self.linux_browser_request(profile, ExtensionBrowserRequestAction::LoadTabUrl { tab, url: Arc::from(url.as_str()) });
                }
                if properties["active"].as_bool() == Some(true) {
                    self.linux_browser_request(profile, ExtensionBrowserRequestAction::ActivateTab { tab });
                }
                Ok(Value::Null)
            }
            "remove" => {
                let numbers: Vec<i64> = match args.first() {
                    Some(Value::Array(list)) => list.iter().filter_map(Value::as_i64).collect(),
                    Some(value) => value.as_i64().into_iter().collect(),
                    None => Vec::new(),
                };
                let tabs: Vec<ItemId> = numbers.into_iter().filter_map(|number| self.linux_tab_item(profile, number)).collect();
                for tab in tabs {
                    self.linux_browser_request(profile, ExtensionBrowserRequestAction::CloseTab { tab });
                }
                Ok(Value::Null)
            }
            "reload" => {
                let tab = arg_i64(args, 0).and_then(|number| self.linux_tab_item(profile, number)).or_else(|| self.linux_focused_tab(profile));
                if let Some(tab) = tab {
                    self.linux_browser_request(profile, ExtensionBrowserRequestAction::ReloadTab { tab });
                }
                Ok(Value::Null)
            }
            "connect" => {
                let view = arg_i64(args, 0).and_then(|tab| self.linux_tab_view(profile, tab)).ok_or("No tab with that id.")?;
                let info = args.get(1).cloned().unwrap_or(Value::Null);
                let frame = info["frameId"].as_u64().unwrap_or(0);
                let name = info["name"].as_str().unwrap_or_default().to_owned();
                let sender = self.linux_sender(profile, message);
                self.linux_connect(profile, &message.extension_id, from.clone(), &name, sender, vec![Endpoint { view, frame }])
            }
            _ => unsupported(&format!("tabs.{method}")),
        }
    }

    fn linux_windows(&mut self, profile: ProfileId, message: &Message, method: &str, args: &[Value]) -> Result<Value, String> {
        let populate = args.iter().any(|arg| arg["populate"].as_bool() == Some(true));
        let windows = self.linux_all_windows(profile, populate);
        match method {
            "getAll" => Ok(Value::Array(windows)),
            "get" => {
                let wanted = arg_i64(args, 0).ok_or("No window with that id.")?;
                windows.into_iter().find(|window| window["id"] == wanted).ok_or_else(|| "No window with that id.".into())
            }
            "getCurrent" | "getLastFocused" => {
                let current = self.linux_sender_tab(profile, message).map(|tab| tab["windowId"].clone());
                Ok(windows
                    .iter()
                    .find(|window| current.as_ref().is_some_and(|id| &window["id"] == id))
                    .or_else(|| windows.iter().find(|window| window["focused"] == true))
                    .or(windows.first())
                    .cloned()
                    .unwrap_or(Value::Null))
            }
            _ => unsupported(&format!("windows.{method}")),
        }
    }

    fn linux_alarms(&mut self, profile: ProfileId, extension_id: &str, method: &str, args: &[Value]) -> Result<Value, String> {
        let install = self.linux_extensions.profiles.get_mut(&profile).and_then(|state| state.install_mut(extension_id)).ok_or("The extension is not running.")?;
        match method {
            "create" => {
                let (name, info) = match args.first() {
                    Some(Value::String(name)) => (name.clone(), args.get(1).cloned().unwrap_or(Value::Null)),
                    other => (String::new(), other.cloned().unwrap_or(Value::Null)),
                };
                let minutes = |key: &str| info[key].as_f64().map(|value| Duration::from_secs_f64(value * 60.0));
                let period = minutes("periodInMinutes").map(|period| period.max(SHORTEST_ALARM));
                let delay = info["when"]
                    .as_f64()
                    .map(|when| Duration::from_millis((when - now_ms()).max(0.0) as u64))
                    .or_else(|| minutes("delayInMinutes"))
                    .or(period)
                    .unwrap_or(SHORTEST_ALARM)
                    .max(Duration::from_secs(1));
                if let Some(previous) = install.alarms.remove(&name) {
                    previous.remove();
                }
                let alarm = json!({ "name": name, "scheduledTime": now_ms() + delay.as_millis() as f64, "periodInMinutes": info["periodInMinutes"] });
                let (extension, alarm_name) = (extension_id.to_owned(), name.clone());
                let source = schedule_alarm(profile, extension, alarm_name, alarm, delay, period);
                install.alarms.insert(name, source);
                Ok(Value::Null)
            }
            "get" => Ok(install.alarms.contains_key(args.first().and_then(Value::as_str).unwrap_or_default()).then(|| json!({ "name": args.first() })).unwrap_or(Value::Null)),
            "getAll" => Ok(Value::Array(install.alarms.keys().map(|name| json!({ "name": name })).collect())),
            "clear" => {
                let name = args.first().and_then(Value::as_str).unwrap_or_default();
                Ok(json!(install.alarms.remove(name).map(|source| source.remove()).is_some()))
            }
            "clearAll" => {
                for (_, source) in install.alarms.drain() {
                    source.remove();
                }
                Ok(json!(true))
            }
            _ => unsupported(&format!("alarms.{method}")),
        }
    }

    /// An alarm went off: raise `alarms.onAlarm` in the background.
    fn linux_alarm(&mut self, profile: ProfileId, extension_id: &str, name: &str, alarm: Value, repeating: bool) {
        let Some(install) = self.linux_extensions.profiles.get_mut(&profile).and_then(|state| state.install_mut(extension_id)) else {
            return;
        };
        if !repeating {
            // The one-shot source has already ended; only forget its ID.
            install.alarms.remove(name);
        }
        if let Some(background) = &install.background {
            raise(&Endpoint { view: background.clone(), frame: 0 }, extension_id, "alarms.onAlarm", json!([alarm]));
        }
    }

    fn linux_permissions(&mut self, profile: ProfileId, extension_id: &str, method: &str, args: &[Value]) -> Result<Value, String> {
        let state = self.linux_extensions.profiles.get(&profile).ok_or("The extension is not running.")?;
        let install = state.install(extension_id).ok_or("The extension is not running.")?;
        match method {
            "getAll" => Ok(json!({ "permissions": install.permissions, "origins": install.hosts })),
            "contains" => {
                let wanted = args.first().cloned().unwrap_or(Value::Null);
                let permissions = strings(&wanted["permissions"]).iter().all(|permission| install.permissions.contains(permission));
                let origins = strings(&wanted["origins"]).iter().all(|origin| {
                    install.hosts.iter().any(|host| host == origin || host == "<all_urls>" || any_matches(std::slice::from_ref(host), origin.trim_end_matches('*')))
                });
                Ok(json!(permissions && origins))
            }
            // Runtime grants are not offered on Linux yet: asking is refused,
            // and nothing granted can be given back.
            "request" | "remove" => Ok(json!(false)),
            _ => unsupported(&format!("permissions.{method}")),
        }
    }

    fn linux_commands(&self, profile: ProfileId, extension_id: &str) -> Value {
        let Some(install) = self.linux_extensions.profiles.get(&profile).and_then(|state| state.install(extension_id)) else {
            return json!([]);
        };
        let Some(commands) = install.manifest.raw()["commands"].as_object() else {
            return json!([]);
        };
        Value::Array(
            commands
                .iter()
                .map(|(name, command)| json!({ "name": name, "description": command["description"], "shortcut": command["suggested_key"]["linux"].as_str().or(command["suggested_key"]["default"].as_str()).unwrap_or("") }))
                .collect(),
        )
    }

    // ---- Who is asking, and where things are --------------------------------

    /// Chrome's `MessageSender` for `message`.
    pub(super) fn linux_sender(&mut self, profile: ProfileId, message: &Message) -> Value {
        let mut sender = Map::new();
        sender.insert("id".into(), json!(message.extension_id));
        sender.insert("url".into(), json!(message.view.uri().map(|uri| uri.to_string())));
        sender.insert("frameId".into(), json!(message.frame_id));
        if message.kind == SenderKind::Content {
            if let Some(tab) = self.linux_sender_tab(profile, message) {
                sender.insert("tab".into(), tab);
            }
        }
        Value::Object(sender)
    }

    fn linux_sender_tab(&mut self, profile: ProfileId, message: &Message) -> Option<Value> {
        let item = self.linux_tab_of_view(profile, &message.view)?;
        let number = self.linux_extensions.profiles.get_mut(&profile)?.routing.tabs.number(item);
        self.linux_all_tabs(profile).into_iter().find(|tab| tab["id"] == number)
    }

    fn linux_tab_of_view(&self, profile: ProfileId, view: &WebView) -> Option<ItemId> {
        use wry::WebViewExtUnix;
        self.views
            .iter()
            .find(|(id, observed)| {
                self.partitions.get(id).is_some_and(|partition| partition.profile() == profile) && &observed.view.webview() == view
            })
            .map(|(id, _)| *id)
    }

    fn linux_tab_item(&self, profile: ProfileId, number: i64) -> Option<ItemId> {
        self.linux_extensions.profiles.get(&profile)?.routing.tabs.item(number)
    }

    fn linux_tab_view(&self, profile: ProfileId, number: i64) -> Option<WebView> {
        use wry::WebViewExtUnix;
        let item = self.linux_tab_item(profile, number)?;
        self.views.get(&item).map(|observed| observed.view.webview())
    }

    fn linux_focused_tab(&self, profile: ProfileId) -> Option<ItemId> {
        let surface = self.extension_browser_surfaces.get(&profile)?;
        let window = surface.windows().iter().find(|window| Some(window.id()) == surface.focused()).or(surface.windows().first())?;
        window.active()
    }

    /// Every view of the profile's tabs, for events every context gets.
    pub(super) fn linux_tab_views(&self, profile: ProfileId) -> Vec<WebView> {
        use wry::WebViewExtUnix;
        self.views
            .iter()
            .filter(|(id, _)| self.partitions.get(id).is_some_and(|partition| partition.profile() == profile))
            .map(|(_, observed)| observed.view.webview())
            .collect()
    }

    /// The extension's own pages, other than `except`: its background, its
    /// popup and any of its pages open in tabs.
    pub(super) fn linux_extension_pages(&self, profile: ProfileId, extension_id: &str, except: Option<&Endpoint>) -> Vec<Endpoint> {
        let Some(install) = self.linux_extensions.profiles.get(&profile).and_then(|state| state.install(extension_id)) else {
            return Vec::new();
        };
        let origin = install.origin();
        let tabs = self.linux_tab_views(profile).into_iter().filter(|view| view.uri().is_some_and(|uri| uri.starts_with(&origin)));
        install
            .own_views()
            .into_iter()
            .chain(tabs)
            .filter(|view| except.is_none_or(|except| &except.view != view))
            .map(|view| Endpoint { view, frame: 0 })
            .collect()
    }

    fn linux_every_context(&self, profile: ProfileId, extension_id: &str) -> Vec<Endpoint> {
        let mut endpoints = self.linux_extension_pages(profile, extension_id, None);
        for view in self.linux_tab_views(profile) {
            if !endpoints.iter().any(|endpoint| endpoint.view == view) {
                endpoints.push(Endpoint { view, frame: 0 });
            }
        }
        endpoints
    }

    fn linux_all_windows(&mut self, profile: ProfileId, populate: bool) -> Vec<Value> {
        let Some(surface) = self.extension_browser_surfaces.get(&profile).cloned() else {
            return Vec::new();
        };
        let Some(state) = self.linux_extensions.profiles.get_mut(&profile) else {
            return Vec::new();
        };
        public_windows(&surface)
            .map(|window| {
                let id = state.routing.windows.number(window.id());
                let mut value = json!({
                    "id": id, "focused": surface.focused() == Some(window.id()), "incognito": false,
                    "type": "normal", "state": "normal", "alwaysOnTop": false,
                });
                if populate {
                    value["tabs"] = Value::Array(window.tabs().iter().enumerate().map(|(index, tab)| tab_value(state, window, index, tab)).collect());
                }
                value
            })
            .collect()
    }

    pub(super) fn linux_all_tabs(&mut self, profile: ProfileId) -> Vec<Value> {
        let Some(surface) = self.extension_browser_surfaces.get(&profile).cloned() else {
            return Vec::new();
        };
        let Some(state) = self.linux_extensions.profiles.get_mut(&profile) else {
            return Vec::new();
        };
        public_windows(&surface)
            .flat_map(|window| window.tabs().iter().enumerate().map(move |(index, tab)| (window, index, tab)))
            .map(|(window, index, tab)| tab_value(state, window, index, tab))
            .collect()
    }

    fn linux_query_tabs(&mut self, profile: ProfileId, message: &Message, query: &Value) -> Vec<Value> {
        let current_window = self.linux_sender_tab(profile, message).map(|tab| tab["windowId"].clone()).or_else(|| {
            let windows = self.linux_all_windows(profile, false);
            windows.iter().find(|window| window["focused"] == true).map(|window| window["id"].clone())
        });
        let wants_current = query["currentWindow"].as_bool() == Some(true)
            || query["lastFocusedWindow"].as_bool() == Some(true)
            || query["windowId"].as_i64() == Some(-2);
        let urls = strings(&query["url"]);
        self.linux_all_tabs(profile)
            .into_iter()
            .filter(|tab| !wants_current || current_window.as_ref() == Some(&tab["windowId"]))
            .filter(|tab| query["windowId"].as_i64().is_none_or(|id| id < 0 || tab["windowId"] == id))
            .filter(|tab| ["active", "pinned", "audible", "highlighted"].iter().all(|key| query[*key].as_bool().is_none_or(|wanted| tab[*key] == wanted)))
            .filter(|tab| query["status"].as_str().is_none_or(|status| tab["status"] == status))
            .filter(|tab| urls.is_empty() || tab["url"].as_str().is_some_and(|url| any_matches(&urls, url)))
            .collect()
    }

    /// An extension-relative URL made absolute in its origin.
    fn linux_absolute(&self, profile: ProfileId, extension_id: &str, url: &str) -> String {
        if url.contains("://") || url.starts_with("about:") || url.starts_with("data:") {
            return url.to_owned();
        }
        let origin = self.linux_extensions.profiles.get(&profile).and_then(|state| state.install(extension_id)).map(super::Install::origin).unwrap_or_default();
        format!("{origin}{}", url.trim_start_matches('/'))
    }

    /// Ask the shell to open a tab.
    pub(super) fn linux_request_tab(&mut self, profile: ProfileId, url: Option<String>, active: bool) {
        let url = url.map(|url| Arc::from(url.as_str()));
        self.linux_browser_request(profile, ExtensionBrowserRequestAction::CreateTab { window: None, url, active });
    }

    /// One tab or window request to the shell, as macOS raises them.
    fn linux_browser_request(&mut self, profile: ProfileId, action: ExtensionBrowserRequestAction) {
        self.linux_extensions.next_request += 1;
        let Some(id) = ExtensionBrowserRequestId::new(LINUX_REQUESTS | self.linux_extensions.next_request) else {
            return;
        };
        if let Ok(request) = ExtensionBrowserRequest::new(profile, id, action) {
            self.sink.emit(EngineEvent::ExtensionBrowserRequested { request });
        }
    }
}

/// Request numbers the Linux runtime raises, apart from anything else's.
const LINUX_REQUESTS: u64 = 1 << 62;

/// The first run waits for `delay`; a repeating alarm then runs every
/// `period`, its new source replacing the first in the install's alarms.
fn schedule_alarm(profile: ProfileId, extension: String, name: String, alarm: Value, delay: Duration, period: Option<Duration>) -> gtk::glib::SourceId {
    gtk::glib::timeout_add_local_once(delay, move || {
        super::super::dispatch::best_effort_with(move |host| {
            host.linux_alarm(profile, &extension, &name, alarm.clone(), period.is_some());
            let Some(period) = period else {
                return;
            };
            let repeat = gtk::glib::timeout_add_local(period, {
                let (extension, name) = (extension.clone(), name.clone());
                move || {
                    let (extension, name, alarm) = (extension.clone(), name.clone(), alarm.clone());
                    super::super::dispatch::best_effort_with(move |host| host.linux_alarm(profile, &extension, &name, alarm, true));
                    gtk::glib::ControlFlow::Continue
                }
            });
            if let Some(install) = host.linux_extensions.profiles.get_mut(&profile).and_then(|state| state.install_mut(&extension)) {
                install.alarms.insert(name, repeat);
            }
        });
    })
}

fn linux_notification(extension_id: &str, method: &str, args: &[Value]) -> Result<Value, String> {
    use gtk::gio::prelude::*;
    match method {
        "create" => {
            let (id, options) = match args.first() {
                Some(Value::String(id)) => (id.clone(), args.get(1).cloned().unwrap_or(Value::Null)),
                other => (format!("{:x}", now_ms() as u64), other.cloned().unwrap_or(Value::Null)),
            };
            if let Some(application) = gtk::gio::Application::default() {
                let notification = gtk::gio::Notification::new(options["title"].as_str().unwrap_or_default());
                notification.set_body(options["message"].as_str());
                application.send_notification(Some(&format!("{extension_id}-{id}")), &notification);
            }
            Ok(json!(id))
        }
        "clear" => {
            if let (Some(application), Some(id)) = (gtk::gio::Application::default(), args.first().and_then(Value::as_str)) {
                application.withdraw_notification(&format!("{extension_id}-{id}"));
            }
            Ok(json!(true))
        }
        "update" => Ok(json!(false)),
        "getAll" => Ok(json!({})),
        _ => unsupported(&format!("notifications.{method}")),
    }
}

fn public_windows(surface: &ExtensionBrowserSurface) -> impl Iterator<Item = &ExtensionBrowserWindow> {
    surface.windows().iter().filter(|window| !window.is_private())
}

fn tab_value(state: &mut super::ProfileExtensions, window: &ExtensionBrowserWindow, index: usize, tab: &ExtensionBrowserTab) -> Value {
    let active = window.active() == Some(tab.id());
    json!({
        "id": state.routing.tabs.number(tab.id()),
        "index": index,
        "windowId": state.routing.windows.number(window.id()),
        "active": active,
        "highlighted": active,
        "selected": active,
        "pinned": tab.pinned(),
        "audible": tab.audible(),
        "discarded": tab.discarded(),
        "autoDiscardable": true,
        "mutedInfo": { "muted": tab.muted() },
        "incognito": false,
        "url": tab.url(),
        "title": tab.title().to_string(),
        "status": if tab.loading() { "loading" } else { "complete" },
        "groupId": -1,
    })
}

/// `runtime.sendMessage([extensionId], message, [options])`: the message. An
/// extension ID comes first only when another argument follows it.
fn message_argument(args: &[Value]) -> Value {
    match args {
        [Value::String(id), message, ..] if is_extension_id(id) => message.clone(),
        [message, ..] => message.clone(),
        [] => Value::Null,
    }
}

fn is_extension_id(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|byte| (b'a'..=b'p').contains(&byte))
}

/// A text file of the package, refusing paths that would leave it.
fn read_package_text(root: &std::path::Path, path: &str) -> Result<String, String> {
    let relative = std::path::Path::new(path.trim_start_matches('/'));
    if relative.components().any(|part| !matches!(part, std::path::Component::Normal(_))) {
        return Err(format!("{path} is outside the extension."));
    }
    std::fs::read_to_string(root.join(relative)).map_err(|reason| format!("{path}: {reason}"))
}

/// CSS goes into the page as a `<style>` marked with its own hash, so
/// `removeCSS` with the same text finds it again.
fn insert_css(css: &str) -> String {
    format!(
        r#"(() => {{ const style = document.createElement("style"); style.dataset.zephiumCss = "{}"; style.textContent = {}; (document.head || document.documentElement).append(style); }})()"#,
        css_mark(css),
        Value::String(css.to_owned())
    )
}

fn remove_css(css: &str) -> String {
    format!(r#"document.querySelectorAll('style[data-zephium-css="{}"]').forEach((style) => style.remove())"#, css_mark(css))
}

fn css_mark(css: &str) -> String {
    let hash = css.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3));
    format!("{hash:016x}")
}

fn strings(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(list) => list.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    }
}

fn arg_u64(args: &[Value], index: usize) -> u64 {
    args.get(index).and_then(Value::as_u64).unwrap_or(0)
}

fn arg_i64(args: &[Value], index: usize) -> Option<i64> {
    args.get(index).and_then(Value::as_i64)
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

fn unsupported(api: &str) -> Result<Value, String> {
    Err(format!("{api} is not available on Linux yet."))
}
