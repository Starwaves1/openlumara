//! Port of modules/lists/module.py

use crate::util::{py_strip, py_type_name, repr_str};
use async_trait::async_trait;
use indexmap::IndexMap;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use ol_core::pyjson::{py_str, truthy};
use ol_core::storage::{StorageDict, StorageType};
use parking_lot::Mutex;
use rand::seq::SliceRandom;
use rand::Rng;
use serde_json::{json, Map, Value};
use std::sync::Arc;

#[path = "../../../generated/modules/lists.rs"]
pub mod meta;

pub struct Lists {
    base: ModuleBase,
    data: Mutex<Option<StorageDict>>,
}

impl Lists {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base, data: Mutex::new(None) }
    }
}

const NO_LIST: &str = "that list doesn't exist";

fn no_attr(v: &Value, attr: &str) -> anyhow::Error {
    anyhow::anyhow!("'{}' object has no attribute '{}'", py_type_name(v), attr)
}

/// python's TypeError for `container[key]` when the container is not a dict. `key_ty` is the python
/// type name of the key. `set` selects item assignment (`container[key] = v`) over a read
fn index_err(container: &Value, key_ty: &str, set: bool) -> anyhow::Error {
    match container {
        Value::Array(_) => anyhow::anyhow!("list indices must be integers or slices, not {}", key_ty),
        Value::String(_) if set => anyhow::anyhow!("'str' object does not support item assignment"),
        Value::String(_) => anyhow::anyhow!("string indices must be integers, not '{}'", key_ty),
        other if set => anyhow::anyhow!("'{}' object does not support item assignment", py_type_name(other)),
        other => anyhow::anyhow!("'{}' object is not subscriptable", py_type_name(other)),
    }
}

/// python's KeyError for a missing key: repr of a str key, str() of any other value (None -> "None")
fn key_error(key: &Value) -> anyhow::Error {
    match key {
        Value::String(k) => anyhow::anyhow!("{}", repr_str(k)),
        other => anyhow::anyhow!("{}", py_str(other)),
    }
}

/// a dict that can be subscripted or item-assigned; anything else raises python's TypeError
fn dict_mut<'a>(container: &'a mut Value, key_ty: &str, set: bool) -> anyhow::Result<&'a mut Map<String, Value>> {
    match container {
        Value::Object(m) => Ok(m),
        other => Err(index_err(other, key_ty, set)),
    }
}

/// a tool argument as python sees it: the raw JSON value, None when absent or null
fn arg(args: &ToolArgs, name: &str) -> Value {
    args.raw(name).cloned().unwrap_or(Value::Null)
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
                // python joins over the items: a str yields characters, a dict yields its keys
                let items = match items {
                    Value::Array(a) => a,
                    Value::String(s) => s.chars().map(|c| Value::String(c.to_string())).collect(),
                    Value::Object(m) => m.keys().map(|k| Value::String(k.clone())).collect(),
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

/// python `_verify_target`: `category not in self.data`, then `list_name not in self.data.get(category, {})`.
/// The keys are the raw JSON values: a non-str key is never a key of the dict, and `in` on a
/// non-container category raises, as python does.
fn verify_target(data: &mut StorageDict, category: &Value, list_name: &Value) -> anyhow::Result<bool> {
    // top-level keys are strings, so only a str category can be found
    let cat_key = match category {
        Value::String(s) => s,
        Value::Array(_) => anyhow::bail!("unhashable type: 'list'"),
        Value::Object(_) => anyhow::bail!("unhashable type: 'dict'"),
        _ => return Ok(false),
    };
    if !data.data.contains_key(cat_key) {
        return Ok(false);
    }
    // StorageDict.get reloads from disk before the lookup
    let Some(container) = data.get(cat_key).cloned() else { return Ok(false) };
    match &container {
        Value::Object(m) => Ok(matches!(list_name, Value::String(n) if m.contains_key(n))),
        Value::Array(a) => Ok(a.contains(list_name)),
        Value::String(s) => match list_name {
            Value::String(n) => Ok(s.contains(n.as_str())),
            other => anyhow::bail!("'in <string>' requires string as left operand, not {}", py_type_name(other)),
        },
        other => anyhow::bail!("argument of type '{}' is not iterable", py_type_name(other)),
    }
}

/// python `_create_if_non_existent`: adds an empty list when the target is missing
fn create_if_non_existent(data: &mut StorageDict, category: &str, list_name: &str) -> anyhow::Result<bool> {
    if verify_target(data, &Value::String(category.to_string()), &Value::String(list_name.to_string()))? {
        return Ok(false);
    }
    let cat = data.data.entry(category.to_string()).or_insert_with(|| json!({}));
    match cat {
        Value::Object(m) => {
            m.insert(list_name.to_string(), json!({"items": [], "pinned": false}));
        }
        other => return Err(index_err(other, "str", true)),
    }
    data.save()?;
    Ok(true)
}

/// the lists.yml key for a category or list name. Only a str can be stored: python would store a
/// None, int or bool key, which the string-keyed storage cannot hold
fn stored_key(key: &Value, what: &str) -> anyhow::Result<String> {
    match key {
        Value::String(s) => Ok(s.clone()),
        Value::Null => anyhow::bail!("unsupported: a null {} cannot be stored in lists.yml (python would use a None key)", what),
        other => anyhow::bail!(
            "unsupported: the {} of type {} cannot be stored in lists.yml (python would use a {} key)",
            what,
            py_type_name(other),
            py_type_name(other)
        ),
    }
}

/// python `self.data[category][list_name]`: the category is subscripted by name, then the list.
/// `set` marks the outer read as item assignment, which only matters for error text
fn entry_mut<'a>(data: &'a mut StorageDict, category: &Value, list_name: &Value, set: bool) -> anyhow::Result<&'a mut Value> {
    let cat = category
        .as_str()
        .and_then(|c| data.data.get_mut(c))
        .ok_or_else(|| key_error(category))?;
    match cat {
        Value::Object(m) => list_name.as_str().and_then(|n| m.get_mut(n)).ok_or_else(|| key_error(list_name)),
        other => Err(index_err(other, py_type_name(list_name), set)),
    }
}

