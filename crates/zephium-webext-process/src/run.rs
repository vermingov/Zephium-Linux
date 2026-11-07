//! `scripting.executeScript`, `insertCSS` and their MV2 forms: the browser,
//! having checked the extension may touch the tab, sends code to run in the
//! extension's world of the page's main frame. The reply is `(s)`: JSON
//! `{"ok": result}`, or `{"error": message}` when the code threw.

use std::ffi::{CStr, CString};

use glib::gobject_ffi::g_object_unref;
use glib::translate::{FromGlibPtrNone, ToGlibPtr};
use glib::ToVariant;

use crate::channel::{self, Sender};
use crate::ffi::*;
use crate::plugin::PLUGIN;
use crate::{KIND_CONTENT, RUN_MESSAGE, SEND_FUNCTION};

pub(crate) unsafe fn run(page: *mut WebKitWebPage, message: *mut WebKitUserMessage) {
    let parameters = webkit_user_message_get_parameters(message);
    let request = (!parameters.is_null())
        .then(|| glib::Variant::from_glib_none(parameters))
        .and_then(|parameters| parameters.get::<(String, String)>());
    let answer = match request {
        Some((extension_id, code)) => evaluate_in(page, &extension_id, &code),
        None => error("malformed request"),
    };
    let name = CString::new(RUN_MESSAGE).expect("constant has no NUL");
    let reply = webkit_user_message_new(name.as_ptr(), (answer,).to_variant().to_glib_none().0);
    webkit_user_message_send_reply(message, reply);
}

unsafe fn evaluate_in(page: *mut WebKitWebPage, extension_id: &str, code: &str) -> String {
    let found = PLUGIN.with(|plugin| {
        plugin
            .borrow()
            .as_ref()
            .and_then(|plugin| plugin.extensions.get(extension_id))
            .map(|extension| (extension.world.raw(), extension.info.clone()))
    });
    let Some((world, info)) = found else {
        return error("the extension is not enabled in this page");
    };
    let frame = webkit_web_page_get_main_frame(page);
    let context = webkit_frame_get_js_context_for_script_world(frame, world);
    if context.is_null() {
        return error("the page has no document to run in");
    }
    // A world no content script has used yet gets its channel first.
    let global = jsc_context_get_global_object(context);
    let name = CString::new(SEND_FUNCTION).expect("constant has no NUL");
    if !ok(jsc_value_object_has_property(global, name.as_ptr())) {
        let sender = Sender::new(extension_id, KIND_CONTENT, webkit_web_page_get_id(page), webkit_frame_get_id(frame));
        channel::install(context, sender, &info, KIND_CONTENT);
    }
    g_object_unref(global.cast());

    let result = jsc_context_evaluate(context, code.as_ptr().cast(), code.len() as isize);
    let exception = jsc_context_get_exception(context);
    let answer = if !exception.is_null() {
        let text = jsc_exception_get_message(exception);
        let text = if text.is_null() { "the script threw".to_owned() } else { CStr::from_ptr(text).to_string_lossy().into_owned() };
        jsc_context_clear_exception(context);
        error(&text)
    } else {
        format!(r#"{{"ok":{}}}"#, to_json(result))
    };
    if !result.is_null() {
        g_object_unref(result.cast());
    }
    g_object_unref(context.cast());
    answer
}

/// The value as JSON; `undefined` and what JSON cannot hold become `null`.
unsafe fn to_json(value: *mut JSCValue) -> String {
    if value.is_null() || ok(jsc_value_is_undefined(value)) {
        return "null".to_owned();
    }
    let raw = jsc_value_to_json(value, 0);
    if raw.is_null() {
        return "null".to_owned();
    }
    let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
    glib::ffi::g_free(raw.cast());
    text
}

fn error(message: &str) -> String {
    format!(r#"{{"error":{}}}"#, json_string(message))
}

/// `text` as a JSON string literal.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if u32::from(control) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(control))),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn errors_are_valid_json_strings() {
        assert_eq!(super::json_string("a\"b\\c\n"), r#""a\"b\\c\u000a""#);
    }
}
