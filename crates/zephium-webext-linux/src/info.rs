//! What an extension context is told when it starts, read synchronously by
//! the API layer: `runtime.id`, `runtime.getURL`, `runtime.getManifest` and
//! `i18n.getMessage` answer without a round trip to the browser.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{json, Map, Value};
use zephium_webext::manifest::Manifest;

/// Where the context runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Context {
    /// The extension's own pages: background, popup, options.
    Page,
    /// A content script, in the extension's world of a web page.
    Content,
}

/// The JSON the plugin exposes as `__zephiumWebextInfo`.
pub fn info(id: &str, base_url: &str, context: Context, package: &Path, manifest: &Manifest) -> String {
    json!({
        "id": id,
        "baseUrl": base_url,
        "context": match context { Context::Page => "page", Context::Content => "content" },
        "manifest": manifest.raw(),
        "messages": messages(package, manifest),
        "uiLanguage": "en",
    })
    .to_string()
}

/// Every message the package defines, resolved the way upstream resolves
/// `__MSG_name__` in the manifest, keyed by its lower-cased name as Chrome
/// looks names up. Placeholders come out as `$1`..`$9`, which the API layer
/// fills from `getMessage`'s substitutions.
fn messages(package: &Path, manifest: &Manifest) -> Map<String, Value> {
    let mut names = BTreeMap::new();
    let Ok(locales) = fs::read_dir(package.join("_locales")) else {
        return Map::new();
    };
    for locale in locales.flatten() {
        let Ok(text) = fs::read_to_string(locale.path().join("messages.json")) else {
            continue;
        };
        let Ok(Value::Object(entries)) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        for name in entries.keys() {
            names.insert(name.to_lowercase(), name.clone());
        }
    }
    names
        .into_iter()
        .filter_map(|(key, name)| {
            let token = format!("__MSG_{name}__");
            let text = manifest.localize(&token);
            (text != token).then(|| (key, Value::String(text)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        dir
    }

    #[test]
    fn messages_are_resolved_and_keyed_in_lower_case() {
        let dir = package(&[
            ("manifest.json", r#"{"manifest_version": 3, "name": "__MSG_appName__", "default_locale": "en"}"#),
            ("_locales/en/messages.json", r#"{"appName": {"message": "Reader"}, "Greeting": {"message": "Hi"}}"#),
        ]);
        let manifest = Manifest::load(dir.path()).unwrap();
        let info: Value = serde_json::from_str(&info("a", "chrome-extension://a/", Context::Page, dir.path(), &manifest)).unwrap();
        assert_eq!(info["messages"]["appname"], "Reader");
        assert_eq!(info["messages"]["greeting"], "Hi");
        assert_eq!(info["context"], "page");
        assert_eq!(info["manifest"]["name"], "__MSG_appName__");
    }

    #[test]
    fn a_package_without_locales_has_no_messages() {
        let dir = package(&[("manifest.json", r#"{"manifest_version": 3, "name": "x"}"#)]);
        let manifest = Manifest::load(dir.path()).unwrap();
        let info: Value = serde_json::from_str(&info("a", "chrome-extension://a/", Context::Content, dir.path(), &manifest)).unwrap();
        assert_eq!(info["messages"], json!({}));
        assert_eq!(info["context"], "content");
    }
}
