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

#[cfg(all(unix, not(target_os = "macos")))]
mod channel;
#[cfg(all(unix, not(target_os = "macos")))]
mod events;
#[cfg(all(unix, not(target_os = "macos")))]
mod ffi;
pub mod matching;
#[cfg(all(unix, not(target_os = "macos")))]
mod worlds;

/// Name of the message a world sends to the browser. Parameters `(ssts)`:
/// extension ID, sender kind, frame ID, body. The reply's parameters are `(s)`.
pub const SEND_MESSAGE: &str = "zephium-webext.send";
/// Name of the message the browser sends a page to raise an event in an
/// extension's worlds there. Parameters `(sts)`: extension ID, frame ID (0 for
/// every frame), body.
pub const EVENT_MESSAGE: &str = "zephium-webext.event";
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
/// Initialization data type: the per-message byte limit, then for each
/// enabled extension its ID, its info block, the host patterns the user
/// granted it, and its content-script groups: matches, exclude_matches,
/// scripts, run_at, all_frames. A group runs only where both its own matches
/// and the grant match.
pub const INIT_DATA_TYPE: &str = "(ua(ssasa(asasassb)))";

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
    use crate::{events, worlds, InitExtension, INIT_DATA_TYPE};

    /// What the browser enabled for this web process at its start.
    pub(crate) struct Plugin {
        pub(crate) extension: *mut WebKitWebExtension,
        pub(crate) message_limit: usize,
        pub(crate) extensions: HashMap<String, Extension>,
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

    /// Entry point WebKitGTK calls once per web process.
    #[no_mangle]
    pub unsafe extern "C" fn webkit_web_extension_initialize_with_user_data(
        extension: *mut WebKitWebExtension,
        user_data: *const glib::ffi::GVariant,
    ) {
        if user_data.is_null() {
            return;
        }
        let data = glib::Variant::from_glib_none(user_data);
        if data.type_().as_str() != INIT_DATA_TYPE {
            return;
        }
        let Some((limit, enabled)) = data.get::<(u32, Vec<InitExtension>)>() else {
            return;
        };
        let extensions = enabled
            .into_iter()
            .filter(|(id, _, _, _)| is_extension_id(id))
            .map(|(id, info, granted, groups)| {
                let world = worlds::World::new(&id);
                let groups = groups.into_iter().map(group).collect();
                (id, Extension { info, granted, groups, world })
            })
            .collect();
        PLUGIN.with(|plugin| {
            *plugin.borrow_mut() = Some(Plugin { extension, message_limit: limit as usize, extensions });
        });
        worlds::watch_main_world();
        events::watch_pages(extension);
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
