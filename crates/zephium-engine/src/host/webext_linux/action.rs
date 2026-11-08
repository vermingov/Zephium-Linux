//! Toolbar actions: what each extension's button shows, and what clicking it
//! does, opening its popup or raising `action.onClicked`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

use gtk::prelude::*;
use serde_json::{json, Value};
use webkit2gtk::{HardwareAccelerationPolicy, SettingsExt, WebView, WebViewExt};
use zephium_core::extensions::{
    ExtensionActionIcon, ExtensionActionRejection, ExtensionActionRequest, ExtensionActionRevision, ExtensionActionScope,
    ExtensionActionSettlement, ExtensionActionSnapshot, ExtensionActionSnapshotSettlement, ExtensionActionState,
    ExtensionBrowserSurfaceGeneration, ExtensionRuntimeInstance,
};
use zephium_core::geometry::Size;
use zephium_core::ids::{ItemId, ProfileId};
use zephium_core::ports::engine::EngineEvent;
use zephium_webext::manifest::Manifest;

use super::routing::{raise, Endpoint};
use crate::platform::imp::extension_channel;

/// The popup's size until its page says otherwise.
const POPUP_SIZE: (i32, i32) = (380, 520);
/// Chrome's bounds for a popup sized to its page.
const POPUP_MIN: i32 = 25;
const POPUP_MAX: (i32, i32) = (800, 600);
/// The page's preferred size, as Chrome sizes a popup: its widest line,
/// whatever the window's width now, and its height at that width.
const MEASURE_POPUP: &str = r#"(() => {
  const root = document.documentElement;
  const saved = root.style.width;
  root.style.width = "max-content";
  // The root's own box: scrollHeight never drops below the window's height.
  const box = root.getBoundingClientRect();
  const size = `${Math.ceil(box.width)},${Math.ceil(box.height)}`;
  root.style.width = saved;
  return size;
})()"#;

// javascriptcore-rs is Wry's private dependency; the one string read a
// measured popup needs goes through JSC's stable C ABI.
unsafe extern "C" {
    fn jsc_value_is_string(value: *mut std::ffi::c_void) -> i32;
    fn jsc_value_to_string_as_bytes(value: *mut std::ffi::c_void) -> *mut gtk::glib::ffi::GBytes;
}
const ICON_SIZE: i32 = 32;

/// One extension's toolbar action. Each property has a default and per-tab
/// overrides, keyed by Chrome's tab number, as `action.set*({tabId})` sets them.
pub(super) struct ActionState {
    present: bool,
    title: Layered<String>,
    badge: Layered<String>,
    popup: Layered<String>,
    enabled: Layered<bool>,
    icon: Layered<Option<ExtensionActionIcon>>,
    colors: HashMap<&'static str, Value>,
}

struct Layered<T> {
    default: T,
    tabs: HashMap<i64, T>,
}

impl<T: Clone> Layered<T> {
    fn new(default: T) -> Self {
        Self { default, tabs: HashMap::new() }
    }

    fn get(&self, tab: Option<i64>) -> T {
        tab.and_then(|tab| self.tabs.get(&tab)).unwrap_or(&self.default).clone()
    }

    fn set(&mut self, tab: Option<i64>, value: T) {
        match tab {
            Some(tab) => {
                self.tabs.insert(tab, value);
            }
            None => self.default = value,
        }
    }
}

impl ActionState {
    pub(super) fn from_manifest(manifest: &Manifest, root: &Path) -> Self {
        let action = manifest.action();
        let icon = action
            .as_ref()
            .and_then(|action| action.default_icon.best(ICON_SIZE as u32).map(str::to_owned))
            .or_else(|| manifest.icons().best(ICON_SIZE as u32).map(str::to_owned))
            .and_then(|path| icon_from_file(&root.join(path.trim_start_matches('/'))));
        Self {
            present: action.is_some(),
            title: Layered::new(action.as_ref().and_then(|action| action.default_title.clone()).or_else(|| manifest.name()).unwrap_or_default()),
            badge: Layered::new(String::new()),
            popup: Layered::new(action.and_then(|action| action.default_popup).unwrap_or_default()),
            enabled: Layered::new(true),
            icon: Layered::new(icon),
            colors: HashMap::new(),
        }
    }

