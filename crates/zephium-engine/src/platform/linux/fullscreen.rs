//! Element fullscreen of tabs on WebKitGTK. WebKit reports a page entering
//! and leaving fullscreen through two signals; the state is kept on the view,
//! so the host reads it again when the change reaches it, as on the other
//! platforms. WebKit's own handling then fills the toplevel window, which the
//! shell's layout follows by giving the page the whole window. While a page
//! is fullscreen, Escape on the window asks it to leave, wherever the
//! keyboard focus is.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib::prelude::ObjectExt;
use gtk::prelude::{Cast, WidgetExt};
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
    escape: EscapeKey,
}

impl Drop for FullscreenObserver {
    fn drop(&mut self) {
        for signal in self.signals.drain(..) {
            self.view.disconnect(signal);
        }
        release(&self.escape);
    }
}

/// The window's Escape handler while the page is fullscreen.
type EscapeKey = Rc<RefCell<Option<(gtk::Window, gtk::glib::SignalHandlerId)>>>;

fn listen_for_escape(view: &WebView, escape: &EscapeKey) {
    release(escape);
    let Some(window) = view.toplevel().and_then(|top| top.downcast::<gtk::Window>().ok()) else {
        return;
    };
    let page = view.clone();
    let signal = window.connect_key_press_event(move |_, event| {
        if event.keyval() != gtk::gdk::keys::constants::Escape {
            return gtk::glib::Propagation::Proceed;
        }
        trace("Escape while fullscreen");
        ask_to_leave(&page);
        gtk::glib::Propagation::Stop
    });
    *escape.borrow_mut() = Some((window, signal));
}

fn release(escape: &EscapeKey) {
    if let Some((window, signal)) = escape.borrow_mut().take() {
        window.disconnect(signal);
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
    let changed = Rc::new(changed);
    let escape: EscapeKey = Rc::default();
    let (on_enter, entered_escape) = (changed.clone(), escape.clone());
    let enter = view.connect_enter_fullscreen(move |view| {
        trace("entering");
        set_state(view, true);
        listen_for_escape(view, &entered_escape);
        on_enter();
        // Not handled here: WebKit goes on to fill the window.
        false
    });
    let left_escape = escape.clone();
    let leave = view.connect_leave_fullscreen(move |view| {
        trace("leaving");
        set_state(view, false);
        release(&left_escape);
        changed();
        false
    });
    Ok(FullscreenObserver { view, signals: vec![enter, leave], escape })
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
    ask_to_leave(&view.webview());
}

/// The request, from the private world; the page's own world is the
/// fallback when that one cannot run it.
fn ask_to_leave(view: &WebView) {
    const LEAVE: &str = "document.fullscreenElement ? (document.exitFullscreen(), true) : false";
    let page = view.clone();
    view.evaluate_javascript(LEAVE, Some(EXIT_WORLD), None, None::<&gtk::gio::Cancellable>, move |result| {
        if let Err(error) = result {
            trace(&format!("leaving from the private world failed: {error}"));
            page.evaluate_javascript(LEAVE, None, None, None::<&gtk::gio::Cancellable>, |result| {
                if let Err(error) = result {
                    trace(&format!("leaving from the page's world failed: {error}"));
                }
            });
        }
    });
}

fn trace(step: &str) {
    if std::env::var_os("ZEPHIUM_WEBEXT_TRACE").is_some() {
        eprintln!("fullscreen: {step}");
    }
}
