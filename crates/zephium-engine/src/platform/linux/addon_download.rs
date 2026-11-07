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

use webkit2gtk::{Download, DownloadExt, URIRequestExt};
use zephium_core::ids::ProfileId;

const ADDONS_HOST: &str = "addons.mozilla.org";
/// The installer's own ceiling for a package.
const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Whether `url` is an add-on file on addons.mozilla.org, such as
/// `https://addons.mozilla.org/firefox/downloads/file/123/name-1.0.xpi`.
pub(crate) fn is_addon_file(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some(ADDONS_HOST)
        && url.port().is_none()
        && url.path().contains("/downloads/file/")
        && url.path().ends_with(".xpi")
}

/// Takes `download` if it is an add-on file; the caller cancels the rest.
pub(crate) fn take(download: &Download, profile: ProfileId) -> bool {
    let url = download.request().and_then(|request| request.uri()).map(|uri| uri.to_string()).unwrap_or_default();
    if !is_addon_file(&url) {
        return false;
    }
    let Some(path) = destination() else {
        return false;
    };
    let target = url::Url::from_file_path(&path).map(String::from).unwrap_or_default();
    download.connect_decide_destination(move |download, _| {
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
    download.connect_failed(move |_, _| {
        failure.set(true);
        let _ = std::fs::remove_file(&partial);
    });
    // `finished` follows `failed` too.
    download.connect_finished(move |_| {
        if failed.get() || !path.is_file() {
            return;
        }
        let path = path.clone();
        let _ = crate::host::try_with(move |host| host.addon_downloaded(profile, path));
    });
    true
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
        assert!(!is_addon_file("http://addons.mozilla.org/firefox/downloads/file/1/x.xpi"));
        assert!(!is_addon_file("https://addons.mozilla.org.evil.test/firefox/downloads/file/1/x.xpi"));
        assert!(!is_addon_file("https://addons.mozilla.org/firefox/downloads/file/1/x.exe"));
        assert!(!is_addon_file("https://example.com/downloads/file/1/x.xpi"));
    }
}