    pub(super) fn popup(&self, tab: Option<i64>) -> Option<String> {
        Some(self.popup.get(tab)).filter(|popup| !popup.is_empty())
    }
}

/// A package image as the 32×32 RGBA tile the toolbar draws, centred.
fn icon_from_file(path: &Path) -> Option<ExtensionActionIcon> {
    use gtk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};
    let scaled = Pixbuf::from_file_at_scale(path, ICON_SIZE, ICON_SIZE, true).ok()?;
    let tile = Pixbuf::new(Colorspace::Rgb, true, 8, ICON_SIZE, ICON_SIZE)?;
    tile.fill(0);
    let (width, height) = (scaled.width().min(ICON_SIZE), scaled.height().min(ICON_SIZE));
    let (left, top) = ((ICON_SIZE - width) / 2, (ICON_SIZE - height) / 2);
    scaled.composite(&tile, left, top, width, height, f64::from(left), f64::from(top), 1.0, 1.0, InterpType::Bilinear, 255);
    let bytes = tile.read_pixel_bytes();
    let stride = tile.rowstride() as usize;
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for row in 0..ICON_SIZE as usize {
        rgba.extend_from_slice(&bytes[row * stride..row * stride + ICON_SIZE as usize * 4]);
    }
    ExtensionActionIcon::from_rgba(rgba).ok()
}

/// An open popup: the extension's popup page in the browser window, above
/// the tab pages and under its button, as Chrome shows it. A view of its own
/// on the stage's layer, not a popover or a window: a popover took the page
/// views down, and a window is the compositor's to place and focus.
pub(super) struct Popup {
    pub(super) view: WebView,
    frame: gtk::Frame,
    parent: gtk::Fixed,
}

impl Popup {
    pub(super) fn close(self) {
        self.parent.remove(&self.frame);
        self.view.try_close();
    }
}

impl super::super::EngineHost {
    /// `action.*`, `browserAction.*` and `pageAction.*`, which the API layer
    /// sends as `action.*`.
    pub(super) fn linux_action_call(&mut self, profile: ProfileId, extension_id: &str, method: &str, args: &[Value]) -> Result<Value, String> {
        let details = args.first().cloned().unwrap_or(Value::Null);
        let tab = details["tabId"].as_i64();
        let root;
        let answer = {
            let install = self
                .linux_extensions
                .profiles
                .get_mut(&profile)
                .and_then(|state| state.install_mut(extension_id))
                .ok_or("The extension is not running.")?;
            root = install.root.clone();
            let action = &mut install.action;
            match method {
                "setTitle" => action.title.set(tab, details["title"].as_str().unwrap_or_default().to_owned()),
                "setBadgeText" => action.badge.set(tab, details["text"].as_str().unwrap_or_default().to_owned()),
                "setPopup" => action.popup.set(tab, details["popup"].as_str().unwrap_or_default().trim_start_matches('/').to_owned()),
                "enable" | "disable" => action.enabled.set(args.first().and_then(Value::as_i64), method == "enable"),
                "setIcon" => {
                    let path = match &details["path"] {
                        Value::String(path) => Some(path.clone()),
                        Value::Object(sizes) => sizes.iter().max_by_key(|(size, _)| size.parse::<u32>().unwrap_or(0)).and_then(|(_, path)| path.as_str().map(str::to_owned)),
                        _ => None,
                    };
                    let icon = path.and_then(|path| icon_from_file(&root.join(path.trim_start_matches('/'))));
                    action.icon.set(tab, icon);
                }
                "setBadgeBackgroundColor" => {
                    action.colors.insert("background", details["color"].clone());
                }
                "setBadgeTextColor" => {
                    action.colors.insert("text", details["color"].clone());
                }
                "getTitle" => return Ok(json!(action.title.get(tab))),
                "getBadgeText" => return Ok(json!(action.badge.get(tab))),
                "getPopup" => return Ok(json!(action.popup(tab).map(|popup| format!("chrome-extension://{extension_id}/{popup}")).unwrap_or_default())),
                "isEnabled" => return Ok(json!(action.enabled.get(args.first().and_then(Value::as_i64)))),
                "getBadgeBackgroundColor" => return Ok(action.colors.get("background").cloned().unwrap_or(json!([0, 0, 0, 0]))),
                "getBadgeTextColor" => return Ok(action.colors.get("text").cloned().unwrap_or(json!([255, 255, 255, 255]))),
                "getUserSettings" => return Ok(json!({ "isOnToolbar": true })),
                _ => return Err(format!("action.{method} is not available on Linux yet.")),
            }
            Value::Null
        };
        self.sink.emit(EngineEvent::ExtensionActionsInvalidated { profile });
        Ok(answer)
    }

