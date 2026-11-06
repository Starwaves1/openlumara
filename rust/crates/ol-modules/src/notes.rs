//! Port of modules/notes/module.py

use crate::util::{py_type_name, read_text};
use async_trait::async_trait;
use indexmap::IndexMap;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use ol_core::pyjson::py_str;
use ol_core::storage::{temporary, StorageDict, StorageType};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[path = "../../../generated/modules/notes.rs"]
pub mod meta;

pub struct Notes {
    base: ModuleBase,
    data: Mutex<Option<StorageDict>>,
}

impl Notes {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base, data: Mutex::new(None) }
    }
}

const NO_CATEGORY: &str = "category doesn't exist. use get_categories first!";

/// `self.data[category]` used as a dict (`.keys()` etc.)
fn as_dict<'a>(v: &'a Value) -> anyhow::Result<&'a Map<String, Value>> {
    v.as_object().ok_or_else(|| anyhow::anyhow!("'{}' object has no attribute 'keys'", py_type_name(v)))
}

/// python core.storage.StorageDict.load() for the markdown kind. ol-core's own loader skips unreadable
/// files and drops symlinked notes, so the notes module reads the folder itself:
/// - os.walk semantics: symlinks to files count as notes, symlinks to folders are not followed
/// - text is read as utf-8 with universal newlines, and a bad file fails the whole load
fn load_markdown(root: &str) -> anyhow::Result<Map<String, Value>> {
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .flatten()
        .filter(|e| !e.path().is_dir() && e.path().to_string_lossy().ends_with(".md"))
        .map(|e| e.path().to_path_buf())
        .collect();
    files.sort();
    let mut flat = Vec::new();
    for p in files {
        let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().to_string();
        let key = ol_core::paths::normpath(Path::new(&rel[..rel.len() - 3]))
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let content = read_text(&p.to_string_lossy()).map_err(|e| anyhow::anyhow!(e))?;
        flat.push((key, Value::String(content)));
    }
    Ok(parse_nested_keys(flat))
}

/// python _parse_nested_keys: "a/b/c" becomes {"a": {"b": {"c": ...}}}
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

/// python StorageDict.load(): replaces the in-memory notes with what is on disk
fn reload(data: &mut StorageDict) -> anyhow::Result<()> {
    let nested = load_markdown(&data.path)?;
    data.load_data(nested);
    Ok(())
}

/// python StorageDict.get() reloads first, unless TEMPORARY or override_temporary is set
fn reload_on_get(data: &mut StorageDict) -> anyhow::Result<()> {
    if !temporary() && !data.override_temporary {
        reload(data)?;
    }
    Ok(())
}

/// python save() writes every note with f.write(content), so a note that is not a str raises TypeError
fn save_notes(data: &mut StorageDict) -> anyhow::Result<()> {
    if let Some(bad) = first_non_str(&data.data) {
        let name = if bad.is_null() { "None" } else { py_type_name(bad) };
        anyhow::bail!("write() argument must be str, not {}", name);
    }
    data.save()
}

fn first_non_str(m: &Map<String, Value>) -> Option<&Value> {
    for v in m.values() {
        match v {
            Value::Object(inner) => {
                if let Some(found) = first_non_str(inner) {
                    return Some(found);
                }
            }
            Value::String(_) => {}
            other => return Some(other),
        }
    }
    None
}

fn recursive_items(data: &Map<String, Value>, prefix: &str, out: &mut Vec<(String, Value)>) {
    for (key, value) in data {
        let current = if prefix.is_empty() { key.clone() } else { format!("{}/{}", prefix, key) };
        if let Value::Object(m) = value {
            recursive_items(m, &current, out);
        } else {
            out.push((current, value.clone()));
        }
    }
}

