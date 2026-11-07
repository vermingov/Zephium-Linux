//! Messages between an extension's contexts: one-off messages and their
//! answers, long-lived ports, and the events the browser raises.

use std::collections::HashMap;
use std::hash::Hash;

use serde_json::{json, Value};
use webkit2gtk::WebView;
use zephium_core::ids::ProfileId;

use crate::platform::imp::extension_channel::{self, Message};

/// Chrome's answer when nobody listens.
const NO_RECEIVER: &str = "Could not establish connection. Receiving end does not exist.";

#[derive(Default)]
pub(super) struct Routing {
    next: u64,
    waiting: HashMap<u64, Waiting>,
    ports: HashMap<u64, Port>,
    /// Chrome's tab and window IDs are small integers; Zephium's are ULIDs
    /// and 64-bit numbers.
    pub(super) tabs: IdMap<zephium_core::ids::ItemId>,
    pub(super) windows: IdMap<zephium_core::ids::WindowId>,
}

/// A one-off message waiting for its answer. Each world that got it answers
/// once, with a value or declining; the first value wins.
struct Waiting {
    extension_id: String,
    request: u64,
    sender: Message,
    views_left: usize,
    listeners: u32,
    declined: u32,
}

/// Where a context lives: its view and its frame (0 for every frame).
#[derive(Clone, PartialEq)]
pub(super) struct Endpoint {
    pub(super) view: WebView,
    pub(super) frame: u64,
}

struct Port {
    extension_id: String,
    opener: Endpoint,
    receivers: Vec<Endpoint>,
}

pub(super) struct IdMap<T> {
    numbers: HashMap<T, i64>,
    items: HashMap<i64, T>,
    next: i64,
}

impl<T> Default for IdMap<T> {
    fn default() -> Self {
        Self { numbers: HashMap::new(), items: HashMap::new(), next: 1 }
    }
}

impl<T: Copy + Eq + Hash> IdMap<T> {
    pub(super) fn number(&mut self, item: T) -> i64 {
        if let Some(number) = self.numbers.get(&item) {
            return *number;
        }
        let number = self.next;
        self.next += 1;
        self.numbers.insert(item, number);
        self.items.insert(number, item);
        number
    }

    pub(super) fn item(&self, number: i64) -> Option<T> {
        self.items.get(&number).copied()
    }
}

impl Routing {
    fn token(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    pub(super) fn forget_extension(&mut self, extension_id: &str) {
        self.ports.retain(|_, port| port.extension_id != extension_id);
        let gone: Vec<u64> =
            self.waiting.iter().filter(|(_, waiting)| waiting.extension_id == extension_id).map(|(token, _)| *token).collect();
        for token in gone {
            if let Some(waiting) = self.waiting.remove(&token) {
                waiting.sender.reply(&error(waiting.request, NO_RECEIVER));
            }
        }
    }
}

pub(super) fn ok(request: u64, value: Value) -> String {
    json!({ "id": request, "ok": value }).to_string()
}

pub(super) fn error(request: u64, message: &str) -> String {
    json!({ "id": request, "error": message }).to_string()
}

/// Raise `event` with `args` in `extension_id`'s worlds at `endpoint`.
pub(super) fn raise(endpoint: &Endpoint, extension_id: &str, event: &str, args: Value) {
    let body = json!({ "event": event, "args": args }).to_string();
    extension_channel::raise(&endpoint.view, extension_id, endpoint.frame, &body, |_| {});
}

impl super::super::EngineHost {
    /// Deliver a one-off message to `targets`; the sender's request is
    /// answered when a receiver answers, or when none will.
    pub(super) fn linux_send_message(
        &mut self,
        profile: ProfileId,
        sender: Message,
        request: u64,
        message: Value,
        sender_info: Value,
        targets: Vec<Endpoint>,
    ) {
        let Some(state) = self.linux_extensions.profiles.get_mut(&profile) else {
            return sender.reply(&error(request, NO_RECEIVER));
        };
        if targets.is_empty() {
            return sender.reply(&error(request, NO_RECEIVER));
        }
        let extension_id = sender.extension_id.clone();
        super::trace!("{extension_id} sends a message to {} context(s)", targets.len());
        let token = state.routing.token();
        let body = json!({ "event": "runtime.onMessage", "args": [message, sender_info], "reply": token }).to_string();
        state.routing.waiting.insert(
            token,
            Waiting { extension_id: extension_id.clone(), request, sender, views_left: targets.len(), listeners: 0, declined: 0 },
        );
        for target in targets {
            extension_channel::raise(&target.view, &extension_id, target.frame, &body, move |count| {
                super::super::dispatch::best_effort_with(move |host| host.linux_message_counted(profile, token, count));
            });
        }
    }

