//! Element fullscreen of tabs on WebKitGTK. WebKit reports a page entering
//! and leaving fullscreen through two signals; the state is kept on the view,
//! so the host reads it again when the change reaches it, as on the other
//! platforms. WebKit's own handling then fills the toplevel window, which the
//! shell's layout follows by giving the page the whole window.

use gtk::glib::prelude::ObjectExt;
use webkit2gtk::{WebView, WebViewExt};
use wry::WebViewExtUnix;

use crate::fullscreen::NativeFullscreen;

/// Where the state lives on the WebView: a `bool`, true while fullscreen.
const STATE_KEY: &str = "zephium-fullscreen";
/// The world the exit request runs in, out of the page's reach.
const EXIT_WORLD: &str = "zephium-fullscreen";

/// Holds the two signal connections for as long as the view lives.
pub(crate) struct FullscreenObserver {
    view: WebView,
    signals: Vec<gtk::glib::SignalHandlerId>,
}

impl Drop for FullscreenObserver {
    fn drop(&mut self) {
        for signal in self.signals.drain(..) {
            self.view.disconnect(signal);
        }
    }
}

fn set_state(view: &WebView, active: bool) {
    // SAFETY: this private key is only ever written and read as `bool`.
    unsafe { view.set_data(STATE_KEY, active) };
}

pub(crate) fn install_fullscreen_observer(view: &wry::WebView, changed: impl Fn() + 'static) -> Result<FullscreenObserver, String> {
    let view = view.webview();
    if let Some(settings) = WebViewExt::settings(&view) {
        use webkit2gtk::SettingsExt;
        settings.set_enable_fullscreen(true);
    }
    set_state(&view, false);
    let changed = std::rc::Rc::new(changed);
    let on_enter = changed.clone();
    let enter = view.connect_enter_fullscreen(move |view| {
        set_state(view, true);
        on_enter();
        // Not handled here: WebKit goes on to fill the window.
        false
    });
    let leave = view.connect_leave_fullscreen(move |view| {
        set_state(view, false);
        changed();
        false
    });
    Ok(FullscreenObserver { view, signals: vec![enter, leave] })
}

pub(crate) fn fullscreen_state(view: &wry::WebView) -> NativeFullscreen {
    // SAFETY: as in `set_state`.
    let active = unsafe { view.webview().data::<bool>(STATE_KEY).map(|state| *state.as_ref()) };
    if active == Some(true) {
        NativeFullscreen::Active
    } else {
        NativeFullscreen::Inactive
    }
}

/// Asks the page to leave fullscreen, from a world of its own so the page
/// cannot replace `exitFullscreen` to keep the screen.
pub(crate) fn exit_fullscreen(view: &wry::WebView) {
    if fullscreen_state(view) != NativeFullscreen::Active {
        return;
    }
    view.webview().evaluate_javascript(
        "document.fullscreenElement && document.exitFullscreen()",
        Some(EXIT_WORLD),
        None,
        None::<&gtk::gio::Cancellable>,
        |_| {},
    );
}
