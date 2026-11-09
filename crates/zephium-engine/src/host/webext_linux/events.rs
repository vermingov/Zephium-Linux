//! Tab and window events. The shell publishes its windows and tabs as one
//! snapshot whenever they change; what differs between two snapshots is what
//! `tabs.on*` and `windows.on*` report, and `webNavigation.on*` for each
//! tab's top frame: a new address is a committed navigation, a load that
//! completes is the document's end.

use serde_json::{json, Map, Value};
use zephium_core::extensions::ExtensionBrowserSurface;

use super::api::{public_windows, tab_value, window_value};
use super::routing::{raise, Endpoint};

/// The tab fields whose changes `tabs.onUpdated` reports.
const UPDATED_FIELDS: [&str; 7] = ["status", "url", "title", "pinned", "audible", "discarded", "mutedInfo"];

/// The public windows and their tabs in Chrome's numbers and shape.
struct Snapshot {
    windows: Vec<i64>,
    tabs: Vec<Value>,
    /// `windows.WINDOW_ID_NONE` when no public window has focus.
    focused: i64,
}

fn snapshot(state: &mut super::ProfileExtensions, surface: &ExtensionBrowserSurface) -> Snapshot {
    let mut snapshot = Snapshot { windows: Vec::new(), tabs: Vec::new(), focused: -1 };
    for window in public_windows(surface) {
        let id = state.routing.windows.number(window.id());
        if surface.focused() == Some(window.id()) {
            snapshot.focused = id;
        }
        snapshot.windows.push(id);
        snapshot.tabs.extend(window.tabs().iter().enumerate().map(|(index, tab)| tab_value(state, window, index, tab)));
    }
    snapshot
}

/// The events that lead from `before` to `after`, in the order Chrome raises
/// them: windows and tabs appear, change, and go.
fn events(before: &Snapshot, after: &Snapshot) -> Vec<(&'static str, Value)> {
    let find = |tabs: &[Value], id: &Value| tabs.iter().find(|tab| tab["id"] == *id).cloned();
    let mut events = Vec::new();
    for window in after.windows.iter().filter(|window| !before.windows.contains(window)) {
        events.push(("windows.onCreated", json!([window_value(*window, after.focused == *window)])));
    }
    for tab in &after.tabs {
        let old = find(&before.tabs, &tab["id"]);
        let navigated = tab["url"].as_str().is_some_and(|url| !url.is_empty()) && old.as_ref().is_none_or(|old| old["url"] != tab["url"]);
        // Chrome's order: a tab is created, then its first navigation commits;
        // a later commit comes before the tab reports its new address.
        let commit = ("webNavigation.onCommitted", json!([navigation(tab, true)]));
        match &old {
            None => {
                events.push(("tabs.onCreated", json!([tab])));
                if navigated {
                    events.push(commit);
                }
            }
            Some(old) => {
                if navigated {
                    events.push(commit);
                }
                let changes: Map<String, Value> =
                    UPDATED_FIELDS.iter().filter(|field| old[**field] != tab[**field]).map(|field| ((*field).to_owned(), tab[*field].clone())).collect();
                if !changes.is_empty() {
                    events.push(("tabs.onUpdated", json!([tab["id"], changes, tab])));
                }
            }
        }
        let completed = tab["status"] == "complete" && old.as_ref().is_none_or(|old| old["status"] != "complete" || navigated);
        if completed && tab["url"].as_str().is_some_and(|url| !url.is_empty()) {
            events.push(("webNavigation.onDOMContentLoaded", json!([navigation(tab, false)])));
            events.push(("webNavigation.onCompleted", json!([navigation(tab, false)])));
        }
        if tab["active"] == true && old.is_none_or(|old| old["active"] != true) {
            events.push(("tabs.onActivated", json!([{ "tabId": tab["id"], "windowId": tab["windowId"] }])));
        }
    }
    for tab in &before.tabs {
        if find(&after.tabs, &tab["id"]).is_none() {
            let closing = tab["windowId"].as_i64().is_some_and(|window| !after.windows.contains(&window));
            events.push(("tabs.onRemoved", json!([tab["id"], { "windowId": tab["windowId"], "isWindowClosing": closing }])));
        }
    }
    for window in before.windows.iter().filter(|window| !after.windows.contains(window)) {
        events.push(("windows.onRemoved", json!([window])));
    }
    if before.focused != after.focused {
        events.push(("windows.onFocusChanged", json!([after.focused])));
    }
    events
}

/// A `webNavigation` event's details for `tab`'s top frame; a commit says
/// how it came about, as far as the port knows: by a link.
fn navigation(tab: &Value, commit: bool) -> Value {
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1000.0);
    let mut details = json!({
        "tabId": tab["id"],
        "url": tab["url"],
        "frameId": 0,
        "parentFrameId": -1,
        "processId": -1,
        "timeStamp": time,
        "frameType": "outermost_frame",
        "documentLifecycle": "active",
    });
    if commit {
        details["transitionType"] = json!("link");
        details["transitionQualifiers"] = json!([]);
    }
    details
}

