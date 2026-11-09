//! Blocking `webRequest`: a page's request is offered to the extensions that
//! listen for it before it leaves the web process.
//!
//! WebKitGTK's one hook into a page's requests is `WebKitWebPage::send-request`,
//! raised here and answered on the spot: true cancels the request, a new URI
//! redirects it. The listeners run in an extension's background, another
//! process, so the plugin asks the browser and waits for the answer. WebKit's
//! own messages cannot carry that answer: their replies arrive on this
//! process's main loop, which the wait blocks. The browser therefore serves a
//! socket for this alone and passes its path in the configuration; the path
//! is the one thing the sandbox adds, read-only.
//!
//! The configuration also says which addresses anyone listens for. Every
//! other request, and every request of an extension's own pages, goes out
//! without asking. A browser that does not answer in time lets the request
//! go: a stuck extension must not stop the web.
//!
//! One question per line, tab-separated: page ID, frame ID, resource type,
//! method, URL, the page's address. One answer per line: `allow`, `cancel`, or
//! `redirect` and a URL. WebKit hands out URLs normalized, so they hold no
//! tab or newline; a request whose fields would is let through unasked.

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void, CStr, CString};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use glib::ffi::gboolean;
use glib::gobject_ffi::g_signal_connect_data;

use crate::ffi::*;
use crate::matching::any_matches;
use crate::plugin::PLUGIN;
use crate::EXTENSION_SCHEMES;

/// How long a page waits for the extensions before its request goes out
/// unchanged. The browser gives up sooner and answers `allow`.
const WAIT: Duration = Duration::from_secs(3);
/// The longest answer read back: a redirect to a long URL.
const ANSWER_LIMIT: u64 = 64 * 1024;

/// Where to ask and about which addresses, from the browser's configuration.
#[derive(Clone, Default)]
pub(crate) struct Interception {
    pub(crate) socket: String,
    pub(crate) patterns: Vec<String>,
}

enum Answer {
    Allow,
    Cancel,
    Redirect(CString),
}

thread_local! {
    static CONNECTION: RefCell<Option<BufReader<UnixStream>>> = const { RefCell::new(None) };
    /// Numbers for the frames this process loads; WebKit does not say which
    /// frame a request comes from.
    static NEXT_FRAME: Cell<u64> = const { Cell::new(1) };
}

pub(crate) unsafe fn watch(page: *mut WebKitWebPage) {
    g_signal_connect_data(
        page.cast(),
        c"send-request".as_ptr(),
        Some(std::mem::transmute::<
            unsafe extern "C" fn(*mut WebKitWebPage, *mut WebKitURIRequest, *mut c_void, *mut c_void) -> gboolean,
            unsafe extern "C" fn(),
        >(send_request)),
        std::ptr::null_mut(),
        None,
        0,
    );
}

unsafe extern "C" fn send_request(
    page: *mut WebKitWebPage,
    request: *mut WebKitURIRequest,
    _redirected_response: *mut c_void,
    _data: *mut c_void,
) -> gboolean {
    let Some(url) = owned(webkit_uri_request_get_uri(request)) else {
        return 0;
    };
    let socket = PLUGIN.with(|plugin| {
        let plugin = plugin.borrow();
        let interception = &plugin.as_ref()?.interception;
        let wanted = !interception.socket.is_empty() && any_matches(&interception.patterns, &url);
        wanted.then(|| interception.socket.clone())
    });
    let Some(socket) = socket else {
        return 0;
    };
    let document = owned(webkit_web_page_get_uri(page)).unwrap_or_default();
    if EXTENSION_SCHEMES.iter().any(|scheme| document.starts_with(scheme)) {
        return 0;
    }
    let headers = webkit_uri_request_get_http_headers(request);
    let destination = header(headers, c"Sec-Fetch-Dest");
    let kind = resource_type(destination.as_deref(), header(headers, c"Accept").as_deref(), &url);
    let frame = if kind == "sub_frame" { NEXT_FRAME.with(|next| next.replace(next.get() + 1)) } else { 0 };
    let method = owned(webkit_uri_request_get_http_method(request)).unwrap_or_else(|| "GET".to_owned());
    let page_id = webkit_web_page_get_id(page);
    let fields = [method.as_str(), url.as_str(), document.as_str()];
    if fields.iter().any(|field| field.contains(['\t', '\n', '\r'])) {
        return 0;
    }
    let question = format!("{page_id}\t{frame}\t{kind}\t{method}\t{url}\t{document}\n");
    crate::trace(format_args!("asks about {kind} {url} (destination {destination:?})"));
    match ask(&socket, &question) {
        Answer::Allow => 0,
        Answer::Cancel => {
            crate::trace(format_args!("cancelled {url}"));
            1
        }
        Answer::Redirect(target) => {
            crate::trace(format_args!("redirected {url} to {target:?}"));
            webkit_uri_request_set_uri(request, target.as_ptr());
            0
        }
    }
}

