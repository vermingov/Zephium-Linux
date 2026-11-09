//! `webRequest`: the listeners an extension's background registers, and the
//! requests the web-process plugin offers them (platform/linux/
//! request_broker.rs).
//!
//! Only `onBeforeRequest` sees requests: WebKitGTK gives the port no hook
//! later in a request's life. Every extension whose listener wants the
//! request hears of it; the blocking ones answer, and the request waits for
//! them. A cancel wins over everything, then the first redirect.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use webkit2gtk::WebViewExt;
use wry::WebViewExtUnix;
use zephium_core::ids::{ItemId, ProfileId};
use zephium_webext_process::matching::{any_matches, is_restricted};

use super::routing::{raise, Endpoint};
use crate::platform::imp::extension_channel::{self, Message};
use crate::platform::imp::request_broker::{Ask, Decision, Question};

const ON_BEFORE_REQUEST: &str = "webRequest.onBeforeRequest";
/// How long the listeners get; the broker waits a little longer.
const WAIT: Duration = Duration::from_millis(2_000);

/// One registered listener: its event, its filter's URL patterns and
/// resource types, and whether it may block.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Listener {
    event: String,
    urls: Vec<String>,
    types: Option<Vec<String>>,
    blocking: bool,
}

impl Listener {
    fn wants(&self, url: &str, kind: &str) -> bool {
        self.event == ON_BEFORE_REQUEST
            && any_matches(&self.urls, url)
            && self.types.as_ref().is_none_or(|types| types.iter().any(|known| known == kind))
    }
}

/// The listeners as the API layer reports them:
/// `[{"event", "urls", "types", "blocking"}]`.
fn listeners(value: &Value) -> Vec<Listener> {
    let strings = |value: &Value| -> Option<Vec<String>> {
        value.as_array().map(|list| list.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
    };
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(Listener {
                event: format!("webRequest.{}", entry["event"].as_str()?),
                urls: strings(&entry["urls"])?,
                types: strings(&entry["types"]),
                blocking: entry["blocking"] == true,
            })
        })
        .collect()
}

/// The URL patterns any of `installs`' backgrounds asks about: what the
/// plugin offers the browser.
pub(super) fn interception<'a>(installs: impl Iterator<Item = &'a super::Install>) -> Vec<String> {
    let mut patterns: Vec<String> = installs
        .flat_map(|install| install.web_request.iter())
        .filter(|listener| listener.event == ON_BEFORE_REQUEST)
        .flat_map(|listener| listener.urls.iter().cloned())
        .collect();
    patterns.sort();
    patterns.dedup();
    patterns
}

/// Requests waiting for blocking listeners.
#[derive(Default)]
pub(super) struct Pending {
    next: u64,
    waiting: HashMap<u64, Waiting>,
}

struct Waiting {
    ask: Ask,
    left: usize,
    redirect: Option<String>,
}

impl Waiting {
    fn finish(self) {
        let decision = self.redirect.map_or(Decision::Allow, Decision::Redirect);
        self.ask.answer(decision);
    }
}

/// `webRequest.onBeforeRequest`'s details, Firefox's fields with Chrome's
/// `initiator`. WebKit does not say which frame a request comes from: what a
/// page loads counts as its main frame's, a frame's document as that frame's.
fn details(question: &Question, request: u64, tab: i64) -> Value {
    let time = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1000.0);
    let (frame, parent) = match question.kind.as_str() {
        "main_frame" => (0, -1),
        "sub_frame" => (question.frame_id as i64, 0),
        _ => (0, -1),
    };
    let mut details = json!({
        "requestId": request.to_string(),
        "url": question.url,
        "method": question.method,
        "frameId": frame,
        "parentFrameId": parent,
        "tabId": tab,
        "type": question.kind,
        "timeStamp": time,
        "incognito": false,
        "frameAncestors": [],
    });
    if question.kind != "main_frame" && !question.document.is_empty() {
        details["documentUrl"] = json!(question.document);
        details["originUrl"] = json!(question.document);
        details["initiator"] = json!(origin(&question.document));
        if question.kind == "sub_frame" {
            details["frameAncestors"] = json!([{ "url": question.document, "frameId": 0 }]);
        }
    }
    details
}

