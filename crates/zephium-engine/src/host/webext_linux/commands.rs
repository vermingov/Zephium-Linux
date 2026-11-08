//! Extensions' keyboard commands (`commands` in the manifest): a key press
//! in a tab that matches one raises `commands.onCommand` in the extension.
//!
//! The table is read on every key press, so it is kept ready: rebuilt when
//! the installed extensions change, shared with each tab's key handler.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gdk::ModifierType;
use gtk::glib::translate::IntoGlib;
use gtk::prelude::WidgetExt;
use serde_json::{json, Value};
use webkit2gtk::WebView;
use zephium_core::ids::ProfileId;
use zephium_webext::manifest::Manifest;

use super::routing::{raise, Endpoint};

/// Modifiers a shortcut may name; others (Num Lock…) are ignored.
const MODIFIERS: ModifierType = ModifierType::CONTROL_MASK.union(ModifierType::MOD1_MASK).union(ModifierType::SHIFT_MASK);

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Shortcut {
    pub(super) extension_id: String,
    pub(super) command: String,
    modifiers: ModifierType,
    key: u32,
}

/// The profile's shortcuts, shared with the tabs' key handlers.
pub(super) type Shortcuts = Rc<RefCell<Vec<Shortcut>>>;

/// `"Alt+Shift+U"` as modifiers and a lower-case key value. `None` for what
/// Linux cannot press (`MacCtrl`, `Command`) or does not know.
fn parse(text: &str) -> Option<(ModifierType, u32)> {
    let mut modifiers = ModifierType::empty();
    let mut key = None;
    for part in text.split('+').map(str::trim) {
        match part {
            "Ctrl" => modifiers |= ModifierType::CONTROL_MASK,
            "Alt" => modifiers |= ModifierType::MOD1_MASK,
            "Shift" => modifiers |= ModifierType::SHIFT_MASK,
            "MacCtrl" | "Command" | "Search" => return None,
            name => key = Some(key_value(name)?),
        }
    }
    Some((modifiers, key?))
}

fn key_value(name: &str) -> Option<u32> {
    let gdk_name = match name {
        "Comma" => "comma",
        "Period" => "period",
        "Space" => "space",
        "Insert" => "Insert",
        "Delete" => "Delete",
        "Home" => "Home",
        "End" => "End",
        "PageUp" => "Page_Up",
        "PageDown" => "Page_Down",
        "Up" | "Down" | "Left" | "Right" => name,
        "MediaNextTrack" => "AudioNext",
        "MediaPrevTrack" => "AudioPrev",
        "MediaPlayPause" => "AudioPlay",
        "MediaStop" => "AudioStop",
        single if single.chars().count() == 1 => return single.chars().next().filter(char::is_ascii_alphanumeric).map(|key| key.to_ascii_lowercase() as u32),
        function if function.starts_with('F') && function[1..].parse::<u8>().is_ok_and(|number| (1..=12).contains(&number)) => function,
        _ => return None,
    };
    Some(gtk::gdk::keys::Key::from_name(gdk_name).into_glib())
}

/// The manifest's commands that have a key on Linux.
pub(super) fn shortcuts(extension_id: &str, manifest: &Manifest) -> Vec<Shortcut> {
    let Some(commands) = manifest.raw()["commands"].as_object() else {
        return Vec::new();
    };
    commands
        .iter()
        .filter_map(|(command, details)| {
            let key = details["suggested_key"]["linux"].as_str().or(details["suggested_key"]["default"].as_str())?;
            let (modifiers, key) = parse(key)?;
            Some(Shortcut { extension_id: extension_id.to_owned(), command: command.clone(), modifiers, key })
        })
        .collect()
}

/// Watch `view`'s key presses for the profile's shortcuts.
pub(super) fn listen(view: &WebView, profile: ProfileId, shortcuts: Shortcuts) {
    view.connect_key_press_event(move |view, event| {
        let modifiers = event.state() & MODIFIERS;
        let key = event.keyval().to_lower().into_glib();
        let hit = shortcuts.borrow().iter().find(|shortcut| shortcut.modifiers == modifiers && shortcut.key == key).cloned();
        let Some(hit) = hit else {
            return gtk::glib::Propagation::Proceed;
        };
        let view = view.clone();
        super::super::dispatch::best_effort_with(move |host| host.linux_command(profile, &view, hit));
        gtk::glib::Propagation::Stop
    });
}

impl super::super::EngineHost {
    fn linux_command(&mut self, profile: ProfileId, view: &WebView, shortcut: Shortcut) {
        let tab = self.linux_tab_of_view(profile, view).and_then(|item| {
            let number = self.linux_extensions.profiles.get_mut(&profile)?.routing.tabs.number(item);
            self.linux_all_tabs(profile).into_iter().find(|tab| tab["id"] == number)
        });
        let Some(install) = self.linux_extensions.profiles.get(&profile).and_then(|state| state.install(&shortcut.extension_id)) else {
            return;
        };
        super::trace!("{} command {}", shortcut.extension_id, shortcut.command);
        for view in install.own_views() {
            raise(&Endpoint { view, frame: 0 }, &shortcut.extension_id, "commands.onCommand", json!([shortcut.command, tab.clone().unwrap_or(Value::Null)]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_keys_read_as_firefox_and_chrome_write_them() {
        let (modifiers, key) = parse("Alt+Shift+U").unwrap();
        assert_eq!(modifiers, ModifierType::MOD1_MASK | ModifierType::SHIFT_MASK);
        assert_eq!(key, 'u' as u32);
        assert_eq!(parse("Ctrl+Comma").map(|(_, key)| key), Some(gtk::gdk::keys::constants::comma.into_glib()));
        assert!(parse("Ctrl+F5").is_some());
        assert!(parse("MacCtrl+Shift+Y").is_none());
        assert!(parse("Ctrl+Shift+F13").is_none());
    }
}
