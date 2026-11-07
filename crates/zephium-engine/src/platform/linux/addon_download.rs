//! Firefox add-ons from addons.mozilla.org.
//!
//! The Linux port saves no downloads yet, so every download is cancelled,
//! except a `.xpi` file the person fetches from addons.mozilla.org. That one
//! is saved to a private folder and handed to the browser, which shows the
//! same install review as for a dropped file; nothing installs until the
//! person confirms there.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use webkit2gtk::glib::object::Cast;
use webkit2gtk::{
    Download, DownloadExt, PolicyDecisionExt, PolicyDecisionType, ResponsePolicyDecision, ResponsePolicyDecisionExt, URIRequestExt,
    WebView, WebViewExt,
};
use zephium_core::ids::ProfileId;

const ADDONS_HOST: &str = "addons.mozilla.org";
/// Where addons.mozilla.org sends a file link on to.
const ADDONS_FILES_HOST: &str = "addons.cdn.mozilla.net";
/// The installer's own ceiling for a package.
const MAX_BYTES: u64 = 256 * 1024 * 1024;

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

/// WebKit cannot show an add-on file and would drop it: on `view`, such a
/// response becomes a download, which `take` then keeps.
pub(crate) fn offer_as_download(view: &WebView) {
    view.connect_decide_policy(|_, decision, kind| {
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
        decision.download();
        true
    });
}

/// Takes `download` if it is an add-on file; the caller cancels the rest.
pub(crate) fn take(download: &Download, profile: ProfileId) -> bool {
    let url = download.request().and_then(|request| request.uri()).map(|uri| uri.to_string()).unwrap_or_default();
    trace(format_args!("download of {url} (an add-on file: {})", is_addon_file(&url)));
    if !is_addon_file(&url) {
        return false;
    }
    let Some(path) = destination() else {
        return false;
    };
    let target = url::Url::from_file_path(&path).map(String::from).unwrap_or_default();
    download.connect_decide_destination(move |download, _| {
        trace(format_args!("saving to {target}"));
        download.set_destination(&target);
        true
    });
    download.connect_received_data(|download, _| {
        if download.received_data_length() > MAX_BYTES {
            download.cancel();
        }
    });
    let failed = Rc::new(Cell::new(false));
    let failure = failed.clone();
    let partial = path.clone();
    download.connect_failed(move |_, error| {
        trace(format_args!("failed: {error}"));
        failure.set(true);
        let _ = std::fs::remove_file(&partial);
    });
    // `finished` follows `failed` too.
    download.connect_finished(move |_| {
        trace(format_args!("finished; saved: {}", path.is_file()));
        if failed.get() || !path.is_file() {
            return;
        }
        let path = path.clone();
        let _ = crate::host::try_with(move |host| host.addon_downloaded(profile, path));
    });
    true
}

/// One step of an add-on download, when `ZEPHIUM_WEBEXT_TRACE` is set.
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

/// A name no other download uses: the process and the time.
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
