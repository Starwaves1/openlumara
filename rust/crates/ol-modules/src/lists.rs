//! Port of modules/lists/module.py

use crate::util::py_type_name;
use async_trait::async_trait;
use indexmap::IndexMap;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use ol_core::pyjson::{py_str, truthy};
use ol_core::storage::{StorageDict, StorageType};
use parking_lot::Mutex;
use rand::seq::SliceRandom;
use serde_json::{json, Map, Value};

#[path = "../../../generated/modules/lists.rs"]
pub(crate) mod meta;

pub struct Lists {
    base: ModuleBase,
    data: Mutex<Option<StorageDict>>,
}

impl Lists {
    pub fn new(base: ModuleBase) -> Self {
        Self { base, data: Mutex::new(None) }
    }
}

const NO_LIST: &str = "that list doesn't exist";

fn no_attr(v: &Value, attr: &str) -> anyhow::Error {
    anyhow::anyhow!("'{}' object has no attribute '{}'", py_type_name(v), attr)
}

/// the pinned-lists system prompt (python on_system_prompt body)
pub fn system_prompt(data: &Map<String, Value>, max_pinned: &Value) -> anyhow::Result<String> {
    let mut output = String::new();
    let mut pinned_by_cat: IndexMap<String, Vec<(String, Vec<Value>)>> = IndexMap::new();
    let mut unpinned_by_cat: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut count: i64 = 1;

    for (cat, lists) in data.iter().rev() {
        let lists = lists.as_object().ok_or_else(|| no_attr(lists, "items"))?;
        for (name, lst) in lists {
            let over = match max_pinned {
                Value::Number(n) => n.as_f64().map(|m| count as f64 > m).unwrap_or(false),
                Value::Bool(b) => count > *b as i64,
                other => anyhow::bail!("'>' not supported between instances of 'int' and '{}'", py_type_name(other)),
            };
            if over {
                break;
            }
            count += 1;
            let lst = lst.as_object().ok_or_else(|| no_attr(lst, "get"))?;
            let items = lst.get("items").cloned().unwrap_or(Value::Null);
            if !truthy(&items) {
                continue;
            }
            if lst.get("pinned").map(truthy).unwrap_or(false) {
                let items = match items {
                    Value::Array(a) => a,
                    Value::String(s) => s.chars().map(|c| Value::String(c.to_string())).collect(),
                    other => anyhow::bail!("'{}' object is not iterable", py_type_name(&other)),
                };
                pinned_by_cat.entry(cat.clone()).or_default().push((name.clone(), items));
            } else {
                unpinned_by_cat.entry(cat.clone()).or_default().push(name.clone());
            }
        }
    }

    for (cat, items) in &pinned_by_cat {
        output.push_str(&format!("## {}\n", cat));
        for (name, lst_items) in items {
            let lines: Vec<String> = lst_items.iter().map(|it| format!("- {}", py_str(it))).collect();
            output.push_str(&format!("### {}\n{}\n", name, lines.join("\n")));
        }
    }
    if !unpinned_by_cat.is_empty() {
        output.push_str("---\nlists that aren't pinned:\n");
        for (cat, names) in &unpinned_by_cat {
            output.push_str(&format!("{}: {}\n", cat, names.join(", ")));
        }
    }
    Ok(output)
}

fn verify_target(data: &mut StorageDict, category: &str, list_name: &str) -> bool {
    if !data.data.contains_key(category) {
        return false;
    }
    // python: `list_name not in self.data.get(category, {})` (StorageDict.get reloads from disk)
    match data.get(category) {
        Some(Value::Object(m)) => m.contains_key(list_name),
        Some(Value::Array(a)) => a.contains(&Value::String(list_name.to_string())),
        Some(Value::String(s)) => s.contains(list_name),
        _ => false,
    }
}

