//! What the browser-data translations borrowed from the macOS port expect of
//! their crate: a call's permission, request and how its answer reads, and
//! three small readers of the extension's arguments.

use serde_json::Value;
use zephium_core::extension_data::{ExtensionDataCall, ExtensionDataReply};

pub(crate) type Answer = Box<dyn FnOnce(ExtensionDataReply) -> Result<Value, String>>;

pub(crate) struct Call {
    pub(crate) permission: &'static str,
    pub(crate) request: ExtensionDataCall,
    pub(crate) answer: Answer,
}

pub(crate) fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite() && *n >= 0.0)
}

pub(crate) fn text<'a>(message: &'a Value, key: &str) -> Result<&'a str, String> {
    message.get(key).and_then(Value::as_str).ok_or_else(|| format!("'{key}' must be a string."))
}

pub(crate) fn unexpected(reply: ExtensionDataReply) -> Result<Value, String> {
    let _ = reply;
    Err("The browser answered with something unexpected.".into())
}
