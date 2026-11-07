//! Port of core/storage.py: StorageList, StorageDict, StorageText.
//!
//! Files are byte-compatible with the Python implementation where it matters:
//! - json:    `json.dumps(data, indent=2)` (ensure_ascii, Python separators)
//! - yaml:    yaml (key order preserved). Emitter formatting differs cosmetically from PyYAML.
//! - msgpack: maps/arrays/str/int/float/bool/nil (same as msgpack-python packb defaults)
//! - markdown (dict only): one `.md` file per key, nested dicts become folders
//! - json_folder (dict only): one bare `.json` file per key
//! - text

use crate::pyjson;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

static TEMPORARY: AtomicBool = AtomicBool::new(false);

/// core.storage.TEMPORARY: when set, nothing is loaded from or saved to disk (except the config)
pub fn temporary() -> bool {
    TEMPORARY.load(Ordering::Relaxed)
}
pub fn set_temporary(v: bool) {
    TEMPORARY.store(v, Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageType {
    Text,
    Json,
    Yaml,
    Msgpack,
    Markdown,
    JsonFolder,
}

impl StorageType {
    pub fn parse(s: &str) -> Self {
        match s {
            "text" => Self::Text,
            "yaml" => Self::Yaml,
            "msgpack" => Self::Msgpack,
            "markdown" => Self::Markdown,
            "json_folder" => Self::JsonFolder,
            _ => Self::Json,
        }
    }
    fn ext(&self) -> &'static str {
        match self {
            Self::Text => "txt",
            Self::Json | Self::JsonFolder => "json",
            Self::Yaml => "yml",
            Self::Msgpack => "mp",
            Self::Markdown => "md",
        }
    }
    fn binary(&self) -> bool {
        matches!(self, Self::Msgpack)
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn resolve_path(name: &str, path: Option<&str>) -> anyhow::Result<String> {
    let base = match path {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => crate::paths::get_data_path(None)?,
    };
    crate::paths::sandbox_path(&base, Some(name))
}

fn ensure_parent(path: &str) {
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
}

pub fn to_yaml(v: &Value) -> anyhow::Result<String> {
    Ok(serde_yaml::to_string(v)?)
}

pub fn from_yaml(s: &str) -> anyhow::Result<Value> {
    let v: serde_yaml::Value = serde_yaml::from_str(s)?;
    Ok(yaml_to_json(v))
}

fn yaml_to_json(v: serde_yaml::Value) -> Value {
    match v {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                Value::Number(u.into())
            } else {
                serde_json::Number::from_f64(n.as_f64().unwrap_or(0.0))
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            }
        }
        serde_yaml::Value::String(s) => Value::String(s),
        serde_yaml::Value::Sequence(seq) => Value::Array(seq.into_iter().map(yaml_to_json).collect()),
        serde_yaml::Value::Mapping(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                let key = match k {
                    serde_yaml::Value::String(s) => s,
                    other => pyjson::py_str(&yaml_to_json(other)),
                };
                out.insert(key, yaml_to_json(v));
            }
            Value::Object(out)
        }
        serde_yaml::Value::Tagged(t) => yaml_to_json(t.value),
    }
}

pub fn to_msgpack(v: &Value) -> anyhow::Result<Vec<u8>> {
    Ok(rmp_serde::to_vec(v)?)
}

pub fn from_msgpack(b: &[u8]) -> anyhow::Result<Value> {
    Ok(rmp_serde::from_slice(b)?)
}

// ---------------------------------------------------------------------------------------
// StorageList
// ---------------------------------------------------------------------------------------

/// list that handles storage of data. supports a variety of storage formats.
#[derive(Debug)]
pub struct StorageList {
    pub data: Vec<Value>,
    pub path: String,
    pub name: String,
    pub kind: StorageType,
    last_modified: Option<SystemTime>,
}

impl StorageList {
    pub fn new(name: &str, kind: StorageType, path: Option<&str>) -> anyhow::Result<Self> {
        Self::with_autoload(name, kind, path, true)
    }

