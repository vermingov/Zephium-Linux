//! The enabled extensions, kept current in processes started before they
//! changed. WebKit starts spare web processes ahead of need, with the startup
//! data of that moment, so the plugin also takes the list later: pushed by the
//! browser when it changes, asked for when a page is created, and asked for
//! when one of an extension's own pages arrives that the process does not
//! know. That page is reloaded once, in this process, when the answer says the
//! extension is enabled.

use std::ffi::{c_void, CStr, CString};

use glib::gobject_ffi::g_object_unref;
use glib::translate::{FromGlibPtrNone, ToGlibPtr};
use glib::ToVariant;

use crate::ffi::*;
use crate::plugin::{configure, is_extension_id, PLUGIN};
use crate::{CONFIG_MESSAGE, CONFIG_REQUEST, EXTENSION_SCHEMES};

/// The browser pushed the current list: take it, and say how many arrived.
pub(crate) unsafe fn apply(message: *mut WebKitUserMessage) {
    let parameters = webkit_user_message_get_parameters(message);
    let applied = !parameters.is_null() && configure(&glib::Variant::from_glib_none(parameters));
    let count = PLUGIN.with(|plugin| plugin.borrow().as_ref().map_or(0, |plugin| plugin.extensions.len()));
    let name = CString::new(CONFIG_MESSAGE).expect("constant has no NUL");
    let reply = webkit_user_message_new(name.as_ptr(), (applied, count as u32).to_variant().to_glib_none().0);
    webkit_user_message_send_reply(message, reply);
}

/// Ask the browser for the current list. With `heal`, reload the page if its
/// main frame is an extension page this process now knows.
pub(crate) unsafe fn request(page: *mut WebKitWebPage, heal: bool) {
    let name = CString::new(CONFIG_REQUEST).expect("constant has no NUL");
    let message = webkit_user_message_new(name.as_ptr(), std::ptr::null_mut());
    let heal = Box::into_raw(Box::new(heal));
    webkit_web_page_send_message_to_view(page, message, std::ptr::null_mut(), Some(answered), heal.cast());
}

unsafe extern "C" fn answered(page: *mut c_void, result: *mut GAsyncResult, data: *mut c_void) {
    let heal = *Box::from_raw(data.cast::<bool>());
    let answer = webkit_web_page_send_message_to_view_finish(page, result, std::ptr::null_mut());
    if answer.is_null() {
        return;
    }
    let parameters = webkit_user_message_get_parameters(answer);
    let applied = !parameters.is_null() && configure(&glib::Variant::from_glib_none(parameters));
    g_object_unref(answer.cast());
    if applied && heal {
        reload_if_known(page);
    }
}

unsafe fn reload_if_known(page: *mut WebKitWebPage) {
    let frame = webkit_web_page_get_main_frame(page);
    let uri = webkit_frame_get_uri(frame);
    if uri.is_null() {
        return;
    }
    let Ok(uri) = CStr::from_ptr(uri).to_str() else {
        return;
    };
    let Some(rest) = EXTENSION_SCHEMES.iter().find_map(|scheme| uri.strip_prefix(scheme)) else {
        return;
    };
    let host = rest.split('/').next().unwrap_or_default();
    let known = is_extension_id(host)
        && PLUGIN.with(|plugin| plugin.borrow().as_ref().is_some_and(|plugin| plugin.extensions.contains_key(host)));
    if !known {
        return;
    }
    let context = webkit_frame_get_js_context_for_script_world(frame, webkit_script_world_get_default());
    if context.is_null() {
        return;
    }
    let code = "location.reload()";
    let result = jsc_context_evaluate(context, code.as_ptr().cast(), code.len() as isize);
    if !result.is_null() {
        g_object_unref(result.cast());
    }
    g_object_unref(context.cast());
}
