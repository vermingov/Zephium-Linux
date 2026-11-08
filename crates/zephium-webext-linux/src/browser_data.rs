//! `history`, `bookmarks`, `sessions` and `topSites`: data in the browser's
//! own stores, which the shell reads and writes. The translation between
//! these APIs and the browser's requests is the macOS port's, compiled here
//! unchanged so both ports answer alike.

use serde_json::{json, Value};
use zephium_core::extension_data::{ExtensionDataCall, ExtensionDataChange, ExtensionDataInterest, ExtensionDataReply};

// Their event lists serve the macOS port's listener bookkeeping.
#[allow(dead_code)]
#[path = "../../zephium-webext-macos/src/bookmarks.rs"]
mod bookmarks;
#[allow(dead_code)]
#[path = "../../zephium-webext-macos/src/history.rs"]
mod history;
#[path = "../../zephium-webext-macos/src/sessions.rs"]
mod sessions;
#[path = "../../zephium-webext-macos/src/top_sites.rs"]
mod top_sites;

/// Whether `api` is one of these namespaces' calls.
pub fn handles(api: &str) -> bool {
    ["history.", "bookmarks.", "sessions.", "topSites."].iter().any(|namespace| api.starts_with(namespace))
}

/// One call on its way to the browser.
pub struct Request {
    pub permission: &'static str,
    pub call: ExtensionDataCall,
    answer: crate::data::Answer,
    restoring: bool,
}

/// `api` with the macOS port's argument object (`{ details }`, `{ id }`…).
pub fn request(api: &str, message: &Value) -> Result<Request, String> {
    let call = match api.split_once('.') {
        Some(("history", op)) => history::call(op, message),
        Some(("bookmarks", op)) => bookmarks::call(op, message),
        Some(("sessions", op)) => sessions::call(op, message),
        Some(("topSites", op)) => top_sites::call(op, message),
        _ => Err(format!("{api} is not supported")),
    }?;
    Ok(Request { permission: call.permission, call: call.request, answer: call.answer, restoring: api == "sessions.restore" })
}

impl Request {
    /// The browser's answer as the extension reads it.
    pub fn answer(self, reply: ExtensionDataReply) -> Result<Value, String> {
        match reply {
            ExtensionDataReply::Unavailable if self.restoring => Err("No closed tab can be reopened in this window.".into()),
            ExtensionDataReply::Unavailable => Err("This profile keeps no such data, or it can't be reached now.".into()),
            ExtensionDataReply::Busy => Err("Too many requests are waiting; try again.".into()),
            reply => (self.answer)(reply),
        }
    }
}

/// The events a change raises, and the permission a listener needs.
pub fn events(change: &ExtensionDataChange) -> (&'static str, Vec<(&'static str, Value)>) {
    match change {
        ExtensionDataChange::Visited(entry) => (history::PERMISSION, vec![(history::ON_VISITED, json!([history::item(entry)]))]),
        ExtensionDataChange::VisitsRemoved { urls, all } => (
            history::PERMISSION,
            vec![(history::ON_VISIT_REMOVED, json!([{ "allHistory": all, "urls": if *all { &[][..] } else { &urls[..] } }]))],
        ),
        ExtensionDataChange::Bookmark(change) => (bookmarks::PERMISSION, bookmarks::events(change)),
        ExtensionDataChange::Sessions => (sessions::PERMISSION, vec![(sessions::ON_CHANGED, json!([]))]),
    }
}

/// What the shell should report, given the extensions' permissions.
pub fn interest<'a>(permissions: impl Iterator<Item = &'a str>) -> ExtensionDataInterest {
    let mut interest = ExtensionDataInterest::default();
    for permission in permissions {
        match permission {
            history::PERMISSION => interest.history = true,
            bookmarks::PERMISSION => interest.bookmarks = true,
            sessions::PERMISSION => interest.sessions = true,
            _ => {}
        }
    }
    interest
}
