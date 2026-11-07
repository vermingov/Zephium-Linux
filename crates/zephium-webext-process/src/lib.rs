//! Zephium's WebKitGTK web-process plugin, the only channel between extension
//! code and the browser on Linux.
//!
//! WebKitGTK loads it into every web process from the directory the browser
//! passes to `webkit_web_context_set_web_extensions_directory`. It owns one
//! isolated script world per enabled extension, injects that extension's
//! content scripts there, and gives each extension world (and the main world
//! of `chrome-extension://<id>/` pages) one native function that sends a
//! bounded string to the browser. No script message handler exists, so there
//! is no raw handler to call around it. The reasoning is in the port's
//! docs/extensions.md.

#[cfg(all(unix, not(target_os = "macos")))]
mod channel;
#[cfg(all(unix, not(target_os = "macos")))]
mod ffi;
pub mod matching;
#[cfg(all(unix, not(target_os = "macos")))]
mod worlds;

/// Name of the message a world sends to the browser. Parameters `(ssts)`:
/// extension ID, sender kind, frame ID, body. The reply's parameters are `(s)`.
pub const SEND_MESSAGE: &str = "zephium-webext.send";
/// Sender kind of a content script's isolated world.
pub const KIND_CONTENT: &str = "content";
/// Sender kind of an extension's own page (background, popup, options).
pub const KIND_PAGE: &str = "page";
/// The global, in each extension world, through which messages leave it.
pub const SEND_FUNCTION: &str = "__zephiumWebextSend";
/// Initialization data type: the per-message byte limit, then for each
/// enabled extension its ID, content-script match patterns and scripts.
pub const INIT_DATA_TYPE: &str = "(ua(sasas))";

#[cfg(all(unix, not(target_os = "macos")))]
mod plugin {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use glib::translate::FromGlibPtrNone;

    use crate::ffi::WebKitWebExtension;
    use crate::{worlds, INIT_DATA_TYPE};

    /// What the browser enabled for this web process at its start.
    pub(crate) struct Plugin {
        pub(crate) extension: *mut WebKitWebExtension,
        pub(crate) message_limit: usize,
        pub(crate) extensions: HashMap<String, Extension>,
    }

    pub(crate) struct Extension {
        pub(crate) matches: Vec<String>,
        pub(crate) scripts: Vec<String>,
        pub(crate) world: worlds::World,
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
        let Some((limit, enabled)) = data.get::<(u32, Vec<(String, Vec<String>, Vec<String>)>)>() else {
            return;
        };
        let extensions = enabled
            .into_iter()
            .filter(|(id, _, _)| is_extension_id(id))
            .map(|(id, matches, scripts)| {
                let world = worlds::World::new(&id);
                (id, Extension { matches, scripts, world })
            })
            .collect();
        PLUGIN.with(|plugin| {
            *plugin.borrow_mut() = Some(Plugin { extension, message_limit: limit as usize, extensions });
        });
        worlds::watch_main_world();
    }

    /// Chrome extension IDs: 32 letters from `a` to `p`.
    pub(crate) fn is_extension_id(id: &str) -> bool {
        id.len() == 32 && id.bytes().all(|byte| (b'a'..=b'p').contains(&byte))
    }
}
