//! Extension pages: `chrome-extension://<id>/<path>`, and `moz-extension://`
//! for the same files, served from the prepared package.
//!
//! A view of the extension's own origin gets every file. Anything else (a web
//! page embedding an extension frame or image) gets only what the manifest
//! lists in `web_accessible_resources`, as in Chrome.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use webkit2gtk::{
    SecurityManagerExt, URISchemeRequest, URISchemeRequestExt, URISchemeResponse, URISchemeResponseExt, WebContext,
    WebContextExt, WebViewExt,
};

pub(super) const SCHEMES: [&str; 2] = ["chrome-extension", "moz-extension"];
/// The page a background without its own page loads: the API layer, then the
/// manifest's scripts or service worker.
pub(super) const BACKGROUND_PATH: &str = "/_zephium_/background.html";
/// Loaded ahead of a service worker run in that page: `importScripts` adds the
/// named scripts to the page while it is still being parsed, so they run in
/// order before the rest of the worker.
pub(super) const WORKER_SHIM_PATH: &str = "/_zephium_/worker-shim.js";
const WORKER_SHIM: &str = r#"globalThis.importScripts = (...urls) => {
  for (const url of urls) document.write(`<script src="${new URL(url, document.baseURI).href}"><\/script>`);
};"#;

/// What the scheme handler needs of one loaded extension.
#[derive(Clone)]
pub(super) struct Package {
    pub(super) root: PathBuf,
    pub(super) csp: String,
    pub(super) accessible: Vec<String>,
    pub(super) background: Option<String>,
}

pub(super) type Packages = Rc<RefCell<HashMap<String, Package>>>;

/// Serve `packages` on `context`; once per context.
pub(super) fn register(context: &WebContext, packages: &Packages) {
    for scheme in SCHEMES {
        let packages = packages.clone();
        context.register_uri_scheme(scheme, move |request| serve(&packages.borrow(), request));
        if let Some(security) = context.security_manager() {
            security.register_uri_scheme_as_secure(scheme);
            security.register_uri_scheme_as_cors_enabled(scheme);
        }
    }
}

fn serve(packages: &HashMap<String, Package>, request: &URISchemeRequest) {
    let uri = request.uri().map(|uri| uri.to_string()).unwrap_or_default();
    let Some((origin, extension_id, path)) = split(&uri) else {
        return fail(request, "not an extension address");
    };
    let Some(package) = packages.get(extension_id) else {
        return fail(request, "no such extension");
    };
    let own = request
        .web_view()
        .and_then(|view| view.uri())
        .is_some_and(|requester| requester.starts_with(&origin));
    super::trace!("request {uri} (from the extension itself: {own})");
    if path == WORKER_SHIM_PATH && own {
        return respond(request, WORKER_SHIM.as_bytes().to_vec(), "text/javascript", None);
    }
    if path == BACKGROUND_PATH {
        return match (&package.background, own) {
            (Some(page), true) => respond(request, page.as_bytes().to_vec(), "text/html", Some(&package.csp)),
            _ => fail(request, "not found"),
        };
    }
    if !own && !package.accessible.iter().any(|pattern| glob(pattern.trim_start_matches('/'), path.trim_start_matches('/'))) {
        return fail(request, "not web accessible");
    }
    match read(&package.root, path) {
        Some(bytes) => {
            let mime = mime(path);
            let csp = (mime == "text/html").then_some(package.csp.as_str());
            respond(request, bytes, mime, csp);
        }
        None => fail(request, "not found"),
    }
}

/// `scheme://id/path?query#fragment` -> (`scheme://id/`, `id`, `/path`); the
/// bare origin is its `/index.html`.
fn split(uri: &str) -> Option<(String, &str, &str)> {
    let (scheme, rest) = uri.split_once("://")?;
    if !SCHEMES.contains(&scheme) {
        return None;
    }
    let (host, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, "/"),
    };
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let path = if path == "/" { "/index.html" } else { path };
    Some((format!("{scheme}://{host}/"), host, path))
}

/// A file of the package, refusing anything that would leave it.
fn read(root: &Path, path: &str) -> Option<Vec<u8>> {
    let relative = Path::new(path.trim_start_matches('/'));
    if relative.components().any(|part| !matches!(part, Component::Normal(_))) {
        return None;
    }
    let file = root.join(relative).canonicalize().ok()?;
    if !file.starts_with(root.canonicalize().ok()?) {
        return None;
    }
    std::fs::read(file).ok()
}

fn respond(request: &URISchemeRequest, bytes: Vec<u8>, mime: &str, csp: Option<&str>) {
    let length = bytes.len() as i64;
    let stream = gtk::gio::MemoryInputStream::from_bytes(&gtk::glib::Bytes::from_owned(bytes));
    let response = URISchemeResponse::new(&stream, length);
    response.set_content_type(mime);
    // These headers replace the response's own, so the type goes in here too:
    // with nosniff, WebKit runs no script whose header lacks a script type.
    let headers = soup::MessageHeaders::new(soup::MessageHeadersType::Response);
    headers.append("Content-Type", mime);
    if let Some(csp) = csp {
        headers.append("Content-Security-Policy", csp);
    }
    headers.append("X-Content-Type-Options", "nosniff");
    response.set_http_headers(headers);
    request.finish_with_response(&response);
}

fn fail(request: &URISchemeRequest, reason: &str) {
    super::trace!("refused {:?}: {reason}", request.uri());
    let mut error = gtk::glib::Error::new(gtk::gio::IOErrorEnum::NotFound, reason);
    request.finish_error(&mut error);
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default().to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// `web_accessible_resources` globs: `*` matches any run of characters.
fn glob(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, tail)) => {
            text.starts_with(head) && (0..=text.len() - head.len()).any(|skip| glob(tail, &text[head.len() + skip..]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_split_into_origin_id_and_path() {
        let (origin, id, path) = split("chrome-extension://abc/popup.html?x=1#y").unwrap();
        assert_eq!((origin.as_str(), id, path), ("chrome-extension://abc/", "abc", "/popup.html"));
        assert_eq!(split("moz-extension://abc").unwrap().2, "/index.html");
        assert!(split("https://abc/").is_none());
    }

    #[test]
    fn accessible_globs() {
        assert!(glob("images/*", "images/a/b.png"));
        assert!(glob("*.css", "x.css"));
        assert!(!glob("images/*", "secret.js"));
    }

    #[test]
    fn files_cannot_leave_the_package() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.js"), "1").unwrap();
        assert_eq!(read(dir.path(), "/a.js").as_deref(), Some(&b"1"[..]));
        assert!(read(dir.path(), "/../etc/passwd").is_none());
    }
}