    fn linux_message_counted(&mut self, profile: ProfileId, token: u64, count: u32) {
        super::trace!("message {token}: a context had {count} listener(s)");
        let Some(waiting) = self.linux_waiting(profile, token) else {
            return;
        };
        waiting.views_left = waiting.views_left.saturating_sub(1);
        waiting.listeners += count;
        self.linux_settle_if_done(profile, token);
    }

    /// `runtime.answer`: a receiving world's answer, or its declining.
    pub(super) fn linux_message_answered(&mut self, profile: ProfileId, token: u64, value: Value, answered: bool) {
        if answered {
            if let Some(waiting) = self.linux_extensions.profiles.get_mut(&profile).and_then(|state| state.routing.waiting.remove(&token)) {
                waiting.sender.reply(&ok(waiting.request, value));
            }
            return;
        }
        if let Some(waiting) = self.linux_waiting(profile, token) {
            waiting.declined += 1;
        }
        self.linux_settle_if_done(profile, token);
    }

    fn linux_waiting(&mut self, profile: ProfileId, token: u64) -> Option<&mut Waiting> {
        self.linux_extensions.profiles.get_mut(&profile)?.routing.waiting.get_mut(&token)
    }

    fn linux_settle_if_done(&mut self, profile: ProfileId, token: u64) {
        let Some(state) = self.linux_extensions.profiles.get_mut(&profile) else {
            return;
        };
        let done = state
            .routing
            .waiting
            .get(&token)
            .is_some_and(|waiting| waiting.views_left == 0 && waiting.declined >= waiting.listeners);
        if !done {
            return;
        }
        let Some(waiting) = state.routing.waiting.remove(&token) else {
            return;
        };
        if waiting.listeners == 0 {
            waiting.sender.reply(&error(waiting.request, NO_RECEIVER));
        } else {
            // Every listener declined: the sender's callback runs with nothing.
            waiting.sender.reply(&json!({ "id": waiting.request }).to_string());
        }
    }

    /// Open a port from `opener` to `receivers`; answers with its number.
    pub(super) fn linux_connect(
        &mut self,
        profile: ProfileId,
        extension_id: &str,
        opener: Endpoint,
        name: &str,
        sender_info: Value,
        receivers: Vec<Endpoint>,
    ) -> Result<Value, String> {
        if receivers.is_empty() {
            return Err(NO_RECEIVER.into());
        }
        let state = self.linux_extensions.profiles.get_mut(&profile).ok_or(NO_RECEIVER)?;
        let id = state.routing.token();
        for receiver in &receivers {
            raise(receiver, extension_id, "runtime.onConnect", json!([id, name, sender_info]));
        }
        state.routing.ports.insert(id, Port { extension_id: extension_id.to_owned(), opener, receivers });
        Ok(json!(id))
    }

    pub(super) fn linux_port_post(&mut self, profile: ProfileId, from: &Endpoint, id: u64, message: Value) -> Result<Value, String> {
        let state = self.linux_extensions.profiles.get(&profile).ok_or("no such port")?;
        let port = state.routing.ports.get(&id).ok_or("Attempting to use a disconnected port object")?;
        for target in port.other_side(from) {
            raise(&target, &port.extension_id, "port.message", json!([id, message.clone()]));
        }
        Ok(Value::Null)
    }

    pub(super) fn linux_port_disconnect(&mut self, profile: ProfileId, from: &Endpoint, id: u64) -> Result<Value, String> {
        let state = self.linux_extensions.profiles.get_mut(&profile).ok_or("no such port")?;
        if let Some(port) = state.routing.ports.remove(&id) {
            for target in port.other_side(from) {
                raise(&target, &port.extension_id, "port.disconnect", json!([id, Value::Null]));
            }
        }
        Ok(Value::Null)
    }
}

impl Port {
    /// The ends a message from `from` goes to.
    fn other_side(&self, from: &Endpoint) -> Vec<Endpoint> {
        let from_opener = from.view == self.opener.view && (from.frame == self.opener.frame || self.opener.frame == 0);
        if from_opener {
            self.receivers.clone()
        } else {
            vec![self.opener.clone()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IdMap;

    #[test]
    fn numbers_are_stable_and_reversible() {
        let mut map = IdMap::<u64>::default();
        let first = map.number(900);
        assert_eq!(map.number(900), first);
        assert_ne!(map.number(901), first);
        assert_eq!(map.item(first), Some(900));
        assert_eq!(map.item(999), None);
    }
}