/// the notes tools, operating on a storage dict
pub fn run(base: &ModuleBase, data: &mut StorageDict, method: &str, args: &ToolArgs) -> anyhow::Result<Value> {
    let s = |k: &str| args.str(k).unwrap_or_else(|| "None".into());
    match method {
        "create" => {
            let (name, category, content) = (s("name"), s("category"), args.raw("content").cloned().unwrap_or(Value::Null));
            if !data.data.contains_key(&category) {
                data.data.insert(category.clone(), json!({}));
            }
            if as_dict(&data.data[&category])?.contains_key(&name) {
                return Ok(base.result(json!("note already exists! edit it instead. make sure to read it before editing it."), false));
            }
            data.data[&category].as_object_mut().unwrap().insert(name, content);
            save_notes(data)?;
            Ok(base.result(json!("note created"), true))
        }
        "read" => {
            let (category, name) = (s("category"), s("name"));
            if !data.data.contains_key(&category) {
                return Ok(base.result(json!(NO_CATEGORY), false));
            }
            let cat = as_dict(&data.data[&category])?;
            match cat.get(&name) {
                None => Ok(base.result(json!("note does not exist"), false)),
                Some(v) => Ok(base.result(v.clone(), true)),
            }
        }
        "edit" => {
            let (category, name, content) = (s("category"), s("name"), args.raw("content").cloned().unwrap_or(Value::Null));
            if !data.data.contains_key(&category) {
                return Ok(base.result(json!(NO_CATEGORY), false));
            }
            if !as_dict(&data.data[&category])?.contains_key(&name) {
                return Ok(base.result(json!("note does not exist"), false));
            }
            data.data[&category].as_object_mut().unwrap().insert(name, content);
            save_notes(data)?;
            Ok(base.result(json!("note edited"), true))
        }
        "get_categories" => Ok(base.result(json!(data.data.keys().collect::<Vec<_>>()), true)),
        "list" => {
            let category = s("category");
            if !data.data.contains_key(&category) {
                return Ok(base.result(json!(NO_CATEGORY), false));
            }
            // python: self.data.get(category, {}) (StorageDict.get reloads from disk)
            reload_on_get(data)?;
            let v = data.data.get(&category).cloned().unwrap_or(json!({}));
            let keys: Vec<String> = as_dict(&v)?.keys().cloned().collect();
            Ok(base.result(json!(keys), true))
        }
        "search" => {
            let query = s("query");
            reload(data)?;
            let mut items = Vec::new();
            recursive_items(&data.data, "", &mut items);
            let entries: Vec<Value> = items.into_iter().map(|(k, v)| json!({"path": k, "content": py_str(&v)})).collect();
            let mut weights = IndexMap::new();
            weights.insert("path".to_string(), 2.0);
            weights.insert("content".to_string(), 1.0);
            let results = ol_core::search::search(&entries, &query, "id", Some(&weights), 10);
            if results.is_empty() {
                return Ok(base.result(json!(format!("no notes found matching '{}'", query)), true));
            }
            let mut lines = Vec::new();
            for hit in results {
                let entry = &hit.entry;
                let mut line = format!("{} (score {:.2})", py_str(&entry["path"]), hit.score);
                let snippets = ol_core::search::make_snippets(entry["content"].as_str().unwrap_or(""), &query, 1, 50);
                if let Some(first) = snippets.first() {
                    line.push_str(&format!("\n  {}", first));
                }
                lines.push(line);
            }
            Ok(base.result(json!(lines.join("\n")), true))
        }
        "delete" => {
            let (category, name) = (s("category"), s("name"));
            if !data.data.contains_key(&category) {
                return Ok(base.result(json!(NO_CATEGORY), false));
            }
            if !as_dict(&data.data[&category])?.contains_key(&name) {
                return Ok(base.result(json!("note does not exist"), false));
            }
            data.data[&category].as_object_mut().unwrap().shift_remove(&name);
            // remove category if it's empty now
            if data.data[&category].as_object().map(|m| m.is_empty()).unwrap_or(false) {
                data.data.shift_remove(&category);
            }
            save_notes(data)?;
            Ok(base.result(json!("note deleted"), true))
        }
        _ => anyhow::bail!("'Notes' object has no attribute '{}'", method),
    }
}

