//! The browser side of the extension channel.
//!
//! WebKitGTK loads zephium-webext-process into each web process of a profile
//! and hands it, at process start, the extensions enabled in that profile.
//! Extension code reaches the browser only through the plugin's one function;
//! this module receives those messages and checks them again before anything
//! acts on them. The reasoning is in the Linux port's docs/extensions.md.
//!
//! The extension runtime, the port's next step, is what sets a channel's
//! extensions and handler and reads the messages; until it lands, those parts
//! have no caller.
#![allow(dead_code)]

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::glib::prelude::ObjectExt;
use gtk::glib::ToVariant;
use webkit2gtk::{UserMessage, UserMessageExt, WebContext, WebContextExt, WebView, WebViewExt};
use zephium_webext_process::{
    InitGroup, CONFIG_MESSAGE, CONFIG_REQUEST, EVENT_MESSAGE, INIT_DATA_TYPE, KIND_CONTENT, KIND_PAGE, SEND_MESSAGE,
};

const PLUGIN_FILE: &str = "libzephium_webext_process.so";
/// Bytes one message may carry. The plugin refuses larger ones in the web
/// process; the browser refuses them again.
pub(crate) const MESSAGE_LIMIT: usize = 64 * 1_024;
const CHANNEL_KEY: &str = "zephium-extension-channel";

type Handler = Rc<dyn Fn(Message)>;

/// What the plugin needs of one enabled extension: its ID, the info block
/// its contexts start with (`zephium_webext_linux::info`), the host patterns
/// the user granted it, and its content scripts
/// (`zephium_webext_linux::content`).
#[derive(Clone, Debug)]
pub(crate) struct ChannelExtension {
    pub(crate) id: String,
    pub(crate) info: String,
    pub(crate) granted: Vec<String>,
    pub(crate) groups: Vec<InitGroup>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SenderKind {
    /// A content script, in the extension's isolated world of a web page.
    Content,
    /// The extension's own page: background, popup or options.
    Page,
}

/// One message from extension code. Answer it with [`Message::reply`];
/// dropping it unanswered sends an empty reply, so the sender's callback
/// still runs.
pub(crate) struct Message {
    /// The view whose page sent it: a tab, or one of the extension's own.
    pub(crate) view: WebView,
    pub(crate) extension_id: String,
    pub(crate) kind: SenderKind,
    pub(crate) frame_id: u64,
    pub(crate) body: String,
    pending: Option<UserMessage>,
}

impl Message {
    pub(crate) fn reply(mut self, answer: &str) {
        if let Some(message) = self.pending.take() {
            message.send_reply(&UserMessage::new("reply", Some(&(answer,).to_variant())));
        }
    }
}

impl Drop for Message {
    fn drop(&mut self) {
        if let Some(message) = self.pending.take() {
            message.send_reply(&UserMessage::new("reply", None));
        }
    }
}

/// A profile's channel: the extensions its new web processes get, and where
/// their messages go.
#[derive(Default)]
pub(crate) struct Channel {
    extensions: RefCell<Vec<ChannelExtension>>,
    handler: RefCell<Option<Handler>>,
}

impl Channel {
    /// Web processes started from now on get `extensions`.
    pub(crate) fn set_extensions(&self, extensions: Vec<ChannelExtension>) {
        *self.extensions.borrow_mut() = extensions;
    }

    pub(crate) fn set_handler(&self, handler: impl Fn(Message) + 'static) {
        *self.handler.borrow_mut() = Some(Rc::new(handler));
    }

    /// The enabled extensions as the plugin takes them, at startup or later.
    pub(crate) fn current_data(&self) -> gtk::glib::Variant {
        let extensions: Vec<zephium_webext_process::InitExtension> =
            self.extensions.borrow().iter().map(|e| (e.id.clone(), e.info.clone(), e.granted.clone(), e.groups.clone())).collect();
        let data = (MESSAGE_LIMIT as u32, extensions).to_variant();
        debug_assert_eq!(data.type_().as_str(), INIT_DATA_TYPE);
        data
    }

    fn enabled(&self, extension_id: &str) -> bool {
        self.extensions.borrow().iter().any(|e| e.id == extension_id)
    }

