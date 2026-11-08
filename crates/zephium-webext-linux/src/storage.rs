//! `chrome.storage`: one JSON object per extension and area. `local` and
//! `sync` are kept on disk in the profile (sync does not leave the machine);
//! `session` lives as long as the browser.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

/// Chrome's quota for `storage.local` without `unlimitedStorage`.
pub const LOCAL_QUOTA: usize = 10 * 1024 * 1024;

pub struct Area {
    file: Option<PathBuf>,
    items: Map<String, Value>,
}

/// What a write changed, as `storage.onChanged` reports it.
pub type Changes = Map<String, Value>;

impl Area {
    /// An area kept in `file`, read back if it exists.
    pub fn on_disk(file: PathBuf) -> io::Result<Self> {
        let items = match fs::read(&file) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(Value::Object(items)) => items,
                _ => Map::new(),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Map::new(),
            Err(error) => return Err(error),
        };
        Ok(Self { file: Some(file), items })
    }

    pub fn in_memory() -> Self {
        Self { file: None, items: Map::new() }
    }

    /// `keys` as Chrome takes it: null for everything, a key, a list of keys,
    /// or an object of keys with defaults.
    pub fn get(&self, keys: &Value) -> Map<String, Value> {
        match keys {
            Value::Null => self.items.clone(),
            Value::String(key) => self.pick([key.as_str()]),
            Value::Array(list) => self.pick(list.iter().filter_map(Value::as_str)),
            Value::Object(defaults) => defaults
                .iter()
                .map(|(key, default)| (key.clone(), self.items.get(key).unwrap_or(default).clone()))
                .collect(),
            _ => Map::new(),
        }
    }

    pub fn set(&mut self, items: Map<String, Value>, quota: Option<usize>) -> Result<Changes, String> {
        // Copied only once something changes: extensions often write what
        // they already stored.
        let mut next: Option<Map<String, Value>> = None;
        let mut changes = Changes::new();
        for (key, value) in items {
            if self.items.get(&key) == Some(&value) {
                continue;
            }
            let old = next.get_or_insert_with(|| self.items.clone()).insert(key.clone(), value.clone());
            changes.insert(key, change(old, Some(value)));
        }
        let Some(next) = next else {
            return Ok(changes);
        };
        let text = serialize(&next)?;
        if quota.is_some_and(|quota| text.len() > quota) {
            return Err("QUOTA_BYTES quota exceeded".into());
        }
        self.commit(next, text)?;
        Ok(changes)
    }

    pub fn remove(&mut self, keys: &Value) -> Result<Changes, String> {
        let keys: Vec<&str> = match keys {
            Value::String(key) => vec![key.as_str()],
            Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !keys.iter().any(|key| self.items.contains_key(*key)) {
            return Ok(Changes::new());
        }
        let mut next = self.items.clone();
        let changes = keys
            .into_iter()
            .filter_map(|key| next.remove(key).map(|old| (key.to_owned(), change(Some(old), None))))
            .collect();
        let text = serialize(&next)?;
        self.commit(next, text)?;
        Ok(changes)
    }

    pub fn clear(&mut self) -> Result<Changes, String> {
        if self.items.is_empty() {
            return Ok(Changes::new());
        }
        let changes = self.items.iter().map(|(key, old)| (key.clone(), change(Some(old.clone()), None))).collect();
        self.commit(Map::new(), b"{}".to_vec())?;
        Ok(changes)
    }

    pub fn bytes_in_use(&self, keys: &Value) -> usize {
        self.get(keys).iter().map(|(key, value)| key.len() + value.to_string().len()).sum()
    }

    fn pick<'a>(&self, keys: impl IntoIterator<Item = &'a str>) -> Map<String, Value> {
        keys.into_iter()
            .filter_map(|key| self.items.get(key).map(|value| (key.to_owned(), value.clone())))
            .collect()
    }

    /// Write first, then keep: a failed write leaves the area as it was.
    fn commit(&mut self, next: Map<String, Value>, text: Vec<u8>) -> Result<(), String> {
        if let Some(file) = &self.file {
            if let Some(parent) = file.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            let staged = file.with_extension("json.new");
            fs::write(&staged, text).map_err(|error| error.to_string())?;
            fs::rename(&staged, file).map_err(|error| error.to_string())?;
        }
        self.items = next;
        Ok(())
    }
}

fn serialize(items: &Map<String, Value>) -> Result<Vec<u8>, String> {
    serde_json::to_vec(items).map_err(|error| error.to_string())
}

fn change(old: Option<Value>, new: Option<Value>) -> Value {
    let mut change = json!({});
    if let Some(old) = old {
        change["oldValue"] = old;
    }
    if let Some(new) = new {
        change["newValue"] = new;
    }
    change
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn get_takes_every_shape_chrome_allows() {
        let mut area = Area::in_memory();
        area.set(object(json!({"a": 1, "b": [2]})), None).unwrap();
        assert_eq!(Value::Object(area.get(&Value::Null)), json!({"a": 1, "b": [2]}));
        assert_eq!(Value::Object(area.get(&json!("a"))), json!({"a": 1}));
        assert_eq!(Value::Object(area.get(&json!(["b", "zz"]))), json!({"b": [2]}));
        assert_eq!(Value::Object(area.get(&json!({"a": 0, "c": 3}))), json!({"a": 1, "c": 3}));
    }

    #[test]
    fn writes_report_only_what_changed() {
        let mut area = Area::in_memory();
        let first = area.set(object(json!({"a": 1})), None).unwrap();
        assert_eq!(Value::Object(first), json!({"a": {"newValue": 1}}));
        let same = area.set(object(json!({"a": 1})), None).unwrap();
        assert!(same.is_empty());
        let removed = area.remove(&json!("a")).unwrap();
        assert_eq!(Value::Object(removed), json!({"a": {"oldValue": 1}}));
    }

    #[test]
    fn the_quota_refuses_the_write_and_keeps_the_area() {
        let mut area = Area::in_memory();
        assert!(area.set(object(json!({"a": "x".repeat(64)})), Some(32)).is_err());
        assert!(area.get(&Value::Null).is_empty());
    }

    #[test]
    fn disk_areas_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ext/local.json");
        Area::on_disk(file.clone()).unwrap().set(object(json!({"k": "v"})), None).unwrap();
        assert_eq!(Value::Object(Area::on_disk(file).unwrap().get(&Value::Null)), json!({"k": "v"}));
    }

    #[test]
    fn unchanged_writes_leave_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("local.json");
        let mut area = Area::on_disk(file.clone()).unwrap();
        area.set(object(json!({"k": "v"})), None).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert!(area.set(object(json!({"k": "v"})), None).unwrap().is_empty());
        assert!(area.remove(&json!("missing")).unwrap().is_empty());
        assert!(!file.exists());
    }
}
