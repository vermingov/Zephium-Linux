//! `cookies`: the profile's cookies, through WebKit's cookie manager, for an
//! extension with the `cookies` permission and host access to each cookie's
//! site. One cookie store per profile, as Chrome has outside private windows.

use std::ffi::{c_void, CString};

use gtk::glib::translate::{FromGlibPtrContainer, ToGlibPtr};
use serde_json::{json, Value};
use webkit2gtk::{CookieManager, CookieManagerExt, WebContextExt, WebsiteDataManagerExt};
use zephium_core::ids::ProfileId;
use zephium_webext_process::matching::any_matches;

use super::routing::{error, ok};
use crate::platform::imp::extension_channel::Message;

/// Chrome's one store outside private windows.
const STORE_ID: &str = "0";

// `get_all_cookies` (WebKitGTK 2.42) has no binding yet.
unsafe extern "C" {
    fn webkit_cookie_manager_get_all_cookies(
        manager: *mut webkit2gtk::ffi::WebKitCookieManager,
        cancellable: *mut c_void,
        callback: gtk::gio::ffi::GAsyncReadyCallback,
        user_data: *mut c_void,
    );
    fn webkit_cookie_manager_get_all_cookies_finish(
        manager: *mut webkit2gtk::ffi::WebKitCookieManager,
        result: *mut gtk::gio::ffi::GAsyncResult,
        error: *mut *mut gtk::glib::ffi::GError,
    ) -> *mut gtk::glib::ffi::GList;
}

type Cookies = Box<dyn FnOnce(Vec<soup::Cookie>)>;

/// The cookies WebKit would send to `url`, or every cookie with no URL.
fn read(manager: &CookieManager, url: Option<&str>, done: impl FnOnce(Vec<soup::Cookie>) + 'static) {
    let done: *mut Cookies = Box::into_raw(Box::new(Box::new(done)));
    let raw: *mut webkit2gtk::ffi::WebKitCookieManager = manager.to_glib_none().0;
    // SAFETY: the box goes to exactly one callback, which takes it back.
    unsafe {
        match url.and_then(|url| CString::new(url).ok()) {
            Some(url) => webkit2gtk::ffi::webkit_cookie_manager_get_cookies(raw, url.as_ptr(), std::ptr::null_mut(), Some(read_some), done.cast()),
            None => webkit_cookie_manager_get_all_cookies(raw, std::ptr::null_mut(), Some(read_all), done.cast()),
        }
    }
}

unsafe extern "C" fn read_some(source: *mut gtk::glib::gobject_ffi::GObject, result: *mut gtk::gio::ffi::GAsyncResult, data: *mut c_void) {
    let list = webkit2gtk::ffi::webkit_cookie_manager_get_cookies_finish(source.cast(), result, std::ptr::null_mut());
    deliver(list, data);
}

unsafe extern "C" fn read_all(source: *mut gtk::glib::gobject_ffi::GObject, result: *mut gtk::gio::ffi::GAsyncResult, data: *mut c_void) {
    let list = webkit_cookie_manager_get_all_cookies_finish(source.cast(), result, std::ptr::null_mut());
    deliver(list, data);
}

unsafe fn deliver(list: *mut gtk::glib::ffi::GList, data: *mut c_void) {
    let done = Box::from_raw(data.cast::<Cookies>());
    // SAFETY: both calls hand over the list and its cookies.
    let cookies: Vec<soup::Cookie> = FromGlibPtrContainer::<*mut soup::ffi::SoupCookie, *mut gtk::glib::ffi::GList>::from_glib_full(list);
    done(cookies);
}