fn origin(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "null".into();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("{scheme}://{authority}")
}

/// Where an extension may send a request instead: the web, a data URL, or
/// its own package.
fn may_redirect_to(target: &str, extension_id: &str) -> bool {
    ["http://", "https://", "data:"].iter().any(|prefix| target.starts_with(prefix))
        || zephium_webext_process::EXTENSION_SCHEMES
            .iter()
            .any(|scheme| target.strip_prefix(scheme).is_some_and(|rest| rest.starts_with(&format!("{extension_id}/"))))
}

impl super::super::EngineHost {
    /// `webRequest.listeners`: the background's listeners, all of them, each
    /// time they change. Other pages of the extension do not count.
    pub(super) fn linux_web_request_listeners(&mut self, profile: ProfileId, message: &Message, value: &Value) -> Result<Value, String> {
        let install = self
            .linux_extensions
            .profiles
            .get_mut(&profile)
            .and_then(|state| state.install_mut(&message.extension_id))
            .ok_or("The extension is not running.")?;
        if install.background.as_ref() != Some(&message.view) {
            return Ok(Value::Null);
        }
        install.web_request = listeners(value);
        super::trace!("{} listens to {} webRequest filter(s)", message.extension_id, install.web_request.len());
        self.linux_web_request_reconfigure(profile);
        Ok(Value::Null)
    }

    /// Tell the profile's pages which requests to ask about, when that
    /// changed.
    fn linux_web_request_reconfigure(&mut self, profile: ProfileId) {
        let tab_views = self.linux_tab_views(profile);
        let Some(state) = self.linux_extensions.profiles.get(&profile) else {
            return;
        };
        let Some(channel) = &state.channel else {
            return;
        };
        if !channel.set_interception(interception(state.installs.values())) {
            return;
        }
        let data = channel.current_data();
        for view in &tab_views {
            extension_channel::configure_page(view, &data);
        }
    }

    /// A page is about to make a request someone listens for: offer it to
    /// the listeners that want it. An `ask` dropped unanswered lets the
    /// request go.
    pub(crate) fn linux_web_request(&mut self, ask: Ask) {
        let question = &ask.question;
        if is_restricted(&question.url) || is_restricted(&question.document) {
            return;
        }
        let Some((profile, item)) = self.linux_page_tab(question.page_id) else {
            return;
        };
        let Some(state) = self.linux_extensions.profiles.get_mut(&profile) else {
            return;
        };
        let tab = state.routing.tabs.number(item);
        let pending = &mut self.linux_extensions.web_requests;
        pending.next += 1;
        let token = pending.next;
        let details = details(question, token, tab);
        let mut blocking = Vec::new();
        for install in state.installs.values() {
            let Some(background) = &install.background else {
                continue;
            };
            let wanted: Vec<&Listener> =
                install.web_request.iter().filter(|listener| listener.wants(&question.url, &question.kind)).collect();
            if wanted.is_empty() || !any_matches(&install.hosts, &question.url) {
                continue;
            }
            if wanted.iter().any(|listener| listener.blocking) {
                blocking.push((install.extension_id.clone(), background.clone()));
            } else {
                raise(&Endpoint { view: background.clone(), frame: 0 }, &install.extension_id, ON_BEFORE_REQUEST, json!([details]));
            }
        }
        if blocking.is_empty() {
            return;
        }
        let body = json!({ "event": ON_BEFORE_REQUEST, "args": [details], "reply": token }).to_string();
        pending.waiting.insert(token, Waiting { ask, left: blocking.len(), redirect: None });
        for (extension_id, background) in blocking {
            extension_channel::raise(&background, &extension_id.clone(), 0, &body, move |count| {
                // A background without the listener yet answers nothing.
                if count == 0 {
                    super::super::dispatch::best_effort_with(move |host| {
                        host.linux_web_request_answered(token, &extension_id, &Value::Null);
                    });
                }
            });
        }
        gtk::glib::timeout_add_local_once(WAIT, move || {
            super::super::dispatch::best_effort_with(move |host| {
                if let Some(waiting) = host.linux_extensions.web_requests.waiting.remove(&token) {
                    super::trace!("webRequest {token}: the listeners took too long");
                    waiting.finish();
                }
            });
        });
    }