    pub(crate) fn extension_actions_snapshot(
        &mut self,
        profile: ProfileId,
        tab: ItemId,
        generation: ExtensionBrowserSurfaceGeneration,
    ) -> ExtensionActionSnapshotSettlement {
        let result = (|| {
            let surface = self.extension_browser_surfaces.get(&profile).ok_or(ExtensionActionRejection::TabUnavailable)?;
            if surface.generation() != generation
                || !surface.tabs().any(|candidate| candidate.id() == tab)
                || self.partitions.get(&tab).is_none_or(|partition| partition.profile() != profile)
            {
                return Err(ExtensionActionRejection::TabUnavailable);
            }
            let state = self.linux_extensions.profiles.get_mut(&profile).ok_or(ExtensionActionRejection::RuntimeUnavailable)?;
            let number = state.routing.tabs.number(tab);
            let mut installs: Vec<_> = state.installs.values().filter(|install| install.action.present).collect();
            installs.sort_by_key(|install| install.id);
            let states = installs
                .into_iter()
                .filter_map(|install| {
                    let action = &install.action;
                    let (label, badge, icon) = (action.title.get(Some(number)), action.badge.get(Some(number)), action.icon.get(Some(number)));
                    let (enabled, popup) = (action.enabled.get(Some(number)), action.popup(Some(number)).is_some());
                    let revision = revision(&label, &badge, icon.as_ref(), enabled, popup)?;
                    ExtensionActionState::new(
                        ExtensionRuntimeInstance::new(profile, install.id, install.generation),
                        ExtensionActionScope::Tab(tab),
                        revision,
                        &label,
                        &badge,
                        icon,
                        enabled,
                        popup,
                        false,
                    )
                    .ok()
                })
                .collect();
            ExtensionActionSnapshot::new(profile, tab, generation, states).map_err(|_| ExtensionActionRejection::InvalidRequest)
        })();
        match result {
            Ok(snapshot) => ExtensionActionSnapshotSettlement::Applied(snapshot),
            Err(error) => ExtensionActionSnapshotSettlement::Rejected(error),
        }
    }

    pub(crate) fn invoke_extension_action(&mut self, request: ExtensionActionRequest) -> ExtensionActionSettlement {
        let runtime = request.runtime();
        let profile = runtime.profile();
        if request.is_menu() {
            // The action's context menu is not drawn on Linux yet.
            return ExtensionActionSettlement::Dispatched;
        }
        let tab_value = self.linux_all_tabs_for_action(profile, request.tab());
        let Some(state) = self.linux_extensions.profiles.get_mut(&profile) else {
            return ExtensionActionSettlement::Rejected(ExtensionActionRejection::RuntimeUnavailable);
        };
        let number = state.routing.tabs.number(request.tab());
        let (context, channel) = (state.context.clone(), state.channel.clone());
        let Some(install) = state.installs.get_mut(&runtime.install_id()) else {
            return ExtensionActionSettlement::Rejected(ExtensionActionRejection::RuntimeUnavailable);
        };
        if install.generation != runtime.generation() {
            return ExtensionActionSettlement::Rejected(ExtensionActionRejection::RuntimeSuperseded);
        }
        if !install.action.enabled.get(Some(number)) {
            return ExtensionActionSettlement::Rejected(ExtensionActionRejection::ActionDisabled);
        }
        install.active_tabs.insert(request.tab());
        match install.action.popup(Some(number)) {
            Some(popup) => {
                let (Some(context), Some(channel), Some(parent)) = (context, channel, crate::platform::imp::container()) else {
                    return ExtensionActionSettlement::Rejected(ExtensionActionRejection::PopupUnavailable);
                };
                if let Some(open) = install.popup.take() {
                    open.close();
                }
                let url = format!("{}{}", install.origin(), popup);
                let rect = request.anchor().rect();
                install.popup = Some(open_popup(&context, channel, &parent, &url, rect, profile, install.extension_id.clone()));
                ExtensionActionSettlement::PopupPresented(Size { width: f64::from(POPUP_SIZE.0), height: f64::from(POPUP_SIZE.1) })
            }
            None => {
                if let Some(background) = &install.background {
                    raise(&Endpoint { view: background.clone(), frame: 0 }, &install.extension_id, "action.onClicked", json!([tab_value]));
                }
                ExtensionActionSettlement::Dispatched
            }
        }
    }