fn create_if_non_existent(data: &mut StorageDict, category: &str, list_name: &str) -> anyhow::Result<bool> {
    if !verify_target(data, category, list_name) {
        if !data.data.contains_key(category) {
            data.data.insert(category.to_string(), json!({}));
        }
        let cat = data.data.get_mut(category).unwrap();
        let cat = match cat {
            Value::Object(m) => m,
            other => anyhow::bail!("'{}' object does not support item assignment", py_type_name(other)),
        };
        cat.insert(list_name.to_string(), json!({"items": [], "pinned": false}));
        data.save()?;
        return Ok(true);
    }
    Ok(false)
}

fn find_item(items: &[Value], starts_with: &str) -> anyhow::Result<Option<usize>> {
    let needle = starts_with.trim().to_lowercase();
    for (index, item) in items.iter().enumerate() {
        let s = item.as_str().ok_or_else(|| no_attr(item, "strip"))?;
        if s.trim().to_lowercase().starts_with(&needle) {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// `self.data[category][list_name]` as a mutable dict
fn target<'a>(data: &'a mut StorageDict, category: &str, list_name: &str) -> anyhow::Result<&'a mut Map<String, Value>> {
    let lst = data
        .data
        .get_mut(category)
        .and_then(|c| c.as_object_mut())
        .and_then(|c| c.get_mut(list_name))
        .ok_or_else(|| anyhow::anyhow!("{}", crate::util::repr_str(list_name)))?;
    match lst {
        Value::Object(m) => Ok(m),
        other => anyhow::bail!("'{}' object is not subscriptable", py_type_name(other)),
    }
}

/// `target_list["items"]` as a mutable list
fn items_mut(lst: &mut Map<String, Value>) -> anyhow::Result<&mut Vec<Value>> {
    match lst.get_mut("items") {
        Some(Value::Array(a)) => Ok(a),
        Some(other) => Err(no_attr(other, "append")),
        None => anyhow::bail!("'items'"),
    }
}

/// the lists tools, operating on a storage dict
pub fn run(base: &ModuleBase, data: &mut StorageDict, method: &str, args: &ToolArgs) -> anyhow::Result<Value> {
    let s = |k: &str| args.str(k).unwrap_or_else(|| "None".into());
    let ok = |m: &str| Ok(base.result(json!(m), true));
    match method {
        "create" => {
            let (category, name) = (s("category"), s("name"));
            if !data.data.contains_key(&category) {
                data.data.insert(category.clone(), json!({}));
            }
            let cat = match data.data.get_mut(&category).unwrap() {
                Value::Object(m) => m,
                other => return Err(no_attr(other, "keys")),
            };
            if cat.contains_key(&name) {
                return Ok(base.result(json!("list already exists!"), false));
            }
            let items = match args.raw("items") {
                Some(v) if truthy(v) => v.clone(),
                _ => json!([]),
            };
            let pinned = args.raw("pinned").cloned().unwrap_or(json!(false));
            cat.insert(name, json!({"items": items, "pinned": pinned}));
            data.save()?;
            ok("list created!")
        }
        "delete" => {
            let (category, list_name) = (s("category"), s("list_name"));
            if !verify_target(data, &category, &list_name) {
                return ok(NO_LIST);
            }
            data.data[&category].as_object_mut().unwrap().shift_remove(&list_name);
            // check if the category still contains any lists. if not, delete the category itself
            if data.data[&category].as_object().map(|m| m.is_empty()).unwrap_or(false) {
                data.data.shift_remove(&category);
            }
            data.save()?;
            ok("list deleted!")
        }
        "pin" | "unpin" => {
            let (category, list_name) = (s("category"), s("list_name"));
            if !verify_target(data, &category, &list_name) {
                return ok(NO_LIST);
            }
            target(data, &category, &list_name)?.insert("pinned".into(), json!(method == "pin"));
            data.save()?;
            ok(if method == "pin" { "list pinned!" } else { "list unpinned!" })
        }
        "search" => {
            let query = s("query");
            data.load();
            let mut list_entries = Vec::new();
            let mut item_entries = Vec::new();
            for (category_name, category) in &data.data {
                let Value::Object(category) = category else { continue };
                for (list_name, lst) in category {
                    let Value::Object(lst) = lst else { continue };
                    let items: Vec<&str> = match lst.get("items") {
                        Some(Value::Array(a)) => a.iter().filter_map(|i| i.as_str()).filter(|i| !i.trim().is_empty()).collect(),
                        _ => Vec::new(),
                    };
                    list_entries.push(json!({"name": format!("[{}] {}", category_name, list_name), "items": items.len()}));
                    for item in items {
                        item_entries.push(json!({"category": category_name, "list": list_name, "item": item}));
                    }
                }
            }
            let mut w_name = IndexMap::new();
            w_name.insert("name".to_string(), 1.0);
            let mut w_item = IndexMap::new();
            w_item.insert("item".to_string(), 1.0);
            let list_results = ol_core::search::search(&list_entries, &query, "id", Some(&w_name), 5);
            let item_results = ol_core::search::search(&item_entries, &query, "id", Some(&w_item), 15);
            if list_results.is_empty() && item_results.is_empty() {
                return ok(&format!("no lists or list items found matching '{}'", query));
            }
            let mut lines = Vec::new();
            if !list_results.is_empty() {
                lines.push("lists matching the query:".to_string());
                for hit in &list_results {
                    let e = &hit.entry;
                    lines.push(format!("- {} ({} items, score {:.2})", py_str(&e["name"]), e["items"], hit.score));
                }
            }
            if !item_results.is_empty() {
                lines.push("list items matching the query:".to_string());
                for hit in &item_results {
                    let e = &hit.entry;
                    lines.push(format!("- [{}] {}: {} (score {:.2})", py_str(&e["category"]), py_str(&e["list"]), py_str(&e["item"]), hit.score));
                }
            }
            ok(&lines.join("\n"))
        }
        "get" => {
            let (category, list_name) = (s("category"), s("list_name"));
            if !verify_target(data, &category, &list_name) {
                return ok(NO_LIST);
            }
            let lst = target(data, &category, &list_name)?;
            Ok(base.result(lst.get("items").cloned().unwrap_or(Value::Null), true))
        }
        "add_item" => {
            let (category, list_name) = (s("category"), s("list_name"));
            let item = args.raw("item_content").cloned().unwrap_or(Value::Null);
            create_if_non_existent(data, &category, &list_name)?;
            items_mut(target(data, &category, &list_name)?)?.push(item);
            data.save()?;
            ok("list item added!")
        }
        "edit_item" | "delete_item" => {
            let (category, list_name, starts) = (s("category"), s("list_name"), s("item_starts_with"));
            if !verify_target(data, &category, &list_name) {
                return ok(NO_LIST);
            }
            let lst = target(data, &category, &list_name)?;
            let items = match lst.get_mut("items") {
                Some(Value::Array(a)) => a,
                Some(other) => anyhow::bail!("'{}' object is not iterable", py_type_name(other)),
                None => anyhow::bail!("'items'"),
            };
            let Some(found) = find_item(items, &starts)? else {
                return Ok(base.result(json!("could not find that list item"), false));
            };
            if method == "edit_item" {
                items[found] = args.raw("item_content").cloned().unwrap_or(Value::Null);
            } else {
                items.remove(found);
            }
            data.save()?;
            ok(if method == "edit_item" { "list item edited!" } else { "list item deleted!" })
        }
        "get_random_item" => {
            let (category, list_name) = (s("category"), s("list_name"));
            if !verify_target(data, &category, &list_name) {
                return ok(NO_LIST);
            }
            let lst = target(data, &category, &list_name)?;
            let items = match lst.get("items") {
                Some(Value::Array(a)) => a,
                Some(other) => anyhow::bail!("object of type '{}' has no len()", py_type_name(other)),
                None => anyhow::bail!("'items'"),
            };
            let choice = items.choose(&mut rand::thread_rng()).cloned().ok_or_else(|| anyhow::anyhow!("Cannot choose from an empty sequence"))?;
            Ok(base.result(choice, true))
        }
        _ => anyhow::bail!("'Lists' object has no attribute '{}'", method),
    }
}

#[async_trait]
impl Module for Lists {
    fn base(&self) -> &ModuleBase {
        &self.base
    }
    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        *self.data.lock() = Some(StorageDict::new("lists", StorageType::Yaml, None)?);
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        if !self.base.config.get_bool("insert_system_prompt") {
            return Ok(None);
        }
        let max_pinned = self.base.config.get1("max_pinned_lists").unwrap_or(Value::Null);
        let guard = self.data.lock();
        let data = guard.as_ref().ok_or_else(|| anyhow::anyhow!("'Lists' object has no attribute 'data'"))?;
        Ok(Some(system_prompt(&data.data, &max_pinned)?))
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        let mut guard = self.data.lock();
        let data = guard.as_mut().ok_or_else(|| anyhow::anyhow!("'Lists' object has no attribute 'data'"))?;
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
    fn operations_and_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        let mut d = StorageDict::new("lists", StorageType::Yaml, Some(&p)).unwrap();
        let base = ModuleBase::detached("lists");
        let r = run(&base, &mut d, "create", &args(json!({"category": "home", "name": "shopping", "items": ["Milk", "eggs"]}))).unwrap();
        assert_eq!(r, json!({"status": "success", "content": "list created!"}));
        let r = run(&base, &mut d, "create", &args(json!({"category": "home", "name": "shopping"}))).unwrap();
        assert_eq!(r, json!({"status": "error", "content": "list already exists!"}));
        run(&base, &mut d, "add_item", &args(json!({"category": "work", "list_name": "todo", "item_content": "write report"}))).unwrap();
        assert_eq!(d.data["work"]["todo"], json!({"items": ["write report"], "pinned": false}));
        let r = run(&base, &mut d, "edit_item", &args(json!({"category": "home", "list_name": "shopping", "item_starts_with": " mil", "item_content": "oat milk"}))).unwrap();
        assert_eq!(r["content"], "list item edited!");
        let r = run(&base, &mut d, "delete_item", &args(json!({"category": "home", "list_name": "shopping", "item_starts_with": "zzz"}))).unwrap();
        assert_eq!(r, json!({"status": "error", "content": "could not find that list item"}));
        let r = run(&base, &mut d, "get", &args(json!({"category": "home", "list_name": "shopping"}))).unwrap();
        assert_eq!(r["content"], json!(["oat milk", "eggs"]));
        let r = run(&base, &mut d, "get", &args(json!({"category": "home", "list_name": "nope"}))).unwrap();
        assert_eq!(r, json!({"status": "success", "content": "that list doesn't exist"}));
        run(&base, &mut d, "pin", &args(json!({"category": "home", "list_name": "shopping"}))).unwrap();
        let prompt = system_prompt(&d.data, &json!(10)).unwrap();
        assert_eq!(prompt, "## home\n### shopping\n- oat milk\n- eggs\n---\nlists that aren't pinned:\nwork: todo\n");
        let r = run(&base, &mut d, "search", &args(json!({"query": "milk"}))).unwrap();
        let text = r["content"].as_str().unwrap();
        assert!(text.starts_with("list items matching the query:\n- [home] shopping: oat milk (score "), "{}", text);
        let r = run(&base, &mut d, "get_random_item", &args(json!({"category": "work", "list_name": "todo"}))).unwrap();
        assert_eq!(r["content"], "write report");
        let r = run(&base, &mut d, "delete", &args(json!({"category": "work", "list_name": "todo"}))).unwrap();
        assert_eq!(r["content"], "list deleted!");
        assert!(!d.data.contains_key("work"));
        // persisted as yaml
        let text = std::fs::read_to_string(dir.path().join("lists.yml")).unwrap();
        assert!(text.contains("shopping"), "{}", text);
    }
}
