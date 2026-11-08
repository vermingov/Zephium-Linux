//! `downloads.download` for extensions: a file the extension made or fetched
//! is saved in the user's Downloads folder.
//!
//! Messages from an extension are bounded, so the API layer reads the file
//! in the extension's own context (a blob, data or web URL) and sends it in
//! pieces: `begin` names it, `chunk` adds to it, `finish` saves it. The path
//! stays inside Downloads, and an existing file is never replaced.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use serde_json::{json, Value};
use zephium_core::ids::ProfileId;

use super::routing::{raise, Endpoint};

/// The most one extension may save in one file, and keep pending at once.
const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
const MAX_PENDING: usize = 8;

#[derive(Default)]
pub(super) struct Downloads {
    next: u64,
    pending: HashMap<u64, Pending>,
}

struct Pending {
    path: PathBuf,
    data: Vec<u8>,
}

/// `filename` as a path inside the Downloads folder: relative, without `..`
/// and with the characters other systems refuse replaced.
fn relative_path(filename: &str) -> Result<PathBuf, String> {
    let mut path = PathBuf::new();
    for part in filename.split(['/', '\\']).filter(|part| !part.is_empty()) {
        if part == "." || part == ".." {
            return Err("The file name may not leave the Downloads folder.".into());
        }
        let clean: String = part
            .chars()
            .map(|character| if character.is_control() || "<>:\"|?*".contains(character) { '_' } else { character })
            .collect();
        let clean = clean.trim_end_matches(['.', ' ']).to_owned();
        if clean.is_empty() || clean.len() > 255 {
            return Err("The file name is not valid.".into());
        }
        path.push(clean);
    }
    match path.components().all(|part| matches!(part, Component::Normal(_))) && path.components().next().is_some() {
        true => Ok(path),
        false => Err("The file name is not valid.".into()),
    }
}

/// `wanted`, or the first of `name (1).ext`, `name (2).ext`… not taken.
fn free_path(wanted: &Path) -> PathBuf {
    if !wanted.exists() {
        return wanted.to_owned();
    }
    let stem = wanted.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    let extension = wanted.extension().map(|extension| format!(".{}", extension.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|number| wanted.with_file_name(format!("{stem} ({number}){extension}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or_else(|| wanted.to_owned())
}

impl super::super::EngineHost {
    pub(super) fn linux_downloads(&mut self, profile: ProfileId, extension_id: &str, method: &str, args: &[Value]) -> Result<Value, String> {
        let install = self
            .linux_extensions
            .profiles
            .get_mut(&profile)
            .and_then(|state| state.install_mut(extension_id))
            .ok_or("The extension is not running.")?;
        if !install.permissions.iter().any(|permission| permission == "downloads") {
            return Err("The extension has no permission to download.".into());
        }
        let downloads = &mut install.downloads;
        match method {
            "begin" => {
                if downloads.pending.len() >= MAX_PENDING {
                    return Err("Too many downloads at once.".into());
                }
                let folder = dirs::download_dir().ok_or("There is no Downloads folder.")?;
                let path = folder.join(relative_path(args.first().and_then(Value::as_str).unwrap_or_default())?);
                downloads.next += 1;
                downloads.pending.insert(downloads.next, Pending { path, data: Vec::new() });
                Ok(json!(downloads.next))
            }
            "chunk" => {
                let token = args.first().and_then(Value::as_u64).unwrap_or(0);
                let piece = args.get(1).and_then(Value::as_str).unwrap_or_default();
                let bytes = base64::engine::general_purpose::STANDARD.decode(piece).map_err(|_| "The download data is not valid.")?;
                let pending = downloads.pending.get_mut(&token).ok_or("No such download.")?;
                if pending.data.len() + bytes.len() > MAX_FILE_BYTES {
                    downloads.pending.remove(&token);
                    return Err("The file is larger than an extension may save.".into());
                }
                pending.data.extend_from_slice(&bytes);
                Ok(Value::Null)
            }
            "finish" => {
                let token = args.first().and_then(Value::as_u64).unwrap_or(0);
                let pending = downloads.pending.remove(&token).ok_or("No such download.")?;
                if let Some(parent) = pending.path.parent() {
                    std::fs::create_dir_all(parent).map_err(|failure| failure.to_string())?;
                }
                let path = free_path(&pending.path);
                std::fs::write(&path, &pending.data).map_err(|failure| failure.to_string())?;
                super::trace!("{extension_id} saved {} bytes to {}", pending.data.len(), path.display());
                // The extension listens for the change once it has the ID: tell
                // it a moment after the answer.
                let item = json!({ "id": token, "filename": path.to_string_lossy(), "state": "complete", "fileSize": pending.data.len() });
                let changed = json!({ "id": token, "state": { "previous": "in_progress", "current": "complete" } });
                let (views, extension_id) = (install.own_views(), extension_id.to_owned());
                gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                    for view in views {
                        let endpoint = Endpoint { view, frame: 0 };
                        raise(&endpoint, &extension_id, "downloads.onCreated", json!([item]));
                        raise(&endpoint, &extension_id, "downloads.onChanged", json!([changed]));
                    }
                });
                Ok(json!(token))
            }
            "cancel" => {
                let token = args.first().and_then(Value::as_u64).unwrap_or(0);
                downloads.pending.remove(&token);
                Ok(Value::Null)
            }
            "search" | "erase" => Ok(json!([])),
            _ => Err(format!("downloads.{method} is not available on Linux yet.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_inside_downloads() {
        assert_eq!(relative_path("uisnap/page__x/shot.png").unwrap(), PathBuf::from("uisnap/page__x/shot.png"));
        assert_eq!(relative_path("a:b?.txt").unwrap(), PathBuf::from("a_b_.txt"));
        assert!(relative_path("../etc/passwd").is_err());
        assert!(relative_path("a/../../b").is_err());
        assert!(relative_path("").is_err());
        assert_eq!(relative_path("/abs/file").unwrap(), PathBuf::from("abs/file"));
    }

    #[test]
    fn an_existing_file_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let wanted = dir.path().join("shot.png");
        assert_eq!(free_path(&wanted), wanted);
        std::fs::write(&wanted, b"x").unwrap();
        assert_eq!(free_path(&wanted), dir.path().join("shot (1).png"));
    }
}