    fn linux_all_tabs_for_action(&mut self, profile: ProfileId, tab: ItemId) -> Value {
        let number = self.linux_extensions.profiles.get_mut(&profile).map(|state| state.routing.tabs.number(tab));
        let tabs = self.linux_all_tabs(profile);
        tabs.into_iter().find(|candidate| number.is_some_and(|number| candidate["id"] == number)).unwrap_or(Value::Null)
    }

    /// The popup closed itself (focus left it, Escape, or the page closed it).
    fn linux_popup_closed(&mut self, profile: ProfileId, extension_id: &str, view: &WebView) {
        if let Some(install) = self.linux_extensions.profiles.get_mut(&profile).and_then(|state| state.install_mut(extension_id)) {
            if install.popup.as_ref().is_some_and(|popup| &popup.view == view) {
                if let Some(popup) = install.popup.take() {
                    popup.close();
                }
            }
        }
    }
}

fn open_popup(
    context: &webkit2gtk::WebContext,
    channel: std::rc::Rc<extension_channel::Channel>,
    parent: &gtk::Fixed,
    url: &str,
    anchor: zephium_core::geometry::Rect,
    profile: ProfileId,
    extension_id: String,
) -> Popup {
    let view = WebView::with_context(context);
    // Popups are small; software rendering keeps them off the GPU paths
    // that a nested view had trouble with.
    if let Some(settings) = WebViewExt::settings(&view) {
        settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Never);
        // With the trace on, the popup's console goes to the log, as the
        // background's does.
        if super::tracing() {
            settings.set_enable_write_console_messages_to_stdout(true);
        }
    }
    view.set_background_color(&gtk::gdk::RGBA::new(1.0, 1.0, 1.0, 1.0));
    extension_channel::listen(&view, channel);

    // A thin edge, so the popup reads apart from the page under it.
    let frame = gtk::Frame::new(None);
    frame.set_shadow_type(gtk::ShadowType::In);
    frame.add(&view);
    view.set_size_request(POPUP_SIZE.0, POPUP_SIZE.1);
    let (x, y) = popup_place(parent, anchor, POPUP_SIZE);
    // Added last, it draws above the tab pages.
    parent.put(&frame, x, y);

    // Chrome closes a popup when it loses focus, and on Escape. Only after
    // it had focus once: it can be told it lost focus before it was shown.
    let dismiss = {
        let (view, extension_id) = (view.clone(), extension_id);
        move || {
            let (view, extension_id) = (view.clone(), extension_id.clone());
            super::super::dispatch::best_effort_with(move |host| host.linux_popup_closed(profile, &extension_id, &view));
        }
    };
    let focused = std::rc::Rc::new(std::cell::Cell::new(false));
    let had_focus = focused.clone();
    view.connect_focus_in_event(move |_, _| {
        had_focus.set(true);
        gtk::glib::Propagation::Proceed
    });
    let on_focus_out = dismiss.clone();
    view.connect_focus_out_event(move |_, _| {
        if focused.get() {
            on_focus_out();
        }
        gtk::glib::Propagation::Proceed
    });
    let on_escape = dismiss.clone();
    view.connect_key_press_event(move |_, event| {
        if event.keyval() == gtk::gdk::keys::constants::Escape {
            on_escape();
            return gtk::glib::Propagation::Stop;
        }
        gtk::glib::Propagation::Proceed
    });
    view.connect_close(move |_| dismiss());

    // Once loaded, the popup takes the page's own size, and follows it while
    // open: a popup's page grows and shrinks as its script draws.
    // One follower per loaded page: a reload retires the one before it.
    let (sized_parent, sized_frame) = (parent.clone(), frame.clone());
    let loads = std::rc::Rc::new(std::cell::Cell::new(0_u64));
    view.connect_load_changed(move |view, event| {
        if event != webkit2gtk::LoadEvent::Finished {
            return;
        }
        let (view, parent, frame) = (view.clone(), sized_parent.clone(), sized_frame.clone());
        fit_popup(&view, parent.clone(), frame.clone(), anchor);
        let load = loads.get() + 1;
        loads.set(load);
        let current = loads.clone();
        gtk::glib::timeout_add_local(std::time::Duration::from_millis(400), move || {
            // Closed (the frame left the stage), or a newer page follows.
            if frame.parent().is_none() || current.get() != load {
                return gtk::glib::ControlFlow::Break;
            }
            fit_popup(&view, parent.clone(), frame.clone(), anchor);
            gtk::glib::ControlFlow::Continue
        });
    });

    view.load_uri(url);
    frame.show_all();
    view.grab_focus();
    Popup { view, frame, parent: parent.clone() }
}