/// A cookie as the API gives it.
fn describe(cookie: &mut soup::Cookie) -> Value {
    let domain = cookie.domain().map(|domain| domain.to_string()).unwrap_or_default();
    let expires = cookie.expires().map(|date| date.to_unix());
    let same_site = match cookie.same_site_policy() {
        soup::SameSitePolicy::Lax => "lax",
        soup::SameSitePolicy::Strict => "strict",
        soup::SameSitePolicy::None => "no_restriction",
        _ => "unspecified",
    };
    let mut value = json!({
        "name": cookie.name().map(|name| name.to_string()).unwrap_or_default(),
        "value": cookie.value().map(|value| value.to_string()).unwrap_or_default(),
        "domain": domain,
        "hostOnly": !domain.starts_with('.'),
        "path": cookie.path().map(|path| path.to_string()).unwrap_or_else(|| "/".into()),
        "secure": cookie.is_secure(),
        "httpOnly": cookie.is_http_only(),
        "sameSite": same_site,
        "session": expires.is_none(),
        "storeId": STORE_ID,
    });
    if let Some(expires) = expires {
        value["expirationDate"] = json!(expires);
    }
    value
}

/// The `https://` address of a cookie's site, to check host access against.
fn site_of(cookie: &Value) -> String {
    let host = cookie["domain"].as_str().unwrap_or_default().trim_start_matches('.');
    format!("https://{host}/")
}

/// Whether `cookie` is on `domain` or one of its subdomains, as `getAll`
/// filters.
fn on_domain(cookie: &Value, domain: &str) -> bool {
    let host = cookie["domain"].as_str().unwrap_or_default().trim_start_matches('.');
    let domain = domain.trim_start_matches('.');
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// The policy `cookies.set` asks for; `None` keeps libsoup's Lax, as Chrome
/// treats an unspecified one. libsoup drops a SameSite=None cookie without
/// Secure, and Chrome refuses it.
fn same_site(details: &Value) -> Result<Option<soup::SameSitePolicy>, &'static str> {
    match details["sameSite"].as_str() {
        Some("strict") => Ok(Some(soup::SameSitePolicy::Strict)),
        Some("lax") => Ok(Some(soup::SameSitePolicy::Lax)),
        Some("no_restriction") if details["secure"] != true => Err("A cookie with sameSite \"no_restriction\" must be secure."),
        Some("no_restriction") => Ok(Some(soup::SameSitePolicy::None)),
        _ => Ok(None),
    }
}

fn matches_filter(cookie: &Value, filter: &Value) -> bool {
    filter["name"].as_str().is_none_or(|name| cookie["name"] == name)
        && filter["domain"].as_str().is_none_or(|domain| on_domain(cookie, domain))
        && filter["path"].as_str().is_none_or(|path| cookie["path"] == path)
        && filter["secure"].as_bool().is_none_or(|secure| cookie["secure"] == secure)
        && filter["session"].as_bool().is_none_or(|session| cookie["session"] == session)
}

