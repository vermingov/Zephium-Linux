//! `menus` / `contextMenus`: items extensions add to a page's context menu.
//!
//! An extension's items are kept with it; a shared table of every
//! extension's items is read when a tab's menu opens, and the matching ones
//! are appended to WebKit's own menu, under the extension's name when it
//! has more than one. A click raises `menus.onClicked` in the extension.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{json, Map, Value};
use webkit2gtk::{ContextMenu, ContextMenuExt, ContextMenuItem, HitTestResult, HitTestResultExt, WebView, WebViewExt};
use zephium_core::ids::ProfileId;
use zephium_webext_process::matching::any_matches;

use super::routing::{raise, Endpoint};

/// Chrome's limit on items per extension.
const MAX_ITEMS: usize = 1000;

#[derive(Default)]
pub(super) struct Menus {
    items: Vec<Value>,
}

/// One extension's items, as the tabs read them.
#[derive(Clone)]
pub(super) struct ExtensionMenu {
    pub(super) extension_id: String,
    pub(super) name: String,
    pub(super) items: Vec<Value>,
}

pub(super) type MenuTable = Rc<RefCell<Vec<ExtensionMenu>>>;

impl Menus {
    pub(super) fn items(&self) -> &[Value] {
        &self.items
    }

    fn call(&mut self, method: &str, args: &[Value]) -> Result<Value, String> {
        match method {
            "create" => {
                let item = args.first().cloned().filter(Value::is_object).ok_or("menus.create takes an object.")?;
                let id = item["id"].clone();
                if self.items.iter().any(|existing| existing["id"] == id) {
                    return Err(format!("Cannot create item with duplicate id {id}"));
                }
                if self.items.len() >= MAX_ITEMS {
                    return Err("Too many menu items.".into());
                }
                self.items.push(item);
                Ok(id)
            }
            "update" => {
                let id = args.first().cloned().unwrap_or(Value::Null);
                let changes = args.get(1).and_then(Value::as_object).cloned().unwrap_or_default();
                let item = self.items.iter_mut().find(|item| item["id"] == id).ok_or(format!("Cannot find menu item with id {id}"))?;
                for (key, value) in changes {
                    item[key] = value;
                }
                Ok(Value::Null)
            }
            "remove" => {
                let id = args.first().cloned().unwrap_or(Value::Null);
                let before = self.items.len();
                // An item's children go with it.
                let mut gone = vec![id.clone()];
                while let Some(parent) = gone.pop() {
                    let children: Vec<Value> = self.items.iter().filter(|item| item["parentId"] == parent).map(|item| item["id"].clone()).collect();
                    self.items.retain(|item| item["id"] != parent);
                    gone.extend(children);
                }
                if self.items.len() == before {
                    return Err(format!("Cannot find menu item with id {id}"));
                }
                Ok(Value::Null)
            }
            "removeAll" => {
                self.items.clear();
                Ok(Value::Null)
            }
            "refresh" => Ok(Value::Null),
            _ => Err(format!("menus.{method} is not available on Linux yet.")),
        }
    }
}

/// What was right-clicked, in the menus API's terms.
struct Target {
    contexts: Vec<&'static str>,
    page_url: String,
    link_url: Option<String>,
    src_url: Option<String>,
    editable: bool,
}

