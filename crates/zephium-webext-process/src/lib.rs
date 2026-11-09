//! Zephium's WebKitGTK web-process plugin, the only channel between extension
//! code and the browser on Linux.
//!
//! WebKitGTK loads it into every web process from the directory the browser
//! passes to `webkit_web_context_set_web_extensions_directory`. It owns one
//! isolated script world per enabled extension and runs that extension's
//! content scripts there, at the time each asks for. Each extension world, and
//! the main world of the extension's own pages, gets two native functions: one
//! sends a bounded string to the browser, the other registers the world's
//! event listener. No script message handler exists, so there is no raw
//! handler to call around them. The reasoning is in the port's
//! docs/extensions.md.

/// The runtime's step-by-step log in the web process, when the browser runs
/// with `ZEPHIUM_WEBEXT_TRACE` set and WebKit passes it on.
#[cfg(all(unix, not(target_os = "macos")))]
/// Whether `ZEPHIUM_WEBEXT_TRACE` is set, read once: tracing sits on hot paths.
fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ZEPHIUM_WEBEXT_TRACE").is_some())
}

pub(crate) fn trace(line: std::fmt::Arguments) {
    if tracing() {
        eprintln!("webext plugin: {line}");
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod channel;
#[cfg(all(unix, not(target_os = "macos")))]
mod config;
#[cfg(all(unix, not(target_os = "macos")))]
mod events;
#[cfg(all(unix, not(target_os = "macos")))]
mod ffi;
pub mod matching;
#[cfg(all(unix, not(target_os = "macos")))]
mod requests;
#[cfg(all(unix, not(target_os = "macos")))]
mod run;
#[cfg(all(unix, not(target_os = "macos")))]
mod worlds;

/// Name of the message a world sends to the browser. Parameters `(ssts)`:
/// extension ID, sender kind, frame ID, body. The reply's parameters are `(s)`.
pub const SEND_MESSAGE: &str = "zephium-webext.send";
/// Name of the message the browser sends a page to raise an event in an
/// extension's worlds there. Parameters `(sts)`: extension ID, frame ID (0 for
/// every frame), body.
pub const EVENT_MESSAGE: &str = "zephium-webext.event";
/// Name of the message the browser sends a page to run code in an
/// extension's world of its main frame. Parameters `(ss)`: extension ID,
/// code. The reply's `(s)` is JSON: `{"ok": result}` or `{"error": message}`.
pub const RUN_MESSAGE: &str = "zephium-webext.run";
/// Name of the message the browser sends a page with the current enabled
/// extensions, `INIT_DATA_TYPE`, whenever they change.
pub const CONFIG_MESSAGE: &str = "zephium-webext.config";
/// Name of the message a page sends the browser to ask for the current
/// enabled extensions; the reply carries them as `INIT_DATA_TYPE`.
pub const CONFIG_REQUEST: &str = "zephium-webext.config-request";
/// Sender kind of a content script's isolated world.
pub const KIND_CONTENT: &str = "content";
/// Sender kind of an extension's own page (background, popup, options).
pub const KIND_PAGE: &str = "page";
/// The globals each extension context gets: the send function, the
/// listener registration, the extension's info block (JSON) and the kind of
/// context (`KIND_PAGE` or `KIND_CONTENT`).
pub const SEND_FUNCTION: &str = "__zephiumWebextSend";
pub const LISTEN_FUNCTION: &str = "__zephiumWebextListen";
pub const INFO_GLOBAL: &str = "__zephiumWebextInfo";
pub const CONTEXT_GLOBAL: &str = "__zephiumWebextContext";
/// Schemes whose pages are an extension's own: Chrome's, and Firefox's for
/// add-ons installed from addons.mozilla.org.
pub const EXTENSION_SCHEMES: [&str; 2] = ["chrome-extension://", "moz-extension://"];
/// Initialization data type: the per-message byte limit; for each enabled
/// extension its ID, its info block, the host patterns the user granted it,
/// and its content-script groups: matches, exclude_matches, scripts, run_at,
/// all_frames; then the socket that answers blocking `webRequest` questions
/// (empty for none) and the URL patterns anyone listens for. A group runs
/// only where both its own matches and the grant match.
pub const INIT_DATA_TYPE: &str = "(ua(ssasa(asasassb))sas)";

/// One `content_scripts` entry as the plugin gets it.
pub type InitGroup = (Vec<String>, Vec<String>, Vec<String>, String, bool);
/// One enabled extension as the plugin gets it.
pub type InitExtension = (String, String, Vec<String>, Vec<InitGroup>);

#[cfg(all(unix, not(target_os = "macos")))]
mod plugin {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use glib::translate::FromGlibPtrNone;

    use crate::ffi::WebKitWebExtension;
    use crate::requests::Interception;
    use crate::{events, worlds, InitExtension, INIT_DATA_TYPE};

    /// What the browser enabled for this web process at its start.
    pub(crate) struct Plugin {
        pub(crate) extension: *mut WebKitWebExtension,
        pub(crate) message_limit: usize,
        pub(crate) extensions: HashMap<String, Extension>,
        pub(crate) interception: Interception,
    }

    pub(crate) struct Extension {
        pub(crate) info: String,
        pub(crate) granted: Vec<String>,
        pub(crate) groups: Vec<Group>,
        pub(crate) world: worlds::World,
    }

    #[derive(Clone)]
    pub(crate) struct Group {
        pub(crate) matches: Vec<String>,
        pub(crate) exclude_matches: Vec<String>,
        pub(crate) scripts: Vec<String>,
        pub(crate) run_at: RunAt,
        pub(crate) all_frames: bool,
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    pub(crate) enum RunAt {
        Start,
        End,
        Idle,
    }

    thread_local! {
        pub(crate) static PLUGIN: RefCell<Option<Plugin>> = const { RefCell::new(None) };
    }

    /// Entry point WebKitGTK calls once per web process. The startup data may
    /// be stale (WebKit starts spare processes ahead of need), so the plugin
    /// also takes the current list later: see `configure`.
    #[no_mangle]
    pub unsafe extern "C" fn webkit_web_extension_initialize_with_user_data(
        extension: *mut WebKitWebExtension,
        user_data: *const glib::ffi::GVariant,
    ) {
        PLUGIN.with(|plugin| {
            *plugin.borrow_mut() = Some(Plugin {
                extension,
                message_limit: 0,
                extensions: HashMap::new(),
                interception: Interception::default(),
            });
        });
        if !user_data.is_null() {
            configure(&glib::Variant::from_glib_none(user_data));
        }
        worlds::watch_main_world();
        events::watch_pages(extension);
    }

    /// Take `data` (`INIT_DATA_TYPE`) as the enabled extensions. An extension
    /// already known keeps its world, so scripts running in it are untouched.
    pub(crate) fn configure(data: &glib::Variant) -> bool {
        if data.type_().as_str() != INIT_DATA_TYPE {
            return false;
        }
        let Some((limit, enabled, socket, patterns)) = data.get::<(u32, Vec<InitExtension>, String, Vec<String>)>() else {
            return false;
        };
        PLUGIN.with(|plugin| {
            let mut plugin = plugin.borrow_mut();
            let Some(plugin) = plugin.as_mut() else {
                return false;
            };
            let mut previous = std::mem::take(&mut plugin.extensions);
            plugin.extensions = enabled
                .into_iter()
                .filter(|(id, _, _, _)| is_extension_id(id))
                .map(|(id, info, granted, groups)| {
                    let world = previous.remove(&id).map_or_else(|| worlds::World::new(&id), |known| known.world);
                    let groups = groups.into_iter().map(group).collect();
                    (id, Extension { info, granted, groups, world })
                })
                .collect();
            plugin.message_limit = limit as usize;
            plugin.interception = Interception { socket, patterns };
            true
        })
    }

    fn group((matches, exclude_matches, scripts, run_at, all_frames): crate::InitGroup) -> Group {
        let run_at = match run_at.as_str() {
            "document_start" => RunAt::Start,
            "document_end" => RunAt::End,
            _ => RunAt::Idle,
        };
        Group { matches, exclude_matches, scripts, run_at, all_frames }
    }

    /// Chrome extension IDs: 32 letters from `a` to `p`.
    pub(crate) fn is_extension_id(id: &str) -> bool {
        id.len() == 32 && id.bytes().all(|byte| (b'a'..=b'p').contains(&byte))
    }
}
