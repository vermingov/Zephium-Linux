//! Extension worlds and what runs in them.
//!
//! WebKit only reports `window-object-cleared` for worlds already used in a
//! frame, and a frame's main world is always used. So the main world's signal
//! drives everything: when a frame gets a new document, the plugin gives an
//! extension page its channel, or creates the matching extensions' worlds in
//! that frame and runs their content scripts.

use std::cell::RefCell;
use std::ffi::{c_void, CStr, CString};

use glib::gobject_ffi::{g_object_unref, g_signal_connect_data};

use crate::channel::{self, Sender};
use crate::{config, events};
use crate::ffi::*;
use crate::matching::any_matches;
use crate::plugin::{is_extension_id, Group, RunAt, PLUGIN};
use crate::{EXTENSION_SCHEMES, KIND_CONTENT, KIND_PAGE};

/// The function the scheduler below calls; it runs the deferred scripts once.
const RUN_FUNCTION: &str = "__zephiumWebextRun";

/// `document_end` scripts run when the document is parsed, `document_idle`
/// ones right after, as Chrome orders them.
const SCHEDULER: &str = r#"(() => {
  const run = globalThis.__zephiumWebextRun;
  const later = () => { run("end"); setTimeout(() => run("idle"), 0); };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", later, { once: true });
  else later();
})();"#;

/// One extension's isolated world, alive as long as the web process.
pub(crate) struct World {
    raw: *mut WebKitScriptWorld,
}

impl World {
    pub(crate) fn raw(&self) -> *mut WebKitScriptWorld {
        self.raw
    }

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
    // The frame's previous document and its listeners are gone.
    events::forget_frame(page_id, frame_id);

    if let Some(rest) = EXTENSION_SCHEMES.iter().find_map(|scheme| uri.strip_prefix(scheme)) {
        let host = rest.split('/').next().unwrap_or_default();
        match info_of(host) {
            Some(info) => {
                let context = webkit_frame_get_js_context_for_script_world(frame, main_world);
                if !context.is_null() {
                    channel::install(context, Sender::new(host, KIND_PAGE, page_id, frame_id), &info, KIND_PAGE);
                    g_object_unref(context.cast());
                }
            }
            // This process may predate the extension: ask, and reload the
            // page here once if it turns out to be enabled.
            None if is_extension_id(host) && ok(webkit_frame_is_main_frame(frame)) => config::request(page, true),
            None => {}
        }
        return;
    }

    let main_frame = ok(webkit_frame_is_main_frame(frame));
    // Copied out first: a content script may call back into the plugin.
    let matching: Vec<(String, *mut WebKitScriptWorld, String, Vec<Group>)> = PLUGIN.with(|plugin| {
        let plugin = plugin.borrow();
        let Some(plugin) = plugin.as_ref() else {
            return Vec::new();
        };
        plugin
            .extensions
            .iter()
            .filter(|(_, extension)| any_matches(&extension.granted, uri))
            .filter_map(|(id, extension)| {
                let groups: Vec<Group> = extension
                    .groups
                    .iter()
                    .filter(|group| group.all_frames || main_frame)
                    .filter(|group| any_matches(&group.matches, uri) && !any_matches(&group.exclude_matches, uri))
                    .cloned()
                    .collect();
                (!groups.is_empty()).then(|| (id.clone(), extension.world.raw, extension.info.clone(), groups))
            })
            .collect()
    });
    for (id, world, info, groups) in matching {
        run_content_scripts(frame, world, Sender::new(&id, KIND_CONTENT, page_id, frame_id), &info, &groups);
    }
}

fn info_of(extension_id: &str) -> Option<String> {
    if !is_extension_id(extension_id) {
        return None;
    }
    PLUGIN.with(|plugin| {
        plugin.borrow().as_ref().and_then(|plugin| plugin.extensions.get(extension_id)).map(|e| e.info.clone())
    })
}

unsafe fn run_content_scripts(
    frame: *mut WebKitFrame,
    world: *mut WebKitScriptWorld,
    sender: Sender,
    info: &str,
    groups: &[Group],
) {
    let context = webkit_frame_get_js_context_for_script_world(frame, world);
    if context.is_null() {
        return;
    }
    channel::install(context, sender, info, KIND_CONTENT);
    let at = |when: RunAt| -> Vec<String> {
        groups.iter().filter(|group| group.run_at == when).flat_map(|group| group.scripts.clone()).collect()
    };
    for script in at(RunAt::Start) {
        evaluate(context, &script);
    }
    let (end, idle) = (at(RunAt::End), at(RunAt::Idle));
    if !end.is_empty() || !idle.is_empty() {
        let deferred = Box::new(Deferred { end: RefCell::new(end), idle: RefCell::new(idle) });
        channel::define_function(context, RUN_FUNCTION, run_deferred, Box::into_raw(deferred).cast(), drop_deferred);
        evaluate(context, SCHEDULER);
    }
    g_object_unref(context.cast());
}

struct Deferred {
    end: RefCell<Vec<String>>,
    idle: RefCell<Vec<String>>,
}

unsafe extern "C" fn drop_deferred(data: *mut c_void) {
    drop(Box::from_raw(data.cast::<Deferred>()));
}

/// `run("end" | "idle")`: each batch runs once, however often it is asked for.
unsafe extern "C" fn run_deferred(args: *mut glib::ffi::GPtrArray, data: *mut c_void) {
    let deferred = &*data.cast::<Deferred>();
    let [when] = arguments(args) else {
        return;
    };
    let context = jsc_value_get_context(*when);
    let batch = match channel::string_of(*when).as_deref() {
        Some("end") => deferred.end.take(),
        Some("idle") => deferred.idle.take(),
        _ => return,
    };
    for script in batch {
        evaluate(context, &script);
    }
}

/// Run `script` at the world's top level, where content scripts share their
/// globals as in Chrome. An exception stays in the script's own world.
unsafe fn evaluate(context: *mut JSCContext, script: &str) {
    let result = jsc_context_evaluate(context, script.as_ptr().cast(), script.len() as isize);
    if !result.is_null() {
        g_object_unref(result.cast());
    }
}
