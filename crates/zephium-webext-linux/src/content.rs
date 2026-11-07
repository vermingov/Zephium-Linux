//! The content scripts the web-process plugin runs, read from a prepared
//! package. Preparation already put the API layer first in each group's
//! scripts.

use std::fs;
use std::path::{Component, Path};

use zephium_webext::manifest::{Manifest, RunAt, World};

/// A script larger than this is refused rather than copied into every web
/// process that starts.
const SCRIPT_LIMIT: u64 = 8 * 1024 * 1024;

/// One `content_scripts` entry with its files' text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub matches: Vec<String>,
    pub exclude_matches: Vec<String>,
    pub js: Vec<String>,
    pub css: Vec<String>,
    /// `document_start`, `document_end` or `document_idle`.
    pub run_at: &'static str,
    pub all_frames: bool,
}

/// The package's isolated-world content scripts. Scripts for the page's own
/// world (`"world": "MAIN"`) are left out in this round.
pub fn groups(package: &Path, manifest: &Manifest) -> Result<Vec<Group>, String> {
    manifest
        .content_scripts()
        .into_iter()
        .filter(|script| script.world == World::Isolated)
        .map(|script| {
            Ok(Group {
                js: read_all(package, &script.js)?,
                css: read_all(package, &script.css)?,
                matches: script.matches,
                exclude_matches: script.exclude_matches,
                run_at: match script.run_at {
                    RunAt::DocumentStart => "document_start",
                    RunAt::DocumentEnd => "document_end",
                    RunAt::DocumentIdle => "document_idle",
                },
                all_frames: script.all_frames,
            })
        })
        .collect()
}

fn read_all(package: &Path, paths: &[String]) -> Result<Vec<String>, String> {
    paths.iter().map(|path| read(package, path)).collect()
}

fn read(package: &Path, path: &str) -> Result<String, String> {
    let relative = Path::new(path);
    if relative.components().any(|part| !matches!(part, Component::Normal(_))) {
        return Err(format!("content script path {path:?} leaves the package"));
    }
    let file = package.join(relative);
    let size = fs::metadata(&file).map_err(|error| format!("{path}: {error}"))?.len();
    if size > SCRIPT_LIMIT {
        return Err(format!("{path} is larger than a content script may be"));
    }
    fs::read_to_string(&file).map_err(|error| format!("{path}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(manifest: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("manifest.json"), manifest).unwrap();
        for (path, text) in files {
            fs::write(dir.path().join(path), text).unwrap();
        }
        dir
    }

    #[test]
    fn groups_carry_their_files_text_and_timing() {
        let dir = package(
            r#"{"manifest_version": 3, "name": "x", "content_scripts": [
                {"matches": ["<all_urls>"], "js": ["a.js"], "css": ["a.css"], "run_at": "document_start", "all_frames": true},
                {"matches": ["https://x.test/*"], "js": ["b.js"], "world": "MAIN"}]}"#,
            &[("a.js", "one()"), ("a.css", "p{}"), ("b.js", "two()")],
        );
        let manifest = Manifest::load(dir.path()).unwrap();
        let groups = groups(dir.path(), &manifest).unwrap();
        assert_eq!(groups.len(), 1, "main-world scripts wait for a later round");
        assert_eq!(groups[0].js, ["one()"]);
        assert_eq!(groups[0].css, ["p{}"]);
        assert_eq!(groups[0].run_at, "document_start");
        assert!(groups[0].all_frames);
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let dir = package(
            r#"{"manifest_version": 3, "name": "x", "content_scripts": [{"matches": ["<all_urls>"], "js": ["gone.js"]}]}"#,
            &[],
        );
        let manifest = Manifest::load(dir.path()).unwrap();
        assert!(groups(dir.path(), &manifest).is_err());
    }

    #[test]
    fn paths_cannot_leave_the_package() {
        let dir = package(r#"{"manifest_version": 3, "name": "x"}"#, &[]);
        assert!(read(dir.path(), "../secret.js").is_err());
        assert!(read(dir.path(), "/etc/passwd").is_err());
    }
}