    /// The plugin's message, if it passes the browser's own checks.
    fn accept(&self, view: &WebView, message: &UserMessage) -> Option<Message> {
        let (extension_id, kind, frame_id, body) =
            message.parameters()?.get::<(String, String, u64, String)>()?;
        let kind = match kind.as_str() {
            KIND_CONTENT => SenderKind::Content,
            KIND_PAGE => SenderKind::Page,
            _ => return None,
        };
        if body.len() > MESSAGE_LIMIT || !self.enabled(&extension_id) {
            return None;
        }
        Some(Message { view: view.clone(), extension_id, kind, frame_id, body, pending: Some(message.clone()) })
    }
}

/// Load the plugin into `context`'s web processes and return its channel.
/// Call before the context starts its first web process; later calls return
/// the same channel. `None` when the installed plugin is missing, which
/// leaves extensions unavailable rather than half-working.
pub(crate) fn attach(context: &WebContext) -> Option<Rc<Channel>> {
    if let Some(channel) = channel(context) {
        return Some(channel);
    }
    let Some(directory) = plugin_directory() else {
        eprintln!("extensions: the web-process plugin is not installed beside the browser");
        return None;
    };
    context.set_web_extensions_directory(&directory.to_string_lossy());
    let channel = Rc::new(Channel::default());
    let starting = channel.clone();
    context.connect_initialize_web_extensions(move |context| {
        context.set_web_extensions_initialization_user_data(&starting.current_data());
    });
    // SAFETY: this private key is written and read only as `Rc<Channel>`
    // here, and lives exactly as long as the context GLib object.
    unsafe { context.set_data(CHANNEL_KEY, channel.clone()) };
    Some(channel)
}

pub(crate) fn channel(context: &WebContext) -> Option<Rc<Channel>> {
    // SAFETY: see `attach`.
    unsafe { context.data::<Rc<Channel>>(CHANNEL_KEY).map(|channel| channel.as_ref().clone()) }
}

/// Deliver messages the plugin sends from `view`'s pages to `channel`.
pub(crate) fn listen(view: &WebView, channel: Rc<Channel>) {
    view.connect_user_message_received(move |view, message| {
        // A process WebKit started earlier asks for the current list.
        if message.name().as_deref() == Some(CONFIG_REQUEST) {
            message.send_reply(&UserMessage::new(CONFIG_REQUEST, Some(&channel.current_data())));
            return true;
        }
        if message.name().as_deref() != Some(SEND_MESSAGE) {
            return false;
        }
        let Some(accepted) = channel.accept(view, message) else {
            // Refused: no reply with content, and nothing acts on it.
            message.send_reply(&UserMessage::new("reply", None));
            return true;
        };
        let handler = channel.handler.borrow().clone();
        if let Some(handler) = handler {
            handler(accepted);
        }
        true
    });
}

/// Raise an event in `extension_id`'s worlds in `view`'s page: one frame, or
/// all of them for frame 0. `counted` learns how many listeners got it (0 when
/// the page could not be reached).
pub(crate) fn raise(view: &WebView, extension_id: &str, frame_id: u64, body: &str, counted: impl FnOnce(u32) + 'static) {
    let event = UserMessage::new(EVENT_MESSAGE, Some(&(extension_id, frame_id, body).to_variant()));
    view.send_message_to_page(&event, None::<&gtk::gio::Cancellable>, move |reply| {
        let count = reply
            .ok()
            .and_then(|reply| reply.parameters())
            .and_then(|parameters| parameters.get::<(u32,)>())
            .map_or(0, |(count,)| count);
        counted(count);
    });
}

/// Give `view`'s page the current enabled extensions.
pub(crate) fn configure_page(view: &WebView, data: &gtk::glib::Variant) {
    let message = UserMessage::new(CONFIG_MESSAGE, Some(data));
    view.send_message_to_page(&message, None::<&gtk::gio::Cancellable>, |_| {});
}

/// `<binary>/../lib/app.zephium/webext` as packaged, or `<binary>/webext` in a
/// development build. WebKit loads every library in the directory, so it
/// must hold the plugin and nothing else.
fn plugin_directory() -> Option<PathBuf> {
    let binary = std::env::current_exe().ok()?.canonicalize().ok()?;
    let bin = binary.parent()?;
    [bin.join("../lib/app.zephium/webext"), bin.join("webext")]
        .into_iter()
        .filter_map(|candidate| candidate.canonicalize().ok())
        .find(|directory| holds_only_plugin(directory))
}

fn holds_only_plugin(directory: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return false;
    };
    let names: Vec<_> = entries.filter_map(|entry| entry.ok().map(|entry| entry.file_name())).collect();
    names.len() == 1 && names[0] == PLUGIN_FILE && directory.join(PLUGIN_FILE).is_file()
}

#[cfg(test)]
mod tests {
    use super::holds_only_plugin;

    #[test]
    fn the_plugin_directory_must_hold_the_plugin_alone() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!holds_only_plugin(directory.path()));
        std::fs::write(directory.path().join(super::PLUGIN_FILE), b"").unwrap();
        assert!(holds_only_plugin(directory.path()));
        std::fs::write(directory.path().join("other.so"), b"").unwrap();
        assert!(!holds_only_plugin(directory.path()));
    }
}
