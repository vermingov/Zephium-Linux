//! Extension worlds and what runs in them.
//!
//! WebKit only reports `window-object-cleared` for worlds already used in a
//! frame, and a frame's main world is always used. So the main world's signal
//! drives everything: when a frame gets a new document, the plugin gives an
//! extension page its channel, or creates the matching extensions' worlds in
//! that frame and injects their content scripts.

use std::ffi::{c_void, CStr, CString};

use glib::gobject_ffi::{g_object_unref, g_signal_connect_data};

use crate::channel::{self, Sender};
use crate::ffi::*;
use crate::matching::any_matches;
use crate::plugin::{is_extension_id, PLUGIN};
use crate::{KIND_CONTENT, KIND_PAGE};

const EXTENSION_SCHEME: &str = "chrome-extension://";

/// One extension's isolated world, alive as long as the web process.
pub(crate) struct World {
    raw: *mut WebKitScriptWorld,
}

impl World {
    pub(crate) fn new(extension_id: &str) -> Self {
        let name = CString::new(format!("zephium-extension-{extension_id}")).expect("ID has no NUL");
        Self { raw: unsafe { webkit_script_world_new_with_name(name.as_ptr()) } }
    }
}

pub(crate) fn watch_main_world() {
    unsafe {
        g_signal_connect_data(
            webkit_script_world_get_default().cast(),
            c"window-object-cleared".as_ptr(),
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut WebKitScriptWorld, *mut WebKitWebPage, *mut WebKitFrame, *mut c_void),
                unsafe extern "C" fn(),
            >(main_world_cleared)),
            std::ptr::null_mut(),
            None,
            0,
        );
    }
}

unsafe extern "C" fn main_world_cleared(
    main_world: *mut WebKitScriptWorld,
    page: *mut WebKitWebPage,
    frame: *mut WebKitFrame,
    _data: *mut c_void,
) {
    let uri = webkit_frame_get_uri(frame);
    if uri.is_null() {
        return;
    }
    let Ok(uri) = CStr::from_ptr(uri).to_str() else {
        return;
    };
    let page_id = webkit_web_page_get_id(page);
    let frame_id = webkit_frame_get_id(frame);
    if let Some(rest) = uri.strip_prefix(EXTENSION_SCHEME) {
        let host = rest.split('/').next().unwrap_or_default();
        if is_extension_id(host) && enabled(host) {
            let sender = Sender::new(host, KIND_PAGE, page_id, frame_id);
            prepare(frame, main_world, sender, &[]);
        }
        return;
    }
    // Copied out first: a content script may call back into the plugin.
    let matching: Vec<(String, *mut WebKitScriptWorld, Vec<String>)> = PLUGIN.with(|plugin| {
        let plugin = plugin.borrow();
        let Some(plugin) = plugin.as_ref() else {
            return Vec::new();
        };
        plugin
            .extensions
            .iter()
            .filter(|(_, extension)| !extension.scripts.is_empty() && any_matches(&extension.matches, uri))
            .map(|(id, extension)| (id.clone(), extension.world.raw, extension.scripts.clone()))
            .collect()
    });
    for (id, world, scripts) in matching {
        let sender = Sender::new(&id, KIND_CONTENT, page_id, frame_id);
        prepare(frame, world, sender, &scripts);
    }
}

fn enabled(extension_id: &str) -> bool {
    PLUGIN.with(|plugin| {
        plugin.borrow().as_ref().is_some_and(|plugin| plugin.extensions.contains_key(extension_id))
    })
}

/// Give `world` in `frame` its channel, then run `scripts` there.
unsafe fn prepare(frame: *mut WebKitFrame, world: *mut WebKitScriptWorld, sender: Sender, scripts: &[String]) {
    let context = webkit_frame_get_js_context_for_script_world(frame, world);
    if context.is_null() {
        return;
    }
    channel::install(context, sender);
    for script in scripts {
        let result = jsc_context_evaluate(context, script.as_ptr().cast(), script.len() as isize);
        if !result.is_null() {
            g_object_unref(result.cast());
        }
    }
    g_object_unref(context.cast());
}