/// The browser's answer to `question`, over the process's one connection.
/// A connection that fails or times out is dropped, so a late answer never
/// meets the next question; a write that fails on a connection left from
/// before is tried once more on a fresh one.
fn ask(socket: &str, question: &str) -> Answer {
    CONNECTION.with(|slot| {
        let mut slot = slot.borrow_mut();
        for _ in 0..2 {
            if slot.is_none() {
                *slot = connect(socket);
            }
            let Some(connection) = slot.as_mut() else {
                return Answer::Allow;
            };
            if connection.get_mut().write_all(question.as_bytes()).is_err() {
                *slot = None;
                continue;
            }
            let mut line = String::new();
            let read = connection.by_ref().take(ANSWER_LIMIT).read_line(&mut line);
            if !matches!(read, Ok(length) if length > 0 && line.ends_with('\n')) {
                crate::trace(format_args!("no answer from the browser: {read:?}"));
                *slot = None;
                return Answer::Allow;
            }
            return parse(line.trim_end_matches('\n'));
        }
        Answer::Allow
    })
}

fn connect(socket: &str) -> Option<BufReader<UnixStream>> {
    let stream = UnixStream::connect(socket)
        .inspect_err(|error| crate::trace(format_args!("cannot reach {socket}: {error}")))
        .ok()?;
    stream.set_read_timeout(Some(WAIT)).ok()?;
    stream.set_write_timeout(Some(WAIT)).ok()?;
    Some(BufReader::new(stream))
}

fn parse(line: &str) -> Answer {
    match line.split_once('\t') {
        Some(("redirect", target)) => CString::new(target).map_or(Answer::Allow, Answer::Redirect),
        _ if line == "cancel" => Answer::Cancel,
        _ => Answer::Allow,
    }
}

/// The request's `webRequest.ResourceType`, Firefox's names. WebKit sets
/// `Sec-Fetch-Dest` on what a page loads; without it, the `Accept` header and
/// the address's file name are the hints left.
pub(crate) fn resource_type(destination: Option<&str>, accept: Option<&str>, url: &str) -> &'static str {
    match destination {
        Some("document") => return "main_frame",
        Some("iframe" | "frame") => return "sub_frame",
        Some("script" | "worker" | "sharedworker" | "serviceworker" | "audioworklet" | "paintworklet") => return "script",
        Some("style") => return "stylesheet",
        Some("image") => return "image",
        Some("font") => return "font",
        Some("audio" | "video" | "track") => return "media",
        Some("object" | "embed") => return "object",
        Some("xslt") => return "xslt",
        Some("report") => return "csp_report",
        Some("manifest") => return "web_manifest",
        Some("empty") => return "xmlhttprequest",
        _ => {}
    }
    let accept = accept.unwrap_or_default();
    if accept.starts_with("text/css") {
        return "stylesheet";
    }
    if accept.starts_with("image/") {
        return "image";
    }
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let extension = path.rsplit_once('.').map(|(_, extension)| extension.to_ascii_lowercase()).unwrap_or_default();
    match extension.as_str() {
        "js" | "mjs" => "script",
        "css" => "stylesheet",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" | "ico" | "bmp" => "image",
        "woff" | "woff2" | "ttf" | "otf" | "eot" => "font",
        "mp4" | "webm" | "m4a" | "mp3" | "ogg" | "m3u8" | "ts" => "media",
        _ => "other",
    }
}

unsafe fn header(headers: *mut SoupMessageHeaders, name: &CStr) -> Option<String> {
    if headers.is_null() {
        return None;
    }
    let value = soup_message_headers_get_one(headers, name.as_ptr());
    (!value.is_null()).then(|| CStr::from_ptr(value).to_string_lossy().into_owned())
}

unsafe fn owned(raw: *const c_char) -> Option<String> {
    (!raw.is_null()).then(|| CStr::from_ptr(raw).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::{parse, resource_type, Answer};

    #[test]
    fn fetch_metadata_names_the_type() {
        assert_eq!(resource_type(Some("document"), None, "https://a.test/"), "main_frame");
        assert_eq!(resource_type(Some("iframe"), None, "https://a.test/"), "sub_frame");
        assert_eq!(resource_type(Some("script"), None, "https://a.test/x"), "script");
        assert_eq!(resource_type(Some("empty"), None, "https://a.test/api"), "xmlhttprequest");
    }

    #[test]
    fn without_fetch_metadata_the_headers_and_file_name_guess() {
        assert_eq!(resource_type(None, Some("text/css,*/*;q=0.1"), "https://a.test/s"), "stylesheet");
        assert_eq!(resource_type(None, Some("image/webp,*/*"), "https://a.test/p"), "image");
        assert_eq!(resource_type(None, Some("*/*"), "https://a.test/ads.js?v=1"), "script");
        assert_eq!(resource_type(None, None, "https://a.test/data"), "other");
    }

    #[test]
    fn answers() {
        assert!(matches!(parse("cancel"), Answer::Cancel));
        assert!(matches!(parse("allow"), Answer::Allow));
        assert!(matches!(parse("redirect\tchrome-extension://x/noop.js"), Answer::Redirect(_)));
        assert!(matches!(parse("garbage"), Answer::Allow));
    }
}