    pub fn with_autoload(name: &str, kind: StorageType, path: Option<&str>, autoload: bool) -> anyhow::Result<Self> {
        let mut p = resolve_path(name, path)?;
        ensure_parent(&p);
        p.push('.');
        p.push_str(kind.ext());
        let mut s = Self { data: Vec::new(), path: p, name: name.to_string(), kind, last_modified: None };
        if Path::new(&s.path).exists() {
            if autoload && !temporary() {
                s.load();
            }
        } else {
            s.save();
        }
        Ok(s)
    }

    fn file_changed(&self) -> bool {
        match mtime(Path::new(&self.path)) {
            Some(m) => Some(m) != self.last_modified,
            None => true,
        }
    }

    fn update_mtime(&mut self) {
        if let Some(m) = mtime(Path::new(&self.path)) {
            self.last_modified = Some(m);
        }
    }

    /// save content to file
    pub fn save(&mut self) -> bool {
        if temporary() {
            return true;
        }
        let arr = Value::Array(self.data.clone());
        let res: anyhow::Result<()> = (|| {
            match self.kind {
                StorageType::Json | StorageType::JsonFolder | StorageType::Markdown => {
                    std::fs::write(&self.path, pyjson::dumps_indent(&arr, 2))?
                }
                StorageType::Yaml => std::fs::write(&self.path, to_yaml(&arr)?)?,
                StorageType::Msgpack => std::fs::write(&self.path, to_msgpack(&arr)?)?,
                StorageType::Text => {
                    if !self.data.is_empty() {
                        let lines: Vec<String> = self.data.iter().map(pyjson::py_str).collect();
                        std::fs::write(&self.path, lines.join("\n"))?
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = res {
            crate::log("error", &format!("error writing {}: {}", self.name, e));
            return false;
        }
        self.update_mtime();
        true
    }

    /// replace content with the given data (python: load(data=...))
    pub fn load_data(&mut self, data: Vec<Value>) {
        self.data = data;
    }

    /// load content from file (skipped if the file hasn't changed on disk)
    pub fn load(&mut self) {
        if !self.file_changed() {
            self.update_mtime();
            return;
        }
        self.data.clear();
        let parsed: anyhow::Result<Option<Value>> = (|| {
            if self.kind.binary() {
                let bytes = std::fs::read(&self.path)?;
                if bytes.is_empty() {
                    return Ok(None);
                }
                Ok(Some(from_msgpack(&bytes)?))
            } else {
                let text = std::fs::read_to_string(&self.path)?;
                if text.is_empty() {
                    return Ok(None);
                }
                Ok(Some(match self.kind {
                    StorageType::Yaml => from_yaml(&text)?,
                    StorageType::Text => Value::Array(text.split('\n').map(|l| Value::String(l.into())).collect()),
                    _ => serde_json::from_str(&text)?,
                }))
            }
        })();
        match parsed {
            Ok(Some(Value::Array(a))) => self.data = a,
            Ok(Some(Value::Null)) | Ok(None) => return,
            Ok(Some(_)) => {}
            Err(e) => {
                crate::log("error", &format!("error reading {}: {}", self.name, e));
                return;
            }
        }
        self.update_mtime();
    }

    /// reload from disk (unless temporary) and return item at index
    pub fn get(&mut self, index: usize) -> Option<&Value> {
        if !temporary() {
            self.load();
        }
        self.data.get(index)
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

// ---------------------------------------------------------------------------------------
// StorageDict
// ---------------------------------------------------------------------------------------

/// dict that handles storage of data. supports a variety of storage formats.
#[derive(Debug)]
pub struct StorageDict {
    pub data: Map<String, Value>,
    pub path: String,
    pub name: String,
    pub kind: StorageType,
    pub override_temporary: bool,
    last_modified: Option<SystemTime>,
}

fn folder_filename(key: &str) -> String {
    let safe: String = key
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '.' || c == ' ' || c == '-' { c } else { '_' })
        .take(80)
        .collect();
    format!("{}.json", safe)
}

fn flatten_nested_keys(nested: &Map<String, Value>, prefix: &str, out: &mut Vec<(String, Value)>) {
    for (k, v) in nested {
        let full = if prefix.is_empty() { k.clone() } else { format!("{}/{}", prefix, k) };
        if let Value::Object(m) = v {
            flatten_nested_keys(m, &full, out);
        } else {
            out.push((full, v.clone()));
        }
    }
}

fn parse_nested_keys(flat: Vec<(String, Value)>) -> Map<String, Value> {
    let mut result = Map::new();
    for (key, value) in flat {
        let norm = key.replace('\\', "/");
        let parts: Vec<&str> = norm.split('/').collect();
        let mut current = &mut result;
        for part in &parts[..parts.len() - 1] {
            let entry = current.entry(part.to_string()).or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            current = entry.as_object_mut().unwrap();
        }
        current.insert(parts[parts.len() - 1].to_string(), value);
    }
    result
}

impl StorageDict {
    pub fn new(name: &str, kind: StorageType, path: Option<&str>) -> anyhow::Result<Self> {
        Self::with_options(name, kind, path, true, false)
    }

    pub fn with_options(
        name: &str,
        kind: StorageType,
        path: Option<&str>,
        autoload: bool,
        override_temporary: bool,
    ) -> anyhow::Result<Self> {
        let mut p = resolve_path(name, path)?;
        ensure_parent(&p);
        if !matches!(kind, StorageType::Markdown | StorageType::JsonFolder) {
            p.push('.');
            p.push_str(kind.ext());
        }
        let mut s = Self {
            data: Map::new(),
            path: p,
            name: name.to_string(),
            kind,
            override_temporary,
            last_modified: None,
        };
        if Path::new(&s.path).exists() {
            if autoload && !(temporary() && !override_temporary) {
                s.load();
            }
        } else {
            let _ = s.save();
        }
        Ok(s)
    }

    fn disk_mtime(&self) -> Option<SystemTime> {
        if self.kind == StorageType::JsonFolder {
            let mut latest = SystemTime::UNIX_EPOCH;
            let rd = std::fs::read_dir(&self.path).ok()?;
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with(".json") {
                    if let Some(m) = mtime(&e.path()) {
                        latest = latest.max(m);
                    }
                }
            }
            return Some(latest);
        }
        mtime(Path::new(&self.path))
    }

    fn file_changed(&self) -> bool {
        match self.disk_mtime() {
            Some(m) => Some(m) != self.last_modified,
            None => true,
        }
    }

    fn update_mtime(&mut self) {
        if let Some(m) = self.disk_mtime() {
            self.last_modified = Some(m);
        }
    }

    fn delete_nested_key(&mut self, flat_key: &str) {
        let norm = flat_key.replace('\\', "/");
        let parts: Vec<&str> = norm.split('/').collect();
        let mut current = &mut self.data;
        for part in &parts[..parts.len() - 1] {
            match current.get_mut(*part) {
                Some(Value::Object(m)) => current = m,
                _ => return,
            }
        }
        current.shift_remove(parts[parts.len() - 1]);
    }

    /// save content to file
    pub fn save(&mut self) -> anyhow::Result<()> {
        if temporary() && !self.override_temporary {
            return Ok(());
        }
        let obj = Value::Object(self.data.clone());
        let write = |path: &str, bytes: &[u8], name: &str| {
            if let Err(e) = std::fs::write(path, bytes) {
                crate::log("error", &format!("error writing {}: {}", name, e));
            }
        };
        match self.kind {
            StorageType::Json => write(&self.path, pyjson::dumps_indent(&obj, 2).as_bytes(), &self.name),
            StorageType::Yaml => write(&self.path, to_yaml(&obj)?.as_bytes(), &self.name),
            StorageType::Msgpack => write(&self.path, &to_msgpack(&obj)?, &self.name),
            StorageType::Text => {
                if !self.data.is_empty() {
                    let keys: Vec<String> = self.data.keys().cloned().collect();
                    write(&self.path, keys.join("\n").as_bytes(), &self.name)
                }
            }
            StorageType::Markdown => {
                std::fs::create_dir_all(&self.path)?;
                let mut flat = Vec::new();
                flatten_nested_keys(&self.data, "", &mut flat);
                let mut failed: Vec<(String, String)> = Vec::new();
                let mut kept: Vec<String> = Vec::new();
                for (key, content) in flat {
                    let name = match crate::paths::sandbox_path(&self.path, Some(&format!("{}.md", key))) {
                        Ok(n) => n,
                        Err(e) => {
                            self.delete_nested_key(&key);
                            failed.push((key, e.to_string()));
                            continue;
                        }
                    };
                    ensure_parent(&name);
                    std::fs::write(&name, pyjson::py_str(&content))?;
                    kept.push(key);
                }
                if !failed.is_empty() {
                    let lines: Vec<String> = failed.iter().map(|(k, e)| format!("- {}: {}", k, e)).collect();
                    anyhow::bail!(
                        "Failed to save the following keys due to validation errors:\n{}",
                        lines.join("\n")
                    );
                }
                // remove files that were deleted, and empty directories
                let mut dirs: Vec<PathBuf> = Vec::new();
                for entry in walkdir::WalkDir::new(&self.path).contents_first(true).into_iter().flatten() {
                    let p = entry.path().to_path_buf();
                    if entry.file_type().is_dir() {
                        dirs.push(p);
                        continue;
                    }
                    let fname = p.to_string_lossy().to_string();
                    if fname.ends_with(".md") {
                        let rel = p.strip_prefix(&self.path).unwrap_or(&p).to_string_lossy().to_string();
                        let logical = crate::paths::normpath(Path::new(&rel[..rel.len() - 3]))
                            .to_string_lossy()
                            .replace(std::path::MAIN_SEPARATOR, "/");
                        if !kept.contains(&logical) {
                            let _ = std::fs::remove_file(&p);
                        }
                    }
                }
                for d in dirs {
                    if d != Path::new(&self.path) {
                        if let Ok(mut rd) = std::fs::read_dir(&d) {
                            if rd.next().is_none() {
                                let _ = std::fs::remove_dir(&d);
                            }
                        }
                    }
                }
            }
            StorageType::JsonFolder => {
                std::fs::create_dir_all(&self.path)?;
                let mut written = Vec::new();
                for (k, v) in &self.data {
                    let fname = folder_filename(k);
                    std::fs::write(Path::new(&self.path).join(&fname), pyjson::dumps_indent(v, 2))?;
                    written.push(fname);
                }
                for e in std::fs::read_dir(&self.path)?.flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    if n.ends_with(".json") && !written.contains(&n) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
        self.update_mtime();
        Ok(())
    }

    /// replace content with the given data (python: load(data=...))
    pub fn load_data(&mut self, data: Map<String, Value>) {
        self.data = data;
    }

    /// load content from file or folder
    pub fn load(&mut self) -> bool {
        if !matches!(self.kind, StorageType::Markdown | StorageType::JsonFolder) && !self.file_changed() {
            self.update_mtime();
            return true;
        }
        self.data.clear();

        if self.kind == StorageType::JsonFolder {
            let mut names: Vec<String> = std::fs::read_dir(&self.path)
                .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
                .unwrap_or_default();
            names.sort();
            for n in names {
                if !n.ends_with(".json") {
                    continue;
                }
                match std::fs::read_to_string(Path::new(&self.path).join(&n))
                    .map_err(anyhow::Error::from)
                    .and_then(|t| serde_json::from_str::<Value>(&t).map_err(anyhow::Error::from))
                {
                    Ok(v) => {
                        self.data.insert(n[..n.len() - 5].to_string(), v);
                    }
                    Err(e) => crate::log("error", &format!("error reading {}/{}: {}", self.name, n, e)),
                }
            }
            self.update_mtime();
            return true;
        }

        if self.kind == StorageType::Markdown {
            let mut flat = Vec::new();
            let mut entries: Vec<PathBuf> = walkdir::WalkDir::new(&self.path)
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
                .map(|e| e.path().to_path_buf())
                .collect();
            entries.sort();
            for p in entries {
                let s = p.to_string_lossy().to_string();
                if !s.ends_with(".md") {
                    continue;
                }
                let rel = p.strip_prefix(&self.path).unwrap_or(&p).to_string_lossy().to_string();
                let key = crate::paths::normpath(Path::new(&rel[..rel.len() - 3]))
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                if let Ok(content) = std::fs::read_to_string(&p) {
                    flat.push((key, Value::String(content)));
                }
            }
            self.data = parse_nested_keys(flat);
            self.update_mtime();
            return true;
        }

        let parsed: anyhow::Result<Option<Value>> = (|| {
            if self.kind.binary() {
                let bytes = std::fs::read(&self.path)?;
                if bytes.is_empty() {
                    return Ok(None);
                }
                Ok(Some(from_msgpack(&bytes)?))
            } else {
                let text = std::fs::read_to_string(&self.path)?;
                if text.is_empty() {
                    return Ok(None);
                }
                Ok(Some(match self.kind {
                    StorageType::Yaml => from_yaml(&text)?,
                    StorageType::Text => {
                        let mut m = Map::new();
                        for l in text.split('\n') {
                            m.insert(l.to_string(), Value::Null);
                        }
                        Value::Object(m)
                    }
                    _ => serde_json::from_str(&text)?,
                }))
            }
        })();
        match parsed {
            Ok(Some(Value::Object(m))) => self.data = m,
            Ok(Some(_)) => {}
            Ok(None) => return false,
            Err(e) => {
                crate::log("error", &format!("error reading {}: {}", self.name, e));
                return false;
            }
        }
        self.update_mtime();
        true
    }

    /// dict.get() with reload-from-disk semantics of the python class
    pub fn get(&mut self, key: &str) -> Option<&Value> {
        if !temporary() && !self.override_temporary {
            self.load();
        }
        self.data.get(key)
    }

    pub fn set(&mut self, key: &str, value: Value) {
        self.data.insert(key.to_string(), value);
    }

    /// ranked text search over this dict's keys and (recursively flattened) values.
    /// returns ranked (key, score) tuples
    pub fn search(&self, query: &str, top_n: usize) -> Vec<(String, f64)> {
        let entries: Vec<Value> = self
            .data
            .iter()
            .map(|(k, v)| serde_json::json!({"key": k, "value": flatten_text(v)}))
            .collect();
        let mut weights = indexmap::IndexMap::new();
        weights.insert("key".to_string(), 1.0);
        weights.insert("value".to_string(), 1.0);
        crate::search::search(&entries, query, "id", Some(&weights), top_n)
            .into_iter()
            .map(|r| (r.entry["key"].as_str().unwrap_or("").to_string(), r.score))
            .collect()
    }
}

/// recursively joins all strings inside nested dicts/lists so nested storages (like notes) stay searchable.
pub fn flatten_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(m) => m.values().map(flatten_text).collect::<Vec<_>>().join(" \n "),
        Value::Array(a) => a.iter().map(flatten_text).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------------------
// StorageText
// ---------------------------------------------------------------------------------------

/// simple class that saves its content to a text file
#[derive(Debug)]
pub struct StorageText {
    pub path: String,
    data: String,
    last_modified: Option<SystemTime>,
}

impl StorageText {
    pub fn new(name: &str, path: Option<&str>) -> anyhow::Result<Self> {
        let p = resolve_path(name, path)?;
        ensure_parent(&p);
        let mut s = Self { path: p, data: String::new(), last_modified: None };
        if Path::new(&s.path).exists() {
            if !temporary() {
                s.load();
            }
        } else {
            s.save();
        }
        Ok(s)
    }

    pub fn set(&mut self, new_data: &str) {
        self.data = new_data.to_string();
        self.save();
    }

    pub fn get(&mut self) -> String {
        if !temporary() {
            self.load();
        }
        self.data.clone()
    }

    pub fn load(&mut self) {
        let m = mtime(Path::new(&self.path));
        if m.is_some() && m == self.last_modified {
            return;
        }
        match std::fs::read_to_string(&self.path) {
            Ok(t) => self.data = t,
            Err(e) => crate::log("error", &format!("error while loading text storage: {}", e)),
        }
        self.last_modified = mtime(Path::new(&self.path));
    }

    pub fn save(&mut self) {
        if temporary() {
            return;
        }
        if let Err(e) = std::fs::write(&self.path, &self.data) {
            crate::log("error", &format!("error writing {}: {}", self.path, e));
        }
        self.last_modified = mtime(Path::new(&self.path));
    }

    /// searches the text for your query and returns a snippet if found
    pub fn search(&mut self, query: &str, radius: usize) -> Option<String> {
        let text = self.get();
        if text.is_empty() {
            return None;
        }
        crate::search::make_snippets(&text, query, 1, radius).into_iter().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dict_json_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let base = crate::paths::realpath(dir.path()).to_string_lossy().to_string();
        let mut d = StorageDict::new("x", StorageType::Json, Some(&base)).unwrap();
        d.set("b", json!(1));
        d.set("a", json!({"z": "é"}));
        d.save().unwrap();
        let text = std::fs::read_to_string(format!("{}/x.json", base)).unwrap();
        assert_eq!(text, "{\n  \"b\": 1,\n  \"a\": {\n    \"z\": \"\\u00e9\"\n  }\n}");
        let mut d2 = StorageDict::new("x", StorageType::Json, Some(&base)).unwrap();
        assert_eq!(d2.get("a"), Some(&json!({"z": "é"})));
        assert_eq!(d2.data.keys().collect::<Vec<_>>(), vec!["b", "a"]);
    }

    #[test]
    fn list_msgpack_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let base = crate::paths::realpath(dir.path()).to_string_lossy().to_string();
        let mut l = StorageList::new("chats/web/index", StorageType::Msgpack, Some(&base)).unwrap();
        l.data.push(json!({"id": "ABC", "tags": [], "token_usage": 0, "x": 1.5}));
        l.save();
        let l2 = StorageList::new("chats/web/index", StorageType::Msgpack, Some(&base)).unwrap();
        assert_eq!(l2.data, l.data);
    }

    #[test]
    fn markdown_nested() {
        let dir = tempfile::tempdir().unwrap();
        let base = crate::paths::realpath(dir.path()).to_string_lossy().to_string();
        let mut d = StorageDict::new("notes", StorageType::Markdown, Some(&base)).unwrap();
        d.set("ideas", json!({"topic": "hello"}));
        d.set("todo", json!("x"));
        d.save().unwrap();
        assert!(Path::new(&format!("{}/notes/ideas/topic.md", base)).exists());
        let mut d2 = StorageDict::new("notes", StorageType::Markdown, Some(&base)).unwrap();
        d2.load();
        assert_eq!(d2.data.get("ideas"), Some(&json!({"topic": "hello"})));
        d2.data.shift_remove("ideas");
        d2.save().unwrap();
        assert!(!Path::new(&format!("{}/notes/ideas", base)).exists());
    }

    #[test]
    fn yaml_roundtrip() {
        let v = json!({"api": {"url": "http://x/v1", "max_context": 16768, "t": 0.7, "flag": true, "l": []}});
        let y = to_yaml(&v).unwrap();
        assert_eq!(from_yaml(&y).unwrap(), v);
        // PyYAML-written content parses
        let py = "core:\n  data_folder: data\n  module_libraries: []\nmodel:\n  temperature: 0.7\n  preloaded_modules:\n  - identity\n  - memory\n";
        let v = from_yaml(py).unwrap();
        assert_eq!(v["model"]["preloaded_modules"], json!(["identity", "memory"]));
    }
}