impl super::super::EngineHost {
    /// The shell is about to replace `next.profile()`'s snapshot with `next`:
    /// tell that profile's extensions what changed. The first snapshot is the
    /// state they start from, not news.
    pub(in crate::host) fn linux_surface_changed(&mut self, next: &ExtensionBrowserSurface) {
        let profile = next.profile();
        if let (Some(focused), Some(state)) = (next.focused(), self.linux_extensions.profiles.get_mut(&profile)) {
            state.last_focused = Some(focused);
        }
        let Some(previous) = self.extension_browser_surfaces.get(&profile).cloned() else {
            return;
        };
        let Some(state) = self.linux_extensions.profiles.get_mut(&profile) else {
            return;
        };
        if state.installs.is_empty() {
            return;
        }
        let (before, after) = (snapshot(state, &previous), snapshot(state, next));
        let events = events(&before, &after);
        if events.is_empty() {
            return;
        }
        super::trace!("{} tab and window event(s)", events.len());
        for install in state.installs.values() {
            for view in install.own_views() {
                let endpoint = Endpoint { view, frame: 0 };
                for (event, args) in &events {
                    raise(&endpoint, &install.extension_id, event, args.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: i64, window: i64, active: bool, title: &str) -> Value {
        json!({ "id": id, "windowId": window, "active": active, "title": title, "status": "complete", "url": "https://x.test/" })
    }

    fn names(events: &[(&'static str, Value)]) -> Vec<&'static str> {
        events.iter().map(|(name, _)| *name).collect()
    }

    #[test]
    fn nothing_changed_is_no_event() {
        let one = || Snapshot { windows: vec![1], tabs: vec![tab(1, 1, true, "a")], focused: 1 };
        assert!(events(&one(), &one()).is_empty());
    }

    #[test]
    fn a_new_active_tab_is_created_then_activated() {
        let before = Snapshot { windows: vec![1], tabs: vec![tab(1, 1, true, "a")], focused: 1 };
        let after = Snapshot { windows: vec![1], tabs: vec![tab(1, 1, false, "a"), tab(2, 1, true, "b")], focused: 1 };
        assert_eq!(
            names(&events(&before, &after)),
            ["tabs.onCreated", "webNavigation.onCommitted", "webNavigation.onDOMContentLoaded", "webNavigation.onCompleted", "tabs.onActivated"]
        );
    }

    #[test]
    fn updates_carry_only_the_changed_fields() {
        let before = Snapshot { windows: vec![1], tabs: vec![tab(1, 1, true, "a")], focused: 1 };
        let after = Snapshot { windows: vec![1], tabs: vec![tab(1, 1, true, "b")], focused: 1 };
        let events = events(&before, &after);
        assert_eq!(names(&events), ["tabs.onUpdated"]);
        assert_eq!(events[0].1[1], json!({ "title": "b" }));
    }

    #[test]
    fn a_new_address_is_a_committed_navigation_and_a_finished_load_completes_it() {
        let loading = |url: &str| json!({ "id": 1, "windowId": 1, "active": true, "title": "a", "status": "loading", "url": url });
        let before = Snapshot { windows: vec![1], tabs: vec![tab(1, 1, true, "a")], focused: 1 };
        let moved = Snapshot { windows: vec![1], tabs: vec![loading("https://y.test/")], focused: 1 };
        assert_eq!(names(&events(&before, &moved)), ["webNavigation.onCommitted", "tabs.onUpdated"]);
        let done = Snapshot { windows: vec![1], tabs: vec![tab(1, 1, true, "a")], focused: 1 };
        let mut done_tab = done.tabs[0].clone();
        done_tab["url"] = json!("https://y.test/");
        let done = Snapshot { tabs: vec![done_tab], ..done };
        assert_eq!(names(&events(&moved, &done)), ["tabs.onUpdated", "webNavigation.onDOMContentLoaded", "webNavigation.onCompleted"]);
    }

    #[test]
    fn closing_a_window_removes_its_tabs_first() {
        let before = Snapshot { windows: vec![1, 2], tabs: vec![tab(1, 1, true, "a"), tab(2, 2, true, "b")], focused: 2 };
        let after = Snapshot { windows: vec![1], tabs: vec![tab(1, 1, true, "a")], focused: 1 };
        let events = events(&before, &after);
        assert_eq!(names(&events), ["tabs.onRemoved", "windows.onRemoved", "windows.onFocusChanged"]);
        assert_eq!(events[0].1[1]["isWindowClosing"], true);
    }
}
