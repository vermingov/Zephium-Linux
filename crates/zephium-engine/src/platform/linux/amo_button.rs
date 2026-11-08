//! addons.mozilla.org's "Download Firefox" button as Zephium's own install
//! button. The page script (amo_page.js) runs in a script world of its own on
//! the store's top frame; a click opens the add-on file, which
//! `addon_download` fetches and offers for review.

use webkit2gtk::{
    UserContentInjectedFrames, UserContentManagerExt, UserScript, UserScriptInjectionTime, WebView, WebViewExt,
};

const WORLD: &str = "zephium-amo";
const STORE_PAGES: &str = "https://addons.mozilla.org/*";
const PAGE_SCRIPT: &str = include_str!("amo_page.js");

/// Give `view`'s visits to the store the install button.
pub(crate) fn install(view: &WebView) {
    let Some(manager) = view.user_content_manager() else {
        return;
    };
    let script = UserScript::for_world(
        PAGE_SCRIPT,
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::End,
        WORLD,
        &[STORE_PAGES],
        &[],
    );
    manager.add_script(&script);
}
