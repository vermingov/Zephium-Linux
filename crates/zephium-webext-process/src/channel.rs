//! The one native function an extension world gets: it sends a bounded
//! string to the browser and hands the reply to a callback.
//!
//! The size is checked here, in the web process, so the browser never
//! receives more than the limit. Measuring copies a string the caller already
//! allocated in this same sandboxed process; nothing larger crosses into the
//! browser.

use std::ffi::{c_char, c_void, CStr, CString};

use glib::gobject_ffi::{g_object_ref, g_object_unref};
use glib::translate::{FromGlibPtrNone, ToGlibPtr};
use glib::ToVariant;

use crate::ffi::*;
use crate::plugin::PLUGIN;
use crate::{SEND_FUNCTION, SEND_MESSAGE};

/// Who is sending, fixed by the plugin when it installs the function.
pub(crate) struct Sender {
    extension_id: String,
    kind: &'static str,
    page_id: u64,
    frame_id: u64,
}

impl Sender {
    pub(crate) fn new(extension_id: &str, kind: &'static str, page_id: u64, frame_id: u64) -> Self {
        Self { extension_id: extension_id.to_owned(), kind, page_id, frame_id }
    }
}

/// Define the send function on `context`'s global object, read-only and
/// non-configurable, so code in the world can call it but not replace it.
pub(crate) unsafe fn install(context: *mut JSCContext, sender: Sender) {
    let name = CString::new(SEND_FUNCTION).expect("constant has no NUL");
    let function = jsc_value_new_function_variadic(
        context,
        name.as_ptr(),
        Some(std::mem::transmute::<unsafe extern "C" fn(*mut glib::ffi::GPtrArray, *mut c_void), unsafe extern "C" fn()>(
            send,
        )),
        Box::into_raw(Box::new(sender)).cast(),
        Some(drop_sender),
        glib::gobject_ffi::G_TYPE_NONE,
    );
    let global = jsc_context_get_global_object(context);
    jsc_value_object_define_property_data(global, name.as_ptr(), JSC_VALUE_PROPERTY_NONE, function);
    g_object_unref(global.cast());
    g_object_unref(function.cast());
}

unsafe extern "C" fn drop_sender(data: *mut c_void) {
    drop(Box::from_raw(data.cast::<Sender>()));
}

/// `send(message: string, reply: (answer?: string) => void)`.
unsafe extern "C" fn send(args: *mut glib::ffi::GPtrArray, data: *mut c_void) {
    let sender = &*data.cast::<Sender>();
    let [message, reply] = arguments(args) else {
        return;
    };
    let context = jsc_value_get_context(*message);
    if !ok(jsc_value_is_string(*message)) || !ok(jsc_value_is_function(*reply)) {
        throw(context, c"send(message, reply) takes a string and a function");
        return;
    }
    let Some(body) = owned_string(jsc_value_to_string(*message)) else {
        throw(context, c"the message is not valid UTF-8");
        return;
    };
    let (limit, extension) = PLUGIN.with(|plugin| {
        plugin.borrow().as_ref().map_or((0, std::ptr::null_mut()), |plugin| (plugin.message_limit, plugin.extension))
    });
    if body.len() > limit {
        throw(context, c"the message is larger than an extension may send");
        return;
    }
    let page = webkit_web_extension_get_page(extension, sender.page_id);
    if page.is_null() {
        return;
    }
    let parameters = (sender.extension_id.as_str(), sender.kind, sender.frame_id, body.as_str()).to_variant();
    let name = CString::new(SEND_MESSAGE).expect("constant has no NUL");
    let message = webkit_user_message_new(name.as_ptr(), parameters.to_glib_none().0);
    g_object_ref(reply.cast());
    webkit_web_page_send_message_to_view(page, message, std::ptr::null_mut(), Some(replied), reply.cast());
}

unsafe extern "C" fn replied(page: *mut c_void, result: *mut GAsyncResult, data: *mut c_void) {
    let reply: *mut JSCValue = data.cast();
    let answer = webkit_web_page_send_message_to_view_finish(page, result, std::ptr::null_mut());
    // A reply without parameters is an answer with no text, not an error.
    let parameters = if answer.is_null() { std::ptr::null_mut() } else { webkit_user_message_get_parameters(answer) };
    let text = (!parameters.is_null())
        .then(|| glib::Variant::from_glib_none(parameters))
        .and_then(|parameters| parameters.get::<(String,)>())
        .map(|(text,)| text);
    let context = jsc_value_get_context(reply);
    let mut arguments: Vec<*mut JSCValue> = Vec::new();
    if let Some(text) = text.and_then(|text| CString::new(text).ok()) {
        arguments.push(jsc_value_new_string(context, text.as_ptr()));
    }
    let returned = jsc_value_function_callv(reply, arguments.len() as u32, arguments.as_mut_ptr());
    for argument in arguments {
        g_object_unref(argument.cast());
    }
    if !returned.is_null() {
        g_object_unref(returned.cast());
    }
    if !answer.is_null() {
        g_object_unref(answer.cast());
    }
    g_object_unref(reply.cast());
}

unsafe fn throw(context: *mut JSCContext, message: &CStr) {
    jsc_context_throw(context, message.as_ptr());
}

unsafe fn owned_string(raw: *mut c_char) -> Option<String> {
    if raw.is_null() {
        return None;
    }
    let text = CStr::from_ptr(raw).to_str().ok().map(str::to_owned);
    glib::ffi::g_free(raw.cast());
    text
}
