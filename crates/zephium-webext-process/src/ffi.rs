//! The WebKitGTK 4.1 web-process API and the JavaScriptCore GLib API this
//! plugin calls, declared from /usr/include/webkitgtk-4.1. WebKit objects are
//! opaque here; only their pointers cross this boundary.

use std::ffi::{c_char, c_int, c_uint, c_void};

use glib::ffi::{gboolean, gpointer, GPtrArray, GVariant};
use glib::gobject_ffi::GCallback;

pub type WebKitWebExtension = c_void;
pub type WebKitWebPage = c_void;
pub type WebKitFrame = c_void;
pub type WebKitScriptWorld = c_void;
pub type WebKitUserMessage = c_void;
pub type JSCContext = c_void;
pub type JSCValue = c_void;
pub type JSCException = c_void;
pub type GAsyncResult = c_void;

pub type GAsyncReadyCallback = Option<unsafe extern "C" fn(*mut c_void, *mut GAsyncResult, gpointer)>;

pub const JSC_VALUE_PROPERTY_NONE: c_uint = 0;

#[link(name = "webkit2gtk-4.1")]
unsafe extern "C" {
    pub fn webkit_web_extension_get_page(extension: *mut WebKitWebExtension, page_id: u64) -> *mut WebKitWebPage;

    pub fn webkit_script_world_get_default() -> *mut WebKitScriptWorld;
    pub fn webkit_script_world_new_with_name(name: *const c_char) -> *mut WebKitScriptWorld;

    pub fn webkit_frame_get_id(frame: *mut WebKitFrame) -> u64;
    pub fn webkit_frame_is_main_frame(frame: *mut WebKitFrame) -> gboolean;
    pub fn webkit_frame_get_uri(frame: *mut WebKitFrame) -> *const c_char;
    pub fn webkit_frame_get_js_context_for_script_world(
        frame: *mut WebKitFrame,
        world: *mut WebKitScriptWorld,
    ) -> *mut JSCContext;

    pub fn webkit_web_page_get_id(page: *mut WebKitWebPage) -> u64;
    pub fn webkit_web_page_get_main_frame(page: *mut WebKitWebPage) -> *mut WebKitFrame;
    pub fn webkit_web_page_send_message_to_view(
        page: *mut WebKitWebPage,
        message: *mut WebKitUserMessage,
        cancellable: *mut c_void,
        callback: GAsyncReadyCallback,
        user_data: gpointer,
    );
    pub fn webkit_web_page_send_message_to_view_finish(
        page: *mut WebKitWebPage,
        result: *mut GAsyncResult,
        error: *mut *mut glib::ffi::GError,
    ) -> *mut WebKitUserMessage;

    pub fn webkit_user_message_new(name: *const c_char, parameters: *mut GVariant) -> *mut WebKitUserMessage;
    pub fn webkit_user_message_get_parameters(message: *mut WebKitUserMessage) -> *mut GVariant;
    pub fn webkit_user_message_get_name(message: *mut WebKitUserMessage) -> *const c_char;
    pub fn webkit_user_message_send_reply(message: *mut WebKitUserMessage, reply: *mut WebKitUserMessage);
}

#[link(name = "javascriptcoregtk-4.1")]
unsafe extern "C" {
    pub fn jsc_context_get_global_object(context: *mut JSCContext) -> *mut JSCValue;
    pub fn jsc_context_throw(context: *mut JSCContext, error_message: *const c_char);
    pub fn jsc_context_evaluate(context: *mut JSCContext, code: *const c_char, length: isize) -> *mut JSCValue;
    pub fn jsc_context_get_exception(context: *mut JSCContext) -> *mut JSCException;
    pub fn jsc_context_clear_exception(context: *mut JSCContext);
    pub fn jsc_exception_get_message(exception: *mut JSCException) -> *const c_char;
    pub fn jsc_value_is_undefined(value: *mut JSCValue) -> gboolean;
    pub fn jsc_value_to_json(value: *mut JSCValue, indent: c_uint) -> *mut c_char;
    pub fn jsc_value_object_has_property(value: *mut JSCValue, name: *const c_char) -> gboolean;

    pub fn jsc_value_get_context(value: *mut JSCValue) -> *mut JSCContext;
    pub fn jsc_value_is_string(value: *mut JSCValue) -> gboolean;
    pub fn jsc_value_is_function(value: *mut JSCValue) -> gboolean;
    pub fn jsc_value_to_string(value: *mut JSCValue) -> *mut c_char;
    pub fn jsc_value_new_string(context: *mut JSCContext, string: *const c_char) -> *mut JSCValue;
    pub fn jsc_value_new_function_variadic(
        context: *mut JSCContext,
        name: *const c_char,
        callback: GCallback,
        user_data: gpointer,
        destroy_notify: glib::ffi::GDestroyNotify,
        return_type: glib::ffi::GType,
    ) -> *mut JSCValue;
    pub fn jsc_value_function_callv(
        value: *mut JSCValue,
        n_parameters: c_uint,
        parameters: *mut *mut JSCValue,
    ) -> *mut JSCValue;
    pub fn jsc_value_object_define_property_data(
        value: *mut JSCValue,
        property_name: *const c_char,
        flags: c_uint,
        property_value: *mut JSCValue,
    );
}

/// The arguments a variadic JavaScriptCore function receives.
pub unsafe fn arguments(args: *mut GPtrArray) -> &'static [*mut JSCValue] {
    if args.is_null() || (*args).len == 0 {
        return &[];
    }
    std::slice::from_raw_parts((*args).pdata.cast::<*mut JSCValue>(), (*args).len as usize)
}

pub fn ok(value: c_int) -> bool {
    value != 0
}
