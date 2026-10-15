//! Port of modules/notes/module.py

use crate::util::py_type_name;
use async_trait::async_trait;
use indexmap::IndexMap;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use ol_core::pyjson::py_str;
use ol_core::storage::{StorageDict, StorageType};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};

#[path = "../../../generated/modules/notes.rs"]
pub(crate) mod meta;

pub struct Notes {
    base: ModuleBase,
    data: Mutex<Option<StorageDict>>,
}

impl Notes {
    pub fn new(base: ModuleBase) -> Self {
        Self { base, data: Mutex::new(None) }
    }
}

const NO_CATEGORY: &str = "category doesn't exist. use get_categories first!";

/// `self.data[category]` used as a dict (`.keys()` etc.)
fn as_dict<'a>(v: &'a Value) -> anyhow::Result<&'a Map<String, Value>> {
    v.as_object().ok_or_else(|| anyhow::anyhow!("'{}' object has no attribute 'keys'", py_type_name(v)))
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
            data.save()?;
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
            data.save()?;
            Ok(base.result(json!("note edited"), true))
        }
        "get_categories" => Ok(base.result(json!(data.data.keys().collect::<Vec<_>>()), true)),
        "list" => {
            let category = s("category");
            if !data.data.contains_key(&category) {
                return Ok(base.result(json!(NO_CATEGORY), false));
            }
            // python: self.data.get(category, {}) (StorageDict.get reloads from disk)
            let v = data.get(&category).cloned().unwrap_or(json!({}));
            let keys: Vec<String> = as_dict(&v)?.keys().cloned().collect();
            Ok(base.result(json!(keys), true))
        }
        "search" => {
            let query = s("query");
            data.load();
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
            data.save()?;
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
        *self.data.lock() = Some(StorageDict::new("notes", StorageType::Markdown, None)?);
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
}