fn target(view: &WebView, hit: &HitTestResult) -> Target {
    let mut contexts = vec!["all"];
    if hit.context_is_link() {
        contexts.push("link");
    }
    if hit.context_is_image() {
        contexts.push("image");
    }
    if hit.context_is_media() {
        contexts.extend(["video", "audio"]);
    }
    if hit.context_is_selection() {
        contexts.push("selection");
    }
    if hit.context_is_editable() {
        contexts.push("editable");
    }
    if contexts.len() == 1 {
        contexts.push("page");
    }
    Target {
        contexts,
        page_url: view.uri().map(|uri| uri.to_string()).unwrap_or_default(),
        link_url: hit.link_uri().map(|uri| uri.to_string()),
        src_url: hit.image_uri().or_else(|| hit.media_uri()).map(|uri| uri.to_string()),
        editable: hit.context_is_editable(),
    }
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().map(|list| list.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

/// Whether `item` shows for `target`.
fn shows(item: &Value, target: &Target) -> bool {
    if item["visible"] == false {
        return false;
    }
    let contexts = strings(&item["contexts"]);
    let contexts = if contexts.is_empty() { vec!["page".to_owned()] } else { contexts };
    if !contexts.iter().any(|context| target.contexts.contains(&context.as_str())) {
        return false;
    }
    let documents = strings(&item["documentUrlPatterns"]);
    if !documents.is_empty() && !any_matches(&documents, &target.page_url) {
        return false;
    }
    let targets = strings(&item["targetUrlPatterns"]);
    let target_url = target.link_url.as_deref().or(target.src_url.as_deref());
    targets.is_empty() || target_url.is_some_and(|url| any_matches(&targets, url))
}

/// Watch `view`'s context menu for the profile's extension items.
pub(super) fn listen(view: &WebView, profile: ProfileId, table: MenuTable) {
    view.connect_context_menu(move |view, menu, _event, hit| {
        let target = target(view, hit);
        let extensions: Vec<ExtensionMenu> = table.borrow().clone();
        let mut first = true;
        for extension in extensions {
            let top: Vec<&Value> =
                extension.items.iter().filter(|item| item["parentId"].is_null() && shows(item, &target)).collect();
            if top.is_empty() {
                continue;
            }
            if first {
                menu.append(&ContextMenuItem::new_separator());
                first = false;
            }
            let entries = Entries { view, profile, extension: &extension, target: &target };
            if top.len() == 1 && !has_children(&extension.items, &top[0]["id"]) {
                menu.append(&entries.item(top[0]));
            } else {
                let submenu = ContextMenu::new();
                for item in top {
                    submenu.append(&entries.item(item));
                }
                menu.append(&ContextMenuItem::with_submenu(&extension.name, &submenu));
            }
        }
        false
    });
}

fn has_children(items: &[Value], id: &Value) -> bool {
    items.iter().any(|item| &item["parentId"] == id)
}

struct Entries<'a> {
    view: &'a WebView,
    profile: ProfileId,
    extension: &'a ExtensionMenu,
    target: &'a Target,
}

impl Entries<'_> {
    fn item(&self, item: &Value) -> ContextMenuItem {
        if item["type"] == "separator" {
            return ContextMenuItem::new_separator();
        }
        let mut title = item["title"].as_str().unwrap_or_default().replace("%s", "…");
        if item["type"] == "checkbox" || item["type"] == "radio" {
            title = format!("{} {title}", if item["checked"] == true { "✓" } else { "  " });
        }
        let children: Vec<&Value> =
            self.extension.items.iter().filter(|child| child["parentId"] == item["id"] && shows(child, self.target)).collect();
        if !children.is_empty() {
            let submenu = ContextMenu::new();
            for child in children {
                submenu.append(&self.item(child));
            }
            return ContextMenuItem::with_submenu(&title, &submenu);
        }
        let action = gtk::gio::SimpleAction::new("zephium-extension-menu", None);
        action.set_enabled(item["enabled"] != false);
        let info = self.info(item);
        let (view, profile, extension_id) = (self.view.clone(), self.profile, self.extension.extension_id.clone());
        action.connect_activate(move |_, _| {
            let (view, info, extension_id) = (view.clone(), info.clone(), extension_id.clone());
            super::super::dispatch::best_effort_with(move |host| host.linux_menu_clicked(profile, &view, &extension_id, info));
        });
        ContextMenuItem::from_gaction(&action, &title, None)
    }

    /// `menus.OnClickData` for `item`, but for the selection, read on click.
    fn info(&self, item: &Value) -> Value {
        let mut info = Map::new();
        info.insert("menuItemId".into(), item["id"].clone());
        if !item["parentId"].is_null() {
            info.insert("parentMenuItemId".into(), item["parentId"].clone());
        }
        info.insert("pageUrl".into(), json!(self.target.page_url));
        info.insert("frameId".into(), json!(0));
        info.insert("editable".into(), json!(self.target.editable));
        if let Some(link) = &self.target.link_url {
            info.insert("linkUrl".into(), json!(link));
        }
        if let Some(source) = &self.target.src_url {
            info.insert("srcUrl".into(), json!(source));
            let kind = if self.target.contexts.contains(&"image") { "image" } else { "video" };
            info.insert("mediaType".into(), json!(kind));
        }
        if item["type"] == "checkbox" {
            info.insert("wasChecked".into(), json!(item["checked"] == true));
            info.insert("checked".into(), json!(item["checked"] != true));
        }
        Value::Object(info)
    }
}