#[async_trait]
impl Module for Notes {
    fn base(&self) -> &ModuleBase {
        &self.base
    }
    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        let mut data = StorageDict::new("notes", StorageType::Markdown, None)?;
        // python: StorageDict.__init__ loads the folder unless TEMPORARY
        if !temporary() {
            reload(&mut data)?;
        }
        *self.data.lock() = Some(data);
        Ok(())
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        let mut guard = self.data.lock();
        let data = guard.as_mut().ok_or_else(|| anyhow::anyhow!("'Notes' object has no attribute 'data'"))?;
        run(&self.base, data, method, &args)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> ToolArgs {
        ToolArgs::new(v.as_object().unwrap().clone())
    }

    #[test]
    fn crud_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        let mut d = StorageDict::new("notes", StorageType::Markdown, Some(&p)).unwrap();
        let base = ModuleBase::detached("notes");
        let r = run(&base, &mut d, "create", &args(json!({"name": "groceries", "category": "home", "content": "buy vitamins and apples"}))).unwrap();
        assert_eq!(r, json!({"status": "success", "content": "note created"}));
        assert!(dir.path().join("notes/home/groceries.md").exists());
        let r = run(&base, &mut d, "create", &args(json!({"name": "groceries", "category": "home", "content": "x"}))).unwrap();
        assert_eq!(r["status"], "error");
        let r = run(&base, &mut d, "read", &args(json!({"category": "home", "name": "groceries"}))).unwrap();
        assert_eq!(r["content"], "buy vitamins and apples");
        let r = run(&base, &mut d, "read", &args(json!({"category": "nope", "name": "groceries"}))).unwrap();
        assert_eq!(r, json!({"status": "error", "content": NO_CATEGORY}));
        let r = run(&base, &mut d, "list", &args(json!({"category": "home"}))).unwrap();
        assert_eq!(r["content"], json!(["groceries"]));
        let r = run(&base, &mut d, "get_categories", &args(json!({}))).unwrap();
        assert_eq!(r["content"], json!(["home"]));
        let r = run(&base, &mut d, "search", &args(json!({"query": "vitamin"}))).unwrap();
        let text = r["content"].as_str().unwrap();
        assert!(text.starts_with("home/groceries (score "), "{}", text);
        assert!(text.contains("\n  buy vitamins and apples"), "{}", text);
        let r = run(&base, &mut d, "search", &args(json!({"query": "zzz"}))).unwrap();
        assert_eq!(r["content"], "no notes found matching 'zzz'");
        let r = run(&base, &mut d, "edit", &args(json!({"category": "home", "name": "groceries", "content": "pears"}))).unwrap();
        assert_eq!(r["content"], "note edited");
        let r = run(&base, &mut d, "delete", &args(json!({"category": "home", "name": "groceries"}))).unwrap();
        assert_eq!(r["content"], "note deleted");
        assert!(d.data.is_empty());
        assert!(!dir.path().join("notes/home").exists());
    }

    #[test]
    fn error_strings_and_category_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        let mut d = StorageDict::new("notes", StorageType::Markdown, Some(&p)).unwrap();
        let base = ModuleBase::detached("notes");
        run(&base, &mut d, "create", &args(json!({"name": "a", "category": "work", "content": "one"}))).unwrap();
        let r = run(&base, &mut d, "create", &args(json!({"name": "b", "category": "work", "content": "two"}))).unwrap();
        assert_eq!(r, json!({"status": "success", "content": "note created"}));
        let missing = json!({"status": "error", "content": "note does not exist"});
        assert_eq!(run(&base, &mut d, "read", &args(json!({"category": "work", "name": "zz"}))).unwrap(), missing);
        assert_eq!(run(&base, &mut d, "edit", &args(json!({"category": "work", "name": "zz", "content": "x"}))).unwrap(), missing);
        assert_eq!(run(&base, &mut d, "delete", &args(json!({"category": "work", "name": "zz"}))).unwrap(), missing);
        let no_cat = json!({"status": "error", "content": NO_CATEGORY});
        assert_eq!(run(&base, &mut d, "edit", &args(json!({"category": "nope", "name": "a", "content": "x"}))).unwrap(), no_cat);
        assert_eq!(run(&base, &mut d, "list", &args(json!({"category": "nope"}))).unwrap(), no_cat);
        let r = run(&base, &mut d, "search", &args(json!({"query": "   "}))).unwrap();
        assert_eq!(r["content"], "no notes found matching '   '");
        // the category survives while it still holds notes, and is dropped once empty
        assert_eq!(run(&base, &mut d, "delete", &args(json!({"category": "work", "name": "a"}))).unwrap()["content"], "note deleted");
        assert_eq!(run(&base, &mut d, "get_categories", &args(json!({}))).unwrap()["content"], json!(["work"]));
        assert_eq!(run(&base, &mut d, "delete", &args(json!({"category": "work", "name": "b"}))).unwrap()["content"], "note deleted");
        assert_eq!(run(&base, &mut d, "get_categories", &args(json!({}))).unwrap()["content"], json!([]));
    }

    #[test]
    fn nested_items_flatten_to_slash_paths() {
        let data = json!({"home": {"groceries": "milk"}, "work": {"ideas": {"x": "y"}}});
        let mut items = Vec::new();
        recursive_items(data.as_object().unwrap(), "", &mut items);
        let paths: Vec<&str> = items.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(paths, vec!["home/groceries", "work/ideas/x"]);
        assert_eq!(items[0].1, json!("milk"));
    }

    #[test]
    fn load_reads_text_like_python_and_fails_on_bad_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("notes");
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::write(root.join("home/crlf.md"), b"a\r\nb\rc").unwrap();
        let root_str = root.to_string_lossy().to_string();
        let nested = load_markdown(&root_str).unwrap();
        // python opens notes in text mode: \r\n and lone \r both come back as \n
        assert_eq!(nested["home"]["crlf"], json!("a\nb\nc"));
        // a note that is not utf-8 makes the whole load raise, like python's UnicodeDecodeError
        std::fs::write(root.join("home/bad.md"), b"\xff").unwrap();
        let err = load_markdown(&root_str).unwrap_err();
        assert_eq!(err.to_string(), "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_note_is_loaded_like_os_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("notes");
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::write(dir.path().join("target.md"), "linked body").unwrap();
        std::os::unix::fs::symlink(dir.path().join("target.md"), root.join("home/link.md")).unwrap();
        let nested = load_markdown(&root.to_string_lossy()).unwrap();
        assert_eq!(nested["home"]["link"], json!("linked body"));
    }

    #[test]
    fn non_str_content_raises_like_python_write() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        let mut d = StorageDict::new("notes", StorageType::Markdown, Some(&p)).unwrap();
        let base = ModuleBase::detached("notes");
        // python: f.write(None) raises TypeError from save(), and the value stays in memory
        let err = run(&base, &mut d, "create", &args(json!({"name": "n", "category": "c", "content": null}))).unwrap_err();
        assert_eq!(err.to_string(), "write() argument must be str, not None");
        let err = run(&base, &mut d, "create", &args(json!({"name": "m", "category": "c", "content": 42}))).unwrap_err();
        assert_eq!(err.to_string(), "write() argument must be str, not None");
        // deleting the null note leaves the number note in memory, so the save now names int
        let err = run(&base, &mut d, "delete", &args(json!({"category": "c", "name": "n"}))).unwrap_err();
        assert_eq!(err.to_string(), "write() argument must be str, not int");
    }
}