/// Size the popup to its page, keeping it under its button and inside the
/// window.
fn fit_popup(view: &WebView, parent: gtk::Fixed, frame: gtk::Frame, anchor: zephium_core::geometry::Rect) {
    let sized = view.clone();
    view.evaluate_javascript(MEASURE_POPUP, None, None, None::<&gtk::gio::Cancellable>, move |result| {
        if let Some(size) = result.ok().and_then(|value| js_string(&value)).as_deref().and_then(popup_size) {
            if sized.size_request() == size {
                return;
            }
            sized.set_size_request(size.0, size.1);
            let (x, y) = popup_place(&parent, anchor, size);
            parent.move_(&frame, x, y);
        }
    });
}

/// Where a popup of `size` goes: under its button, moved in as far as it
/// takes to stay inside the window.
fn popup_place(parent: &gtk::Fixed, anchor: zephium_core::geometry::Rect, size: (i32, i32)) -> (i32, i32) {
    const MARGIN: i32 = 4;
    let room = parent.allocation();
    let fit = |start: i32, length: i32, available: i32| start.min(available - length - MARGIN).max(0);
    (
        fit(anchor.x as i32, size.0, room.width()),
        fit((anchor.y + anchor.height) as i32 + MARGIN, size.1, room.height()),
    )
}

/// `"width,height"` as the popup's size within Chrome's bounds.
fn popup_size(measured: &str) -> Option<(i32, i32)> {
    let (width, height) = measured.split_once(',')?;
    let (width, height): (i32, i32) = (width.trim().parse().ok()?, height.trim().parse().ok()?);
    Some((width.clamp(POPUP_MIN, POPUP_MAX.0), height.clamp(POPUP_MIN, POPUP_MAX.1)))
}

/// A JavaScript string result as Rust text.
pub(super) fn js_string(value: &impl gtk::glib::ObjectType) -> Option<String> {
    let value = value.as_ptr().cast::<std::ffi::c_void>();
    // SAFETY: a live JSCValue borrowed for this call.
    if unsafe { jsc_value_is_string(value) } == 0 {
        return None;
    }
    // SAFETY: as above; the call hands over one GBytes reference, which
    // `from_glib_full` takes.
    let raw = unsafe { jsc_value_to_string_as_bytes(value) };
    if raw.is_null() {
        return None;
    }
    let bytes: gtk::glib::Bytes = unsafe { gtk::glib::translate::from_glib_full(raw) };
    std::str::from_utf8(&bytes).ok().map(str::to_owned)
}

/// A revision that changes exactly when what the button shows changes.
fn revision(label: &str, badge: &str, icon: Option<&ExtensionActionIcon>, enabled: bool, popup: bool) -> Option<ExtensionActionRevision> {
    let mut hasher = DefaultHasher::new();
    (label, badge, enabled, popup).hash(&mut hasher);
    icon.map(|icon| format!("{icon:?}")).hash(&mut hasher);
    ExtensionActionRevision::new(hasher.finish() | 1)
}

#[cfg(test)]
mod tests {
    use super::popup_size;

    #[test]
    fn popups_take_their_page_size_within_chromes_bounds() {
        assert_eq!(popup_size("320,240"), Some((320, 240)));
        assert_eq!(popup_size("4000,3"), Some((800, 25)));
        assert_eq!(popup_size("wide"), None);
    }
}
