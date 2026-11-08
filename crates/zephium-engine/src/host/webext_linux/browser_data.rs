//! `history`, `bookmarks`, `sessions` and `topSites`: requests to the shell,
//! which owns those stores, and the changes it reports back. The API's
//! translation is shared with the macOS port (`zephium_webext_linux::
//! browser_data`).

use std::collections::HashMap;

use serde_json::Value;
use zephium_core::extension_data::{ExtensionDataChange, ExtensionDataReply};
use zephium_core::ids::ProfileId;
use zephium_core::ports::engine::{EngineEvent, WebExtensionDataRequest};
use zephium_webext_linux::browser_data::{self, Request};

use super::routing::{error, ok, raise, Endpoint};
use crate::platform::imp::extension_channel::Message;

/// Requests waiting for the shell, by number.
#[derive(Default)]
pub(super) struct Pending {
    next: u64,
    /// The waiting message, the extension's own request number, the call.
    waiting: HashMap<u64, (Message, u64, Request)>,
}

/// More than this waiting at once is refused rather than queued.
const MAX_WAITING: usize = 64;

impl super::super::EngineHost {
    pub(super) fn linux_browser_data(&mut self, profile: ProfileId, message: Message, id: u64, api: &str, args: &[Value]) {
        let fields = args.first().cloned().unwrap_or(Value::Null);
        let request = match browser_data::request(api, &fields) {
            Ok(request) => request,
            Err(reason) => return message.reply(&error(id, &reason)),
        };
        let permitted = self
            .linux_extensions
            .profiles
            .get(&profile)
            .and_then(|state| state.install(&message.extension_id))
            .is_some_and(|install| install.permissions.iter().any(|permission| permission == request.permission));
        if !permitted {
            return message.reply(&error(id, &format!("The extension doesn't have the '{}' permission.", request.permission)));
        }
        let pending = &mut self.linux_extensions.data;
        if pending.waiting.len() >= MAX_WAITING {
            return message.reply(&error(id, "Too many requests are waiting; try again."));
        }
        pending.next += 1;
        let number = pending.next;
        let call = request.call.clone();
        pending.waiting.insert(number, (message, id, request));
        self.sink.emit(EngineEvent::WebExtensionDataRequested(Box::new(WebExtensionDataRequest { profile, request: number, call })));
    }

    /// The shell answered request `number`.
    pub(crate) fn answer_web_extension_data(&mut self, _profile: ProfileId, number: u64, reply: ExtensionDataReply) {
        let Some((message, id, request)) = self.linux_extensions.data.waiting.remove(&number) else {
            return;
        };
        match request.answer(reply) {
            Ok(value) => message.reply(&ok(id, value)),
            Err(reason) => message.reply(&error(id, &reason)),
        }
    }

    /// The shell's data changed: extensions with the permission hear it.
    pub(crate) fn web_extension_data_changed(&mut self, profile: ProfileId, change: ExtensionDataChange) {
        let Some(state) = self.linux_extensions.profiles.get(&profile) else {
            return;
        };
        let (permission, events) = browser_data::events(&change);
        for install in state.installs.values().filter(|install| install.permissions.iter().any(|granted| granted == permission)) {
            for view in install.own_views() {
                let endpoint = Endpoint { view, frame: 0 };
                for (name, args) in &events {
                    raise(&endpoint, &install.extension_id, name, args.clone());
                }
            }
        }
    }

    /// Tell the shell which changes the profile's extensions may hear.
    pub(super) fn linux_data_interest(&mut self, profile: ProfileId) {
        let Some(state) = self.linux_extensions.profiles.get(&profile) else {
            return;
        };
        let interest = browser_data::interest(state.installs.values().flat_map(|install| install.permissions.iter().map(String::as_str)));
        self.sink.emit(EngineEvent::WebExtensionDataInterest { profile, interest });
    }
}