impl super::super::EngineHost {
    pub(super) fn linux_menus(&mut self, profile: ProfileId, extension_id: &str, method: &str, args: &[Value]) -> Result<Value, String> {
        let install = self
            .linux_extensions
            .profiles
            .get_mut(&profile)
            .and_then(|state| state.install_mut(extension_id))
            .ok_or("The extension is not running.")?;
        let answer = install.menus.call(method, args)?;
        self.linux_menu_table_refresh(profile);
        Ok(answer)
    }

    fn linux_menu_clicked(&mut self, profile: ProfileId, view: &WebView, extension_id: &str, mut info: Value) {
        let tab = self.linux_tab_of_view(profile, view).and_then(|item| {
            let number = self.linux_extensions.profiles.get_mut(&profile)?.routing.tabs.number(item);
            self.linux_all_tabs(profile).into_iter().find(|tab| tab["id"] == number)
        });
        // A checkbox's new state is kept, as the browser keeps it.
        if let Some(install) = self.linux_extensions.profiles.get_mut(&profile).and_then(|state| state.install_mut(extension_id)) {
            if let Some(checked) = info["checked"].as_bool() {
                let _ = install.menus.call("update", &[info["menuItemId"].clone(), json!({ "checked": checked })]);
            }
        }
        self.linux_menu_table_refresh(profile);
        let Some(views) = self.linux_extensions.profiles.get(&profile).and_then(|state| state.install(extension_id)).map(super::Install::own_views) else {
            return;
        };
        let extension_id = extension_id.to_owned();
        // The selection is the page's, read now from a world of its own.
        view.evaluate_javascript("String(getSelection())", Some("zephium-menus"), None, None::<&gtk::gio::Cancellable>, move |result| {
            let selection = result.ok().and_then(|value| super::action::js_string(&value)).unwrap_or_default();
            if !selection.is_empty() {
                info["selectionText"] = json!(selection);
            }
            for view in views {
                raise(&Endpoint { view, frame: 0 }, &extension_id, "contextMenus.onClicked", json!([info.clone(), tab.clone().unwrap_or(Value::Null)]));
            }
        });
    }

    /// Every installed extension's items, as the tabs read them.
    pub(super) fn linux_menu_table_refresh(&mut self, profile: ProfileId) {
        let Some(state) = self.linux_extensions.profiles.get(&profile) else {
            return;
        };
        *state.menus.borrow_mut() = state
            .installs
            .values()
            .filter(|install| !install.menus.items().is_empty())
            .map(|install| ExtensionMenu { extension_id: install.extension_id.clone(), name: install.loaded.name.clone(), items: install.menus.items().to_vec() })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Target {
        Target { contexts: vec!["all", "page"], page_url: "https://x.test/".into(), link_url: None, src_url: None, editable: false }
    }

    #[test]
    fn items_show_in_their_contexts_only() {
        assert!(shows(&json!({ "id": "a", "title": "A" }), &page()));
        assert!(!shows(&json!({ "id": "a", "contexts": ["link"] }), &page()));
        assert!(shows(&json!({ "id": "a", "contexts": ["all"] }), &page()));
        assert!(!shows(&json!({ "id": "a", "visible": false }), &page()));
        assert!(!shows(&json!({ "id": "a", "documentUrlPatterns": ["https://y.test/*"] }), &page()));
    }

    #[test]
    fn removing_an_item_removes_its_children() {
        let mut menus = Menus::default();
        menus.call("create", &[json!({ "id": "parent" })]).unwrap();
        menus.call("create", &[json!({ "id": "child", "parentId": "parent" })]).unwrap();
        menus.call("create", &[json!({ "id": "other" })]).unwrap();
        assert!(menus.call("create", &[json!({ "id": "other" })]).is_err());
        menus.call("remove", &[json!("parent")]).unwrap();
        assert_eq!(menus.items().len(), 1);
    }
}
