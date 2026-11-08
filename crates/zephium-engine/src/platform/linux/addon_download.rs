//! Firefox add-ons from addons.mozilla.org.
//!
//! The Linux port saves no downloads, and every view refuses them. A `.xpi`
//! file the person opens from addons.mozilla.org is the one exception: the
//! view drops the response, and the file is fetched again here, saved to a
//! private folder and handed to the browser, which shows the same install
//! review as for a dropped file. Nothing installs until the person confirms
//! there.

use std::io::Write;
use std::path::{Path, PathBuf};

use webkit2gtk::gio::prelude::InputStreamExt;
use webkit2gtk::glib::object::Cast;
use webkit2gtk::{
    PolicyDecisionExt, PolicyDecisionType, ResponsePolicyDecision, ResponsePolicyDecisionExt, URIRequestExt, WebView,
    WebViewExt,
};
use soup::prelude::SessionExt;
use zephium_core::ids::ProfileId;

const ADDONS_HOST: &str = "addons.mozilla.org";
/// Where addons.mozilla.org sends a file link on to.
const ADDONS_FILES_HOST: &str = "addons.cdn.mozilla.net";
/// The installer's own ceiling for a package.
const MAX_BYTES: usize = 256 * 1024 * 1024;

/// Whether `url` is an add-on file on addons.mozilla.org, such as
/// `https://addons.mozilla.org/firefox/downloads/file/123/name-1.0.xpi`, or
/// on the file host it redirects to.
pub(crate) fn is_addon_file(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    let path = url.path();
    let place = match url.host_str() {
        Some(ADDONS_HOST) => path.contains("/downloads/file/"),
        Some(ADDONS_FILES_HOST) => path.starts_with("/user-media/addons/"),
        _ => false,
    };
    url.scheme() == "https" && url.port().is_none() && place && path.ends_with(".xpi")
}

/// On `view` of `profile`, an add-on file the page cannot show is dropped
/// and fetched for review instead.
pub(crate) fn offer_for_review(view: &WebView, profile: ProfileId) {
    view.connect_decide_policy(move |_, decision, kind| {
        if kind != PolicyDecisionType::Response {
            return false;
        }
        let url = decision
            .dynamic_cast_ref::<ResponsePolicyDecision>()
            .and_then(|response| response.request())
            .and_then(|request| request.uri())
            .map(|uri| uri.to_string())
            .unwrap_or_default();
        if !is_addon_file(&url) {
            return false;
        }
        decision.ignore();
        trace(format_args!("fetching {url}"));
        std::thread::spawn(move || match fetch(&url) {
            Ok(path) => {
                webkit2gtk::glib::MainContext::default().invoke(move || {
                    let _ = crate::host::try_with(move |host| host.addon_downloaded(profile, path));
                });
            }
            Err(reason) => trace(format_args!("{url} was not fetched: {reason}")),
        });
        true
    });
}

/// Fetch `url` into a new private file, refusing more than the installer
/// would take.
fn fetch(url: &str) -> Result<PathBuf, String> {
    let session = soup::Session::new();
    let message = soup::Message::new("GET", url).map_err(|error| error.to_string())?;
    let stream = session.send(&message, None::<&webkit2gtk::gio::Cancellable>).map_err(|error| error.to_string())?;
    if message.status() != soup::Status::Ok {
        return Err(format!("the server answered {:?}", message.status()));
    }
    let path = destination().ok_or("no private folder to save to")?;
    let mut file = std::fs::File::create(&path).map_err(|error| error.to_string())?;
    let mut total = 0;
    loop {
        let chunk = stream.read_bytes(64 * 1024, None::<&webkit2gtk::gio::Cancellable>).map_err(|error| error.to_string())?;
        if chunk.is_empty() {
            break;
        }
        total += chunk.len();
        if total > MAX_BYTES {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err("the file is larger than an extension may be".into());
        }
        file.write_all(&chunk).map_err(|error| error.to_string())?;
    }
    Ok(path)
}

/// One step of an add-on fetch, when `ZEPHIUM_WEBEXT_TRACE` is set.
fn trace(step: std::fmt::Arguments<'_>) {
    if std::env::var_os("ZEPHIUM_WEBEXT_TRACE").is_some() {
        eprintln!("webext: add-on {step}");
    }
}

/// A new file in the user's private runtime folder, where nobody else can
/// swap it before the review reads it.
fn destination() -> Option<PathBuf> {
    let folder = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir).join("zephium-addons");
    std::fs::create_dir_all(&folder).ok()?;
    restrict(&folder);
    Some(folder.join(unique_name()))
}

fn restrict(folder: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700));
}

/// A name no other fetch uses: the process and the time.
fn unique_name() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.as_nanos());
    format!("addon-{}-{nanos}.xpi", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::is_addon_file;

    #[test]
    fn only_add_on_files_from_addons_mozilla_org_are_taken() {
        assert!(is_addon_file("https://addons.mozilla.org/firefox/downloads/file/4412/ublock_origin-1.61.xpi"));
        assert!(is_addon_file("https://addons.mozilla.org/en-US/firefox/downloads/file/1/x.xpi"));
        assert!(is_addon_file("https://addons.cdn.mozilla.net/user-media/addons/1/x-1.0.xpi"));
        assert!(!is_addon_file("https://addons.cdn.mozilla.net/static/x.xpi"));
        assert!(!is_addon_file("http://addons.mozilla.org/firefox/downloads/file/1/x.xpi"));
        assert!(!is_addon_file("https://addons.mozilla.org.evil.test/firefox/downloads/file/1/x.xpi"));
        assert!(!is_addon_file("https://addons.mozilla.org/firefox/downloads/file/1/x.exe"));
        assert!(!is_addon_file("https://example.com/downloads/file/1/x.xpi"));
    }
}
