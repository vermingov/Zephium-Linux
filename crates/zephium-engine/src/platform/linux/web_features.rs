//! The web platform features a modern site expects, where WebKitGTK leaves
//! them off by default: WebRTC (calls, voice chat) and WebGPU.

use std::ffi::{c_char, c_void, CStr};

use gtk::glib::translate::ToGlibPtr;
use webkit2gtk::{SettingsExt, WebView, WebViewExt};

/// Features turned on by their WebKit identifier.
const ENABLED_FEATURES: [&str; 1] = ["WebGPU"];

// WebKitGTK's feature API (2.42) has no Rust binding yet.
unsafe extern "C" {
    fn webkit_settings_get_all_features() -> *mut c_void;
    fn webkit_feature_list_get_length(list: *mut c_void) -> usize;
    fn webkit_feature_list_get(list: *mut c_void, index: usize) -> *mut c_void;
    fn webkit_feature_list_unref(list: *mut c_void);
    fn webkit_feature_get_identifier(feature: *mut c_void) -> *const c_char;
    fn webkit_settings_set_feature_enabled(settings: *mut webkit2gtk::ffi::WebKitSettings, feature: *mut c_void, enabled: i32);
}

pub(crate) fn enable(view: &WebView) {
    let Some(settings) = WebViewExt::settings(view) else {
        return;
    };
    settings.set_enable_webrtc(true);
    // With the extension trace on, a tab's console goes to the log too, for
    // finding why a site does not work. Off otherwise.
    if std::env::var_os("ZEPHIUM_WEBEXT_TRACE").is_some() {
        settings.set_enable_write_console_messages_to_stdout(true);
    }
    let raw_settings: *mut webkit2gtk::ffi::WebKitSettings = settings.to_glib_none().0;
    // SAFETY: the list is owned here until unreffed; features are borrowed
    // from it and identifiers are static strings.
    unsafe {
        let list = webkit_settings_get_all_features();
        if list.is_null() {
            return;
        }
        for index in 0..webkit_feature_list_get_length(list) {
            let feature = webkit_feature_list_get(list, index);
            let identifier = webkit_feature_get_identifier(feature);
            if identifier.is_null() {
                continue;
            }
            let identifier = CStr::from_ptr(identifier).to_str().unwrap_or_default();
            if ENABLED_FEATURES.contains(&identifier) {
                webkit_settings_set_feature_enabled(raw_settings, feature, 1);
            }
        }
        webkit_feature_list_unref(list);
    }
}
