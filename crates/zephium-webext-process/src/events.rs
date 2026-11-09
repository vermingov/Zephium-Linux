//! Events the browser raises in extension worlds.
//!
//! Each world registers one listener (`__zephiumWebextListen`). The browser
//! sends a page the `zephium-webext.event` message naming the extension and
//! the frame (0 for all of the extension's worlds in the page); the plugin
//! calls the matching listeners with the event's text and replies with how
//! many it called, `(u)`, so the browser knows when nobody is listening.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_void, CStr};

use glib::ffi::gboolean;
use glib::gobject_ffi::{g_object_ref, g_object_unref, g_signal_connect_data};
use glib::translate::{FromGlibPtrNone, ToGlibPtr};
use glib::ToVariant;

use crate::channel;
use crate::ffi::*;
use crate::{config, requests, run, CONFIG_MESSAGE, EVENT_MESSAGE, RUN_MESSAGE};

/// The world a listener belongs to.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Key {
    pub(crate) page_id: u64,
    pub(crate) frame_id: u64,
    pub(crate) extension_id: String,
}

/// A listener function, kept alive while registered.
struct Listener(*mut JSCValue);

impl Drop for Listener {
    fn drop(&mut self) {
        unsafe { g_object_unref(self.0.cast()) };
    }
}

thread_local! {
    static LISTENERS: RefCell<HashMap<Key, Listener>> = RefCell::new(HashMap::new());
}

pub(crate) unsafe fn register(key: Key, function: *mut JSCValue) {
    g_object_ref(function.cast());
    // The replaced listener, if any, is released after the borrow ends.
    let replaced = LISTENERS.with(|listeners| listeners.borrow_mut().insert(key, Listener(function)));
    drop(replaced);
}

/// A frame got a new document: its old worlds' listeners are gone.
pub(crate) fn forget_frame(page_id: u64, frame_id: u64) {
    let forgotten: Vec<Listener> = LISTENERS.with(|listeners| {
        let mut listeners = listeners.borrow_mut();
        let keys: Vec<Key> =
            listeners.keys().filter(|key| key.page_id == page_id && key.frame_id == frame_id).cloned().collect();
        keys.into_iter().filter_map(|key| listeners.remove(&key)).collect()
    });
    drop(forgotten);
}

pub(crate) unsafe fn watch_pages(extension: *mut WebKitWebExtension) {
    g_signal_connect_data(
        extension.cast(),
        c"page-created".as_ptr(),
        Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut WebKitWebExtension, *mut WebKitWebPage, *mut c_void),
            unsafe extern "C" fn(),
        >(page_created)),
        std::ptr::null_mut(),
        None,
        0,
    );
}

unsafe extern "C" fn page_created(_extension: *mut WebKitWebExtension, page: *mut WebKitWebPage, _data: *mut c_void) {
    // The process may have started before the enabled extensions changed.
    config::request(page, false);
    requests::watch(page);
    g_signal_connect_data(
        page.cast(),
        c"user-message-received".as_ptr(),
        Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut WebKitWebPage, *mut WebKitUserMessage, *mut c_void) -> gboolean,
            unsafe extern "C" fn(),
        >(message_received)),
        std::ptr::null_mut(),
        None,
        0,
    );
}

unsafe extern "C" fn message_received(
    page: *mut WebKitWebPage,
    message: *mut WebKitUserMessage,
    _data: *mut c_void,
) -> gboolean {
    let name = webkit_user_message_get_name(message);
    let name = if name.is_null() { None } else { CStr::from_ptr(name).to_str().ok() };
    if name == Some(RUN_MESSAGE) {
        run::run(page, message);
        return 1;
    }
    if name == Some(CONFIG_MESSAGE) {
        config::apply(message);
        return 1;
    }
    if name != Some(EVENT_MESSAGE) {
        return 0;
    }
    let parameters = webkit_user_message_get_parameters(message);
    let event = (!parameters.is_null())
        .then(|| glib::Variant::from_glib_none(parameters))
        .and_then(|parameters| parameters.get::<(String, u64, String)>());
    let Some((extension_id, frame_id, body)) = event else {
        reply_count(message, 0);
        return 1;
    };
    let page_id = webkit_web_page_get_id(page);
    // Taken out with a reference each: a listener may register or forget
    // listeners while it runs.
    let targets: Vec<Listener> = LISTENERS.with(|listeners| {
        listeners
            .borrow()
            .iter()
            .filter(|(key, _)| {
                key.page_id == page_id && key.extension_id == extension_id && (frame_id == 0 || key.frame_id == frame_id)
            })
            .map(|(_, listener)| {
                g_object_ref(listener.0.cast());
                Listener(listener.0)
            })
            .collect()
    });
    crate::trace(format_args!(
        "event for {extension_id} in page {page_id} frame {frame_id}: {} listener(s) of {} known",
        targets.len(),
        LISTENERS.with(|listeners| listeners.borrow().len())
    ));
    for listener in &targets {
        channel::call_with(listener.0, Some(&body));
    }
    reply_count(message, targets.len() as u32);
    1
}

unsafe fn reply_count(message: *mut WebKitUserMessage, count: u32) {
    let name = std::ffi::CString::new(EVENT_MESSAGE).expect("constant has no NUL");
    let parameters = (count,).to_variant();
    let reply = webkit_user_message_new(name.as_ptr(), parameters.to_glib_none().0);
    webkit_user_message_send_reply(message, reply);
}