impl super::super::EngineHost {
    /// Answers `message` once WebKit has the cookies.
    pub(super) fn linux_cookies(&mut self, profile: ProfileId, message: Message, id: u64, method: &str, args: &[Value]) {
        let details = args.first().cloned().unwrap_or(Value::Null);
        let prepared = (|| {
            let state = self.linux_extensions.profiles.get(&profile).ok_or("The extension is not running.")?;
            let install = state.install(&message.extension_id).ok_or("The extension is not running.")?;
            if !install.permissions.iter().any(|permission| permission == "cookies") {
                return Err("The extension has no permission to read cookies.".to_owned());
            }
            let manager = state
                .context
                .as_ref()
                .and_then(|context| context.website_data_manager())
                .and_then(|data| data.cookie_manager())
                .ok_or("The profile has no cookie store yet.")?;
            Ok((manager, install.hosts.clone()))
        })();
        let (manager, hosts) = match prepared {
            Ok(prepared) => prepared,
            Err(reason) => return message.reply(&error(id, &reason)),
        };
        let url = details["url"].as_str().map(str::to_owned);
        if let Some(url) = &url {
            if !any_matches(&hosts, url) {
                return message.reply(&error(id, &format!("No host permissions for cookies at url: \"{url}\".")));
            }
        }
        match method {
            "getAllCookieStores" => {
                let tabs: Vec<Value> = self.linux_all_tabs(profile).into_iter().map(|tab| tab["id"].clone()).collect();
                message.reply(&ok(id, json!([{ "id": STORE_ID, "tabIds": tabs }])))
            }
            "get" | "getAll" => {
                let single = method == "get";
                read(&manager, url.as_deref(), move |mut cookies| {
                    let found: Vec<Value> = cookies
                        .iter_mut()
                        .map(describe)
                        .filter(|cookie| any_matches(&hosts, &site_of(cookie)) && matches_filter(cookie, &details))
                        .collect();
                    if single {
                        message.reply(&ok(id, found.into_iter().next().unwrap_or(Value::Null)));
                    } else {
                        message.reply(&ok(id, Value::Array(found)));
                    }
                });
            }
            "set" => {
                let Some(url) = url else {
                    return message.reply(&error(id, "cookies.set needs a url."));
                };
                let Ok(parsed) = url::Url::parse(&url) else {
                    return message.reply(&error(id, "The url is not valid."));
                };
                let domain = details["domain"].as_str().map(str::to_owned).unwrap_or_else(|| parsed.host_str().unwrap_or_default().to_owned());
                let path = details["path"].as_str().unwrap_or("/");
                let mut cookie = soup::Cookie::new(
                    details["name"].as_str().unwrap_or_default(),
                    details["value"].as_str().unwrap_or_default(),
                    &domain,
                    path,
                    -1,
                );
                cookie.set_secure(details["secure"] == true);
                cookie.set_http_only(details["httpOnly"] == true);
                match same_site(&details) {
                    Ok(Some(policy)) => cookie.set_same_site_policy(policy),
                    Ok(None) => {}
                    Err(reason) => return message.reply(&error(id, reason)),
                }
                if let Some(expires) = details["expirationDate"].as_f64().and_then(|seconds| gtk::glib::DateTime::from_unix_utc(seconds as i64).ok()) {
                    cookie.set_expires(&expires);
                }
                let answer = describe(&mut cookie.clone());
                manager.add_cookie(&mut cookie, None::<&gtk::gio::Cancellable>, move |result| match result {
                    Ok(()) => message.reply(&ok(id, answer)),
                    Err(failure) => message.reply(&error(id, &failure.to_string())),
                });
            }
            "remove" => {
                let Some(url) = url else {
                    return message.reply(&error(id, "cookies.remove needs a url."));
                };
                let name = details["name"].as_str().unwrap_or_default().to_owned();
                let deleting = manager.clone();
                read(&manager, Some(&url.clone()), move |cookies| {
                    let mut target = cookies.into_iter().find(|cookie| cookie.clone().name().is_some_and(|found| found == name.as_str()));
                    let Some(mut cookie) = target.take() else {
                        return message.reply(&ok(id, Value::Null));
                    };
                    deleting.delete_cookie(&mut cookie, None::<&gtk::gio::Cancellable>, move |_| {
                        message.reply(&ok(id, json!({ "url": url, "name": name, "storeId": STORE_ID })));
                    });
                });
            }
            _ => message.reply(&error(id, &format!("cookies.{method} is not available on Linux yet."))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_read_as_chrome_documents_them() {
        let cookie = json!({ "name": "sid", "domain": ".example.com", "path": "/", "secure": true, "session": false });
        assert!(matches_filter(&cookie, &json!({ "domain": "example.com" })));
        assert!(matches_filter(&cookie, &json!({ "domain": "www.example.com" })) == false);
        assert!(on_domain(&json!({ "domain": "a.example.com" }), "example.com"));
        assert!(!on_domain(&json!({ "domain": "badexample.com" }), "example.com"));
        assert!(matches_filter(&cookie, &json!({ "name": "sid", "secure": true })));
        assert!(!matches_filter(&cookie, &json!({ "session": true })));
        assert_eq!(site_of(&cookie), "https://example.com/");
    }

    #[test]
    fn same_site_keeps_what_libsoup_stores() {
        assert_eq!(same_site(&json!({ "url": "https://example.com/" })), Ok(None));
        assert_eq!(same_site(&json!({ "sameSite": "strict" })), Ok(Some(soup::SameSitePolicy::Strict)));
        assert!(same_site(&json!({ "sameSite": "no_restriction" })).is_err());
        assert_eq!(same_site(&json!({ "sameSite": "no_restriction", "secure": true })), Ok(Some(soup::SameSitePolicy::None)));
    }
}
