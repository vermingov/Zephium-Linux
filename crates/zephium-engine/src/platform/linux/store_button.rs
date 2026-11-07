//! The Chrome Web Store's in-page "Add to Zephium" button on WebKitGTK.
//!
//! As on macOS, its script runs in a script world of its own: the page can
//! neither reach the button nor post to the handler, which exists only in
//! that world. WebKitGTK does not name the frame of a message, so the script
//! is injected into the store's top frame only, and a click counts only when
//! the view's own address is the listing it names.

use std::ffi::c_void;

use gtk::glib::translate::FromGlibPtrFull;
use gtk::glib::prelude::ObjectExt;
use gtk::glib::ObjectType;
use webkit2gtk::{
    JavascriptResult, UserContentInjectedFrames, UserContentManager, UserContentManagerExt, UserScript,
    UserScriptInjectionTime, WebView, WebViewExt,
};
use zephium_webext::{store, ExtensionId};

const WORLD: &str = "zephium-store";
const HANDLER: &str = "zephiumStore";
const STORE_PAGES: &str = "https://chromewebstore.google.com/*";
const MAX_BODY_UTF16: usize = 64;

/// One step of the button, when `ZEPHIUM_WEBEXT_TRACE` is set.
fn trace(step: std::fmt::Arguments<'_>) {
    if std::env::var_os("ZEPHIUM_WEBEXT_TRACE").is_some() {
        eprintln!("store button: {step}");
    }
}
/// A body this long in bytes is refused before it is measured in UTF-16.
const MAX_BODY_BYTES: usize = 4 * MAX_BODY_UTF16;

// As in the platform module: javascriptcore-rs is Wry's private dependency,
// so the two string reads go through JSC's stable C ABI.
unsafe extern "C" {
    fn jsc_value_is_string(value: *mut c_void) -> i32;
    fn jsc_value_to_string_as_bytes(value: *mut c_void) -> *mut gtk::glib::ffi::GBytes;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StoreMessage {
    /// A new document asks for the button's state.
    Ready,
    /// A click on the button, for the listing the view shows.
    Clicked(ExtensionId),
}

/// The world and handler of one view; dropping it removes the handler and
/// the script.
pub(crate) struct StoreButton {
    manager: UserContentManager,
    view: gtk::glib::WeakRef<WebView>,
    script: UserScript,
    signal: Option<gtk::glib::SignalHandlerId>,
}

impl StoreButton {
    /// Runs the store's page script in `view`'s store world and routes what
    /// its button reports to `on_message`.
    pub(crate) fn install(view: &WebView, on_message: impl Fn(StoreMessage) + 'static) -> Option<Self> {
        let manager = view.user_content_manager()?;
        let weak = gtk::glib::WeakRef::new();
        weak.set(Some(view));
        let page = weak.clone();
        let signal = manager.connect_script_message_received(Some(HANDLER), move |_, result| {
            let Some(view) = page.upgrade() else {
                return;
            };
            let message = body(result).and_then(|body| admit(&view, &body));
            trace(format_args!("message {message:?}"));
            if let Some(message) = message {
                on_message(message);
            }
        });
        if !manager.register_script_message_handler_in_world(HANDLER, WORLD) {
            trace(format_args!("the handler could not be registered"));
            manager.disconnect(signal);
            return None;
        }
        let script = UserScript::for_world(
            store::PAGE_SCRIPT,
            UserContentInjectedFrames::TopFrame,
            UserScriptInjectionTime::Start,
            WORLD,
            &[STORE_PAGES],
            &[],
        );
        manager.add_script(&script);
        Some(Self { manager, view: weak, script, signal: Some(signal) })
    }

    /// Shows `state`, a JSON object the script's `show` accepts, in the
    /// view's current document.
    pub(crate) fn show(&self, state: &str) {
        let Some(view) = self.view.upgrade() else {
            return;
        };
        let script = format!("window.__zephium_store_v1__?.show({state})");
        trace(format_args!("show {state}"));
        view.evaluate_javascript(&script, Some(WORLD), None, None::<&gtk::gio::Cancellable>, |result| {
            if let Err(error) = result {
                trace(format_args!("show failed: {error}"));
            }
        });
    }
}

/// The message's text, if it is a short string.
fn body(result: &JavascriptResult) -> Option<String> {
    let value = result.js_value()?;
    let value = value.as_ptr().cast::<c_void>();
    // SAFETY: a live JSCValue held by `result` for this call.
    if unsafe { jsc_value_is_string(value) } == 0 {
        return None;
    }
    // SAFETY: as above; the call transfers one owned GBytes reference, which
    // `from_glib_full` takes.
    let raw = unsafe { jsc_value_to_string_as_bytes(value) };
    if raw.is_null() {
        return None;
    }
    let bytes = unsafe { gtk::glib::Bytes::from_glib_full(raw) };
    if bytes.len() > MAX_BODY_BYTES {
        return None;
    }
    std::str::from_utf8(&bytes).ok().map(str::to_owned)
}

/// What a message from the store world means, when it is one the button
/// sends from the store page the view shows.
fn admit(view: &WebView, body: &str) -> Option<StoreMessage> {
    if body.encode_utf16().count() > MAX_BODY_UTF16 {
        return None;
    }
    let address = view.uri()?.to_string();
    if !address.starts_with("https://chromewebstore.google.com/") {
        return None;
    }
    if body == "ready" {
        return Some(StoreMessage::Ready);
    }
    store::listing_id(&address).filter(|id| id.as_str() == body).map(StoreMessage::Clicked)
}

impl Drop for StoreButton {
    fn drop(&mut self) {
        self.manager.remove_script(&self.script);
        self.manager.unregister_script_message_handler_in_world(HANDLER, WORLD);
        if let Some(signal) = self.signal.take() {
            self.manager.disconnect(signal);
        }
    }
}