    /// `webRequest.answer`: one extension's merged blocking response.
    pub(super) fn linux_web_request_answered(&mut self, token: u64, extension_id: &str, response: &Value) {
        let pending = &mut self.linux_extensions.web_requests;
        let Some(waiting) = pending.waiting.get_mut(&token) else {
            return;
        };
        if response["cancel"] == true {
            if let Some(waiting) = pending.waiting.remove(&token) {
                super::trace!("{extension_id} cancels {}", waiting.ask.question.url);
                waiting.ask.answer(Decision::Cancel);
            }
            return;
        }
        if let Some(target) = response["redirectUrl"].as_str() {
            if waiting.redirect.is_none() && may_redirect_to(target, extension_id) {
                super::trace!("{extension_id} redirects {} to {target}", waiting.ask.question.url);
                waiting.redirect = Some(target.to_owned());
            }
        }
        waiting.left = waiting.left.saturating_sub(1);
        if waiting.left == 0 {
            if let Some(waiting) = pending.waiting.remove(&token) {
                waiting.finish();
            }
        }
    }

    /// The tab, and its profile, whose view shows WebKit page `page_id`.
    fn linux_page_tab(&self, page_id: u64) -> Option<(ProfileId, ItemId)> {
        let (item, _) = self.views.iter().find(|(_, observed)| observed.view.webview().page_id() == page_id)?;
        let partition = self.partitions.get(item)?;
        Some((partition.profile(), *item))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn listeners_keep_their_filters() {
        let reported = json!([
            { "event": "onBeforeRequest", "urls": ["http://*/*", "https://*/*"], "types": null, "blocking": true },
            { "event": "onCompleted", "urls": ["<all_urls>"], "types": ["script"], "blocking": false },
            { "event": "onBeforeRequest" },
        ]);
        let listeners = listeners(&reported);
        assert_eq!(listeners.len(), 2);
        assert!(listeners[0].wants("https://ads.test/a.js", "script"));
        assert!(!listeners[1].wants("https://ads.test/a.js", "script"));
    }

    #[test]
    fn types_narrow_what_a_listener_wants() {
        let listener = Listener { event: ON_BEFORE_REQUEST.into(), urls: vec!["<all_urls>".into()], types: Some(vec!["image".into()]), blocking: true };
        assert!(listener.wants("https://a.test/p.png", "image"));
        assert!(!listener.wants("https://a.test/a.js", "script"));
    }

    #[test]
    fn subresources_belong_to_the_page_and_frames_to_their_parent() {
        let question = |kind: &str, frame: u64| Question {
            page_id: 1,
            frame_id: frame,
            kind: kind.into(),
            method: "GET".into(),
            url: "https://ads.test/x".into(),
            document: "https://site.test/page".into(),
        };
        let script = details(&question("script", 0), 9, 3);
        assert_eq!((script["frameId"].clone(), script["parentFrameId"].clone()), (json!(0), json!(-1)));
        assert_eq!(script["initiator"], "https://site.test");
        assert_eq!(script["tabId"], 3);
        let frame = details(&question("sub_frame", 4), 9, 3);
        assert_eq!((frame["frameId"].clone(), frame["parentFrameId"].clone()), (json!(4), json!(0)));
        let top = details(&question("main_frame", 0), 9, 3);
        assert!(top.get("documentUrl").is_none());
    }

    #[test]
    fn redirects_stay_on_the_web_or_in_the_extension() {
        let id = "pbndgdjohcldagmpdjpmmgmidaehmafj";
        assert!(may_redirect_to("https://example.test/", id));
        assert!(may_redirect_to(&format!("moz-extension://{id}/web_accessible_resources/noop.js"), id));
        assert!(!may_redirect_to("moz-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/x.js", id));
        assert!(!may_redirect_to("file:///etc/passwd", id));
    }
}