/// python `target_list["items"]`
fn items_of(target: &mut Value) -> anyhow::Result<&mut Value> {
    let map = dict_mut(target, "str", false)?;
    map.get_mut("items").ok_or_else(|| anyhow::anyhow!("'items'"))
}

fn empty_choice() -> anyhow::Error {
    anyhow::anyhow!("Cannot choose from an empty sequence")
}

/// python `_find_item`: the first index whose item, stripped and lowercased, starts with the
/// stripped, lowercased prefix. enumerate() yields characters for a str and keys for a dict
fn find_item(items: &Value, starts_with: &Value) -> anyhow::Result<Option<usize>> {
    let pieces: Vec<Value> = match items {
        Value::Array(a) => a.clone(),
        Value::String(s) => s.chars().map(|c| Value::String(c.to_string())).collect(),
        Value::Object(m) => m.keys().map(|k| Value::String(k.clone())).collect(),
        other => anyhow::bail!("'{}' object is not iterable", py_type_name(other)),
    };
    for (index, item) in pieces.iter().enumerate() {
        let text = item.as_str().ok_or_else(|| no_attr(item, "strip"))?;
        let prefix = starts_with.as_str().ok_or_else(|| no_attr(starts_with, "strip"))?;
        if py_strip(text).to_lowercase().starts_with(&py_strip(prefix).to_lowercase()) {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// python `random.choice(seq)` for any items value
fn random_choice(items: &Value) -> anyhow::Result<Value> {
    let mut rng = rand::thread_rng();
    match items {
        Value::Array(a) => a.choose(&mut rng).cloned().ok_or_else(empty_choice),
        Value::String(s) => {
            let chars: Vec<char> = s.chars().collect();
            chars.choose(&mut rng).map(|c| Value::String(c.to_string())).ok_or_else(empty_choice)
        }
        Value::Object(m) => {
            if m.is_empty() {
                return Err(empty_choice());
            }
            // python indexes the dict by a random position, which is never a key, so it raises KeyError(position)
            let position = rng.gen_range(0..m.len());
            Err(anyhow::anyhow!("{}", position))
        }
        other => anyhow::bail!("object of type '{}' has no len()", py_type_name(other)),
    }
}

/// the lists tools, operating on a storage dict
pub fn run(base: &ModuleBase, data: &mut StorageDict, method: &str, args: &ToolArgs) -> anyhow::Result<Value> {
    let ok = |m: &str| Ok(base.result(json!(m), true));
    match method {
        "create" => {
            let category = stored_key(&arg(args, "category"), "category")?;
            let name = stored_key(&arg(args, "name"), "name")?;
            let cat = match data.data.entry(category).or_insert_with(|| json!({})) {
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
            // python stores the pinned argument as given, so an explicit null stays null
            let pinned = args.0.get("pinned").cloned().unwrap_or(json!(false));
            cat.insert(name, json!({"items": items, "pinned": pinned}));
            data.save()?;
            ok("list created!")
        }
        "delete" => {
            let category = arg(args, "category");
            let list_name = arg(args, "list_name");
            if !verify_target(data, &category, &list_name)? {
                return ok(NO_LIST);
            }
            // verify_target only passes for a str category, so this is always Some
            let Some(cat_key) = category.as_str() else { return ok(NO_LIST) };
            // `del self.data[category][list_name]`: only a dict category supports item deletion
            let cat = match data.data.get_mut(cat_key) {
                Some(Value::Object(m)) => m,
                Some(Value::Array(_)) => anyhow::bail!("list indices must be integers or slices, not {}", py_type_name(&list_name)),
                Some(Value::String(_)) => anyhow::bail!("'str' object doesn't support item deletion"),
                Some(other) => anyhow::bail!("'{}' object doesn't support item deletion", py_type_name(other)),
                None => return Err(key_error(&category)),
            };
            // a dict category only passes verify_target for a str list name
            let Some(list_name) = list_name.as_str() else { return Err(key_error(&list_name)) };
            cat.shift_remove(list_name);
            // check if the category still contains any lists. if not, delete the category itself
            if cat.is_empty() {
                data.data.shift_remove(cat_key);
            }
            data.save()?;
            ok("list deleted!")
        }
        "pin" | "unpin" => {
            let category = arg(args, "category");
            let list_name = arg(args, "list_name");
            if !verify_target(data, &category, &list_name)? {
                return ok(NO_LIST);
            }
            let entry = entry_mut(data, &category, &list_name, false)?;
            dict_mut(entry, "str", true)?.insert("pinned".into(), json!(method == "pin"));
            data.save()?;
            ok(if method == "pin" { "list pinned!" } else { "list unpinned!" })
        }
        "search" => {
            let query = args.str_or("query", "None");
            data.load();
            let mut list_entries = Vec::new();
            let mut item_entries = Vec::new();
            for (category_name, category) in &data.data {
                let Value::Object(category) = category else { continue };
                for (list_name, lst) in category {
                    let Value::Object(lst) = lst else { continue };
                    // `[i for i in lst.get("items", []) if isinstance(i, str) and i.strip()]`
                    let raw_items: Vec<Value> = match lst.get("items") {
                        None => Vec::new(),
                        Some(Value::Array(a)) => a.clone(),
                        Some(Value::String(s)) => s.chars().map(|c| Value::String(c.to_string())).collect(),
                        Some(Value::Object(m)) => m.keys().map(|k| Value::String(k.clone())).collect(),
                        Some(other) => anyhow::bail!("'{}' object is not iterable", py_type_name(other)),
                    };
                    let items: Vec<String> = raw_items
                        .iter()
                        .filter_map(|i| i.as_str())
                        .filter(|i| !py_strip(i).is_empty())
                        .map(|i| i.to_string())
                        .collect();
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
            let category = arg(args, "category");
            let list_name = arg(args, "list_name");
            if !verify_target(data, &category, &list_name)? {
                return ok(NO_LIST);
            }
            match entry_mut(data, &category, &list_name, false)? {
                Value::Object(m) => Ok(base.result(m.get("items").cloned().unwrap_or(Value::Null), true)),
                other => Err(no_attr(other, "get")),
            }
        }
        "add_item" => {
            let category = stored_key(&arg(args, "category"), "category")?;
            let list_name = stored_key(&arg(args, "list_name"), "list_name")?;
            // python stores item_content as given, so a null is appended as null
            let content = args.0.get("item_content").cloned().unwrap_or(Value::Null);
            create_if_non_existent(data, &category, &list_name)?;
            let entry = entry_mut(data, &json!(category), &json!(list_name), false)?;
            match items_of(entry)? {
                Value::Array(a) => a.push(content),
                other => return Err(no_attr(other, "append")),
            }
            data.save()?;
            ok("list item added!")
        }
        "edit_item" | "delete_item" => {
            let category = arg(args, "category");
            let list_name = arg(args, "list_name");
            let starts = arg(args, "item_starts_with");
            if !verify_target(data, &category, &list_name)? {
                return ok(NO_LIST);
            }
            let entry = entry_mut(data, &category, &list_name, false)?;
            let items = items_of(entry)?;
            let Some(found) = find_item(&*items, &starts)? else {
                return Ok(base.result(json!("could not find that list item"), false));
            };
            if method == "edit_item" {
                let content = args.0.get("item_content").cloned().unwrap_or(Value::Null);
                match items {
                    Value::Array(a) => a[found] = content,
                    Value::String(_) => anyhow::bail!("'str' object does not support item assignment"),
                    _ => anyhow::bail!(
                        "unsupported: list items is a mapping, so assigning key {} would add a non-string key that lists.yml cannot hold",
                        found
                    ),
                }
                data.save()?;
                ok("list item edited!")
            } else {
                match items {
                    Value::Array(a) => {
                        a.remove(found);
                    }
                    Value::String(_) => anyhow::bail!("'str' object has no attribute 'pop'"),
                    // dict.pop(position) with string keys raises KeyError(position)
                    _ => return Err(anyhow::anyhow!("{}", found)),
                }
                data.save()?;
                ok("list item deleted!")
            }
        }
        "get_random_item" => {
            let category = arg(args, "category");
            let list_name = arg(args, "list_name");
            if !verify_target(data, &category, &list_name)? {
                return ok(NO_LIST);
            }
            let entry = entry_mut(data, &category, &list_name, false)?;
            let choice = random_choice(&*items_of(entry)?)?;
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

    #[test]
    fn prompt_caps_lists_and_skips_empty_ones() {
        let mut data = Map::new();
        data.insert("home".into(), json!({
            "empty": {"items": [], "pinned": true},
            "shopping": {"items": ["milk"], "pinned": true},
        }));
        data.insert("work".into(), json!({
            "todo": {"items": ["report"], "pinned": false},
        }));
        // categories are walked in reverse (work, then home); lists inside a category in order.
        // work/todo takes slot 1, home/empty takes slot 2 (skipped but counted), home/shopping takes slot 3.
        let prompt = system_prompt(&data, &json!(3)).unwrap();
        assert_eq!(prompt, "## home\n### shopping\n- milk\n---\nlists that aren't pinned:\nwork: todo\n");
        // cap 2 breaks before home/shopping, so the pinned list is cut off and the prompt has no pinned section
        let prompt = system_prompt(&data, &json!(2)).unwrap();
        assert_eq!(prompt, "---\nlists that aren't pinned:\nwork: todo\n");
    }

    #[test]
    fn missing_and_non_dict_targets_report_python_strings() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        let mut d = StorageDict::new("lists", StorageType::Yaml, Some(&p)).unwrap();
        let base = ModuleBase::detached("lists");
        let r = run(&base, &mut d, "search", &args(json!({"query": "zzzqx"}))).unwrap();
        assert_eq!(r, json!({"status": "success", "content": "no lists or list items found matching 'zzzqx'"}));
        let r = run(&base, &mut d, "pin", &args(json!({"category": "nope", "list_name": "x"}))).unwrap();
        assert_eq!(r, json!({"status": "success", "content": "that list doesn't exist"}));
        // a list-valued category passes the `in` membership check; delete must error, not panic
        // get() reloads from disk, so the odd category has to be saved to be seen
        d.data.insert("odd".into(), json!(["todo"]));
        d.save().unwrap();
        let err = run(&base, &mut d, "delete", &args(json!({"category": "odd", "list_name": "todo"}))).unwrap_err();
        assert_eq!(err.to_string(), "list indices must be integers or slices, not str");
        let err = run(&base, &mut d, "create", &args(json!({"category": "odd", "name": "todo"}))).unwrap_err();
        assert_eq!(err.to_string(), "'list' object has no attribute 'keys'");
    }

    #[test]
    fn non_container_category_raises_like_python_in_operator() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        let mut d = StorageDict::new("lists", StorageType::Yaml, Some(&p)).unwrap();
        let base = ModuleBase::detached("lists");
        d.data.insert("bad".into(), json!(5));
        d.save().unwrap();
        // python: `list_name not in 5` raises instead of reporting a missing list
        let err = run(&base, &mut d, "pin", &args(json!({"category": "bad", "list_name": "x"}))).unwrap_err();
        assert_eq!(err.to_string(), "argument of type 'int' is not iterable");
        let err = run(&base, &mut d, "get", &args(json!({"category": "bad", "list_name": "x"}))).unwrap_err();
        assert_eq!(err.to_string(), "argument of type 'int' is not iterable");
        // a null category is never a key, so the list is simply not found
        let r = run(&base, &mut d, "get", &args(json!({"category": null, "list_name": "x"}))).unwrap();
        assert_eq!(r, json!({"status": "success", "content": "that list doesn't exist"}));
    }

    #[test]
    fn stored_values_match_python_for_null_pinned_and_string_items() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        let mut d = StorageDict::new("lists", StorageType::Yaml, Some(&p)).unwrap();
        let base = ModuleBase::detached("lists");
        let r = run(&base, &mut d, "create", &args(json!({"category": "home", "name": "shopping", "items": "milk", "pinned": null}))).unwrap();
        assert_eq!(r["content"], "list created!");
        // python stores pinned=None as given, and keeps a str items value as a str
        assert_eq!(d.data["home"]["shopping"], json!({"items": "milk", "pinned": null}));
        // random.choice on a str returns one character of it
        let r = run(&base, &mut d, "get_random_item", &args(json!({"category": "home", "list_name": "shopping"}))).unwrap();
        let c = r["content"].as_str().unwrap();
        assert!("milk".contains(c) && c.chars().count() == 1, "{}", c);
        // no matching character: python reports "could not find" without raising
        let r = run(&base, &mut d, "edit_item", &args(json!({"category": "home", "list_name": "shopping", "item_starts_with": "z", "item_content": "x"}))).unwrap();
        assert_eq!(r, json!({"status": "error", "content": "could not find that list item"}));
        // a matching character reaches python's str item assignment and delete errors
        let err = run(&base, &mut d, "edit_item", &args(json!({"category": "home", "list_name": "shopping", "item_starts_with": "m", "item_content": "x"}))).unwrap_err();
        assert_eq!(err.to_string(), "'str' object does not support item assignment");
        let err = run(&base, &mut d, "delete_item", &args(json!({"category": "home", "list_name": "shopping", "item_starts_with": "m"}))).unwrap_err();
        assert_eq!(err.to_string(), "'str' object has no attribute 'pop'");
    }

    #[test]
    fn python_strip_separators_count_as_whitespace_when_matching_items() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        let mut d = StorageDict::new("lists", StorageType::Yaml, Some(&p)).unwrap();
        let base = ModuleBase::detached("lists");
        run(&base, &mut d, "create", &args(json!({"category": "home", "name": "shopping", "items": ["\u{1c}apple"]}))).unwrap();
        // python: "\x1capple".strip() == "apple", so the prefix matches
        let r = run(&base, &mut d, "edit_item", &args(json!({"category": "home", "list_name": "shopping", "item_starts_with": "apple", "item_content": "pear"}))).unwrap();
        assert_eq!(r["content"], "list item edited!");
        assert_eq!(d.data["home"]["shopping"]["items"], json!(["pear"]));
    }

    #[test]
    fn non_str_arguments_are_never_matched_as_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        let mut d = StorageDict::new("lists", StorageType::Yaml, Some(&p)).unwrap();
        let base = ModuleBase::detached("lists");
        run(&base, &mut d, "create", &args(json!({"category": "5", "name": "5", "items": ["x"]}))).unwrap();
        // python: `5 not in self.data` is True, so the str category "5" is not found
        let r = run(&base, &mut d, "pin", &args(json!({"category": 5, "list_name": "5"}))).unwrap();
        assert_eq!(r, json!({"status": "success", "content": "that list doesn't exist"}));
        // python: `5 not in {"5": ...}` is True, so the list named "5" is not found
        let r = run(&base, &mut d, "get", &args(json!({"category": "5", "list_name": 5}))).unwrap();
        assert_eq!(r, json!({"status": "success", "content": "that list doesn't exist"}));
        assert_eq!(d.data["5"]["5"]["pinned"], json!(false));
        // python stores an int category under an int key, which string-keyed lists.yml cannot hold
        let err = run(&base, &mut d, "create", &args(json!({"category": 5, "name": "x"}))).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unsupported: the category of type int cannot be stored in lists.yml (python would use a int key)"
        );
        // python: a non-str prefix reaches `starts_with.strip()` on the first item and raises
        let err = run(&base, &mut d, "edit_item", &args(json!({"category": "5", "list_name": "5", "item_starts_with": 5, "item_content": "y"}))).unwrap_err();
        assert_eq!(err.to_string(), "'int' object has no attribute 'strip'");
    }

    #[test]
    fn dict_items_render_keys_in_prompt_and_other_types_raise() {
        let mut data = Map::new();
        data.insert("home".into(), json!({"shopping": {"items": {"milk": 1, "eggs": 2}, "pinned": true}}));
        // python iterates a dict's keys when joining a pinned list
        assert_eq!(system_prompt(&data, &json!(10)).unwrap(), "## home\n### shopping\n- milk\n- eggs\n");
        let mut bad = Map::new();
        bad.insert("home".into(), json!({"shopping": {"items": 5, "pinned": true}}));
        let err = system_prompt(&bad, &json!(10)).unwrap_err();
        assert_eq!(err.to_string(), "'int' object is not iterable");
    }
}
