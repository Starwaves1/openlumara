//! Port of modules/memory/module.py: persistent memories with pinning + BM25 search.

#[path = "../../../generated/modules/memory.rs"]
pub mod meta;

use crate::util::{body_get, dict_body, iso, now, py_type, query_str, round2, truthy_opt};
use anyhow::anyhow;
use async_trait::async_trait;
use indexmap::IndexMap;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::{py_str, py_str_opt, truthy};
use ol_core::storage::{StorageList, StorageType};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const NUDGE_PROMPT: &str = "You are responsible for managing your own long-term memory. You must proactively and autonomously decide to use the memory tools to maintain an accurate, up-to-date, and efficient record of the user, your own operational preferences, and important contextual facts. Do not wait for instructions to remember; if information is valuable for future interactions, store it immediately.";

pub struct Memory {
    base: ModuleBase,
    mem: Mutex<Option<StorageList>>,
    deleted: Mutex<Option<StorageList>>,
}

fn weights(search_in_content: bool) -> IndexMap<String, f64> {
    let mut w = IndexMap::new();
    w.insert("tags".to_string(), 2.0);
    if search_in_content {
        w.insert("content".to_string(), 1.0);
    }
    w
}

/// `str(query.get("search_content", "true")).lower() not in ("false", "0", "no")`
fn search_content_flag(query: &Map<String, Value>) -> bool {
    !matches!(query_str(query, "search_content", "true").to_lowercase().as_str(), "false" | "0" | "no")
}

/// compact JSON shape used by the webui memory sidebar
fn ui_item(mem: &Value, score: Option<f64>) -> Value {
    let tags = mem.get("tags").filter(|t| truthy(t)).cloned().unwrap_or(json!([]));
    let mut item = json!({
        "id": mem.get("id").cloned().unwrap_or(Value::Null),
        "content": mem.get("content").cloned().unwrap_or(json!("")),
        "tags": tags,
        "pinned": truthy_opt(mem.get("pinned")),
        "date_created": mem.get("date_created").cloned().unwrap_or(json!("")),
    });
    if let Some(s) = score {
        item["score"] = json!(round2(s));
    }
    item
}

/// python `for tag in mem.get("tags", []) or []`: a list yields its items, a string its characters, a dict its
/// keys; any other truthy value raises TypeError
fn tag_items(mem: &Value) -> anyhow::Result<Vec<Value>> {
    Ok(match mem.get("tags").filter(|t| truthy(t)) {
        None => vec![],
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => s.chars().map(|c| Value::String(c.to_string())).collect(),
        Some(Value::Object(o)) => o.keys().map(|k| Value::String(k.clone())).collect(),
        Some(other) => anyhow::bail!("'{}' object is not iterable", py_type(other)),
    })
}

/// python `int(s)` for a query string: surrounding whitespace, one sign and single underscores between digits
/// are accepted, anything else is a ValueError (None). magnitudes saturate, so every clamp and comparison the
/// routes make comes out the same as with python's arbitrary-precision int
fn py_int_query(s: &str) -> Option<i128> {
    let t = s.trim();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let b = digits.as_bytes();
    if !b.first()?.is_ascii_digit() || !b.last()?.is_ascii_digit() {
        return None;
    }
    let mut v: i128 = 0;
    let mut prev_underscore = false;
    for &c in b {
        match c {
            b'_' if !prev_underscore => prev_underscore = true,
            b'0'..=b'9' => {
                prev_underscore = false;
                v = v.saturating_mul(10).saturating_add(i128::from(c - b'0'));
            }
            _ => return None,
        }
    }
    Some(if neg { -v } else { v })
}

/// `int((query or {}).get(key, default) or default)`, with a ValueError falling back to the default
fn query_limit(query: &Map<String, Value>, key: &str, default: i128) -> i128 {
    match query.get(key) {
        Some(v) if truthy(v) => py_int_query(&py_str(v)).unwrap_or(default),
        _ => default,
    }
}

/// case-insensitive tag filter used by the webui routes
fn matches_tag(mem: &Value, tag: Option<&Value>) -> anyhow::Result<bool> {
    let Some(tag) = tag.filter(|t| truthy(t)) else { return Ok(true) };
    let wanted = py_str(tag).trim().to_lowercase();
    if wanted.is_empty() {
        return Ok(true);
    }
    Ok(tag_items(mem)?.iter().any(|t| py_str(t).trim().to_lowercase() == wanted))
}

impl Memory {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base, mem: Mutex::new(None), deleted: Mutex::new(None) }
    }

    fn with_mem<R>(&self, f: impl FnOnce(&mut StorageList) -> R) -> anyhow::Result<R> {
        let mut g = self.mem.lock();
        let s = g.as_mut().ok_or_else(|| anyhow!("'Memory' object has no attribute '_mem'"))?;
        Ok(f(s))
    }

    /// snapshot of all memories (python iterates the list without reloading)
    fn entries(&self) -> anyhow::Result<Vec<Value>> {
        self.with_mem(|m| m.data.clone())
    }

    fn reload(&self) -> anyhow::Result<()> {
        self.with_mem(|m| m.load())
    }

    /// checks if a memory with ID exists in memories
    fn get_index(data: &[Value], id: &str) -> i64 {
        for (i, mem) in data.iter().enumerate() {
            if id.trim() == py_str_opt(mem.get("id")).trim() {
                return i as i64;
            }
        }
        -1
    }

    /// python `_get_index(ulid)` calls `ulid.strip()` once per memory, so a non-string id only fails when memories
    /// exist; with none it is a plain miss (None)
    fn lookup_id(&self, id: &Value) -> anyhow::Result<Option<String>> {
        match id {
            Value::String(s) => Ok(Some(s.clone())),
            _ if self.entries()?.is_empty() => Ok(None),
            other => anyhow::bail!("'{}' object has no attribute 'strip'", py_type(other)),
        }
    }

    fn not_found(&self) -> Value {
        self.base.result("memory with that ID not found!", true)
    }

    pub async fn create(&self, content: Value, tags: Value, pinned: Value) -> anyhow::Result<Value> {
        let id = ol_core::globals::new_ulid();
        let mem = json!({
            "id": id,
            "content": content,
            "tags": tags,
            "pinned": pinned,
            "date_created": iso(&now()),
        });
        self.with_mem(|m| {
            m.data.push(mem);
            m.save();
        })?;
        Ok(self.base.result(format!("memory added. ID: {}", id), true))
    }

    pub async fn edit(&self, id: &str, content: Option<Value>, tags: Option<Value>) -> anyhow::Result<Value> {
        let found = self.with_mem(|m| {
            let index = Self::get_index(&m.data, id);
            if index == -1 {
                return false;
            }
            let entry = &mut m.data[index as usize];
            if let Some(c) = content.filter(truthy) {
                entry["content"] = c;
            }
            if let Some(t) = tags.filter(truthy) {
                entry["tags"] = t;
            }
            m.save();
            true
        })?;
        if !found {
            return Ok(self.not_found());
        }
        Ok(self.base.result(format!("memory {} edited", id), true))
    }

    pub async fn delete(&self, id: &str) -> anyhow::Result<Value> {
        let removed = self.with_mem(|m| {
            let index = Self::get_index(&m.data, id);
            if index == -1 {
                return None;
            }
            Some(m.data[index as usize].clone())
        })?;
        let Some(removed) = removed else { return Ok(self.not_found()) };
        {
            let mut g = self.deleted.lock();
            let d = g.as_mut().ok_or_else(|| anyhow!("'Memory' object has no attribute '_mem_deleted'"))?;
            d.data.push(removed);
            d.save();
        }
        self.with_mem(|m| {
            let index = Self::get_index(&m.data, id);
            if index >= 0 {
                m.data.remove(index as usize);
            }
            m.save();
        })?;
        Ok(self.base.result(format!("memory {} deleted", id), true))
    }

    async fn set_pinned(&self, id: &str, pinned: bool) -> anyhow::Result<Value> {
        let found = self.with_mem(|m| {
            let index = Self::get_index(&m.data, id);
            if index == -1 {
                return false;
            }
            m.data[index as usize]["pinned"] = Value::Bool(pinned);
            m.save();
            true
        })?;
        if !found {
            return Ok(self.not_found());
        }
        let verb = if pinned { "pinned" } else { "unpinned" };
        Ok(self.base.result(format!("memory {} {}", id, verb), true))
    }

    pub async fn search(&self, query: &str, search_in_content: bool) -> anyhow::Result<Value> {
        let entries = self.entries()?;
        let w = weights(search_in_content);
        let results = ol_core::search::search(&entries, query, "id", Some(&w), 10);
        if results.is_empty() {
            return Ok(self.base.result(format!("No memories found matching '{}'.", query), true));
        }
        let lines: Vec<String> = results
            .iter()
            .map(|hit| {
                let mem = &hit.entry;
                format!(
                    "ID: {} | Score: {:.2} | Tags: {} | Content: {}",
                    py_str_opt(mem.get("id")),
                    hit.score,
                    py_str_opt(mem.get("tags")),
                    py_str_opt(mem.get("content"))
                )
            })
            .collect();
        Ok(self.base.result(lines.join("\n"), true))
    }

    pub async fn list_unpinned(&self, tag: Option<String>) -> anyhow::Result<Value> {
        let tag = tag.filter(|t| !t.is_empty());
        let mut results = Vec::new();
        for mem in self.entries()? {
            if truthy_opt(mem.get("pinned")) {
                continue;
            }
            let tags = mem.get("tags").cloned().unwrap_or(json!([]));
            let line = format!("ID: {} | Tags: {} | Content: {}", py_str_opt(mem.get("id")), py_str(&tags), py_str_opt(mem.get("content")));
            match &tag {
                Some(t) => {
                    let wanted = t.to_lowercase();
                    let hit = tags.as_array().map(|a| a.iter().any(|x| py_str(x).to_lowercase().contains(&wanted))).unwrap_or(false);
                    if hit {
                        results.push(line);
                    }
                }
                None => results.push(line),
            }
        }
        if results.is_empty() {
            let mut msg = "No unpinned memories found.".to_string();
            if let Some(t) = &tag {
                msg.push_str(&format!(" (No unpinned memories found with tag '{}')", t));
            }
            return Ok(self.base.result(msg, true));
        }
        Ok(self.base.result(results.join("\n"), true))
    }

    // ---------------- webui routes ----------------

    fn route_tags(&self, query: &Map<String, Value>) -> anyhow::Result<Value> {
        self.reload()?;
        let tab = query_str(query, "tab", "all").to_lowercase();
        let q = query_str(query, "q", "").trim().to_string();
        let limit = query_limit(query, "limit", 200).clamp(1, 500);
        let all = self.entries()?;
        let entries: Vec<Value> = if tab == "pinned" {
            all.into_iter().filter(|m| truthy_opt(m.get("pinned"))).collect()
        } else if tab == "search" && !q.is_empty() {
            let w = weights(search_content_flag(query));
            ol_core::search::search(&all, &q, "id", Some(&w), limit as usize)
                .into_iter()
                .map(|h| h.entry)
                .filter(truthy)
                .collect()
        } else {
            all
        };
        let mut tags: Vec<String> = Vec::new();
        for mem in &entries {
            for tag in tag_items(mem)? {
                let t = py_str(&tag).trim().to_string();
                if !t.is_empty() && !tags.contains(&t) {
                    tags.push(t);
                }
            }
        }
        tags.sort();
        tags.sort_by_key(|t| t.to_lowercase());
        Ok(json!(tags))
    }

    fn route_list(&self, query: &Map<String, Value>, pinned_only: bool) -> anyhow::Result<Value> {
        self.reload()?;
        let limit = if pinned_only { query_limit(query, "limit", 0) } else { query_limit(query, "limit", 100).clamp(1, 500) };
        let tag = query.get("tag");
        let mut items = Vec::new();
        for mem in self.entries()?.iter().rev() {
            if pinned_only && !truthy_opt(mem.get("pinned")) {
                continue;
            }
            if !matches_tag(mem, tag)? {
                continue;
            }
            items.push(ui_item(mem, None));
            if (!pinned_only || limit != 0) && items.len() as i128 >= limit {
                break;
            }
        }
        Ok(Value::Array(items))
    }

    fn route_search(&self, query: &Map<String, Value>) -> anyhow::Result<Value> {
        self.reload()?;
        let q = query_str(query, "q", "").trim().to_string();
        if q.is_empty() {
            return Ok(json!([]));
        }
        let limit = query_limit(query, "limit", 20).clamp(1, 100);
        let tag = query.get("tag");
        let mut entries: Vec<Value> = Vec::new();
        for mem in self.entries()? {
            if matches_tag(&mem, tag)? {
                entries.push(mem);
            }
        }
        if entries.is_empty() {
            return Ok(json!([]));
        }
        let w = weights(search_content_flag(query));
        let items: Vec<Value> = ol_core::search::search(&entries, &q, "id", Some(&w), limit as usize)
            .into_iter()
            .filter(|h| truthy(&h.entry))
            .map(|h| ui_item(&h.entry, Some(h.score)))
            .collect();
        Ok(Value::Array(items))
    }

    async fn route_update(&self, body: &Option<Value>, mem_id: &Value) -> anyhow::Result<Value> {
        self.reload()?;
        let content = body_get(body, "content");
        let content = match content {
            Some(c) if !c.is_null() && !py_str(&c).trim().is_empty() => py_str(&c).trim().to_string(),
            _ => return Ok(self.base.result("memory content cannot be empty", false)),
        };
        let tags: Vec<Value> = match body_get(body, "tags") {
            Some(Value::Array(a)) => a,
            _ => vec![],
        };
        let tags: Vec<Value> = tags.iter().map(|t| py_str(t).trim().to_string()).filter(|t| !t.is_empty()).map(Value::String).collect();
        let Some(id) = self.lookup_id(mem_id)? else {
            return Ok(self.base.result("memory with that ID not found!", false));
        };
        let found = self.with_mem(|m| {
            let index = Self::get_index(&m.data, &id);
            if index == -1 {
                return false;
            }
            let e = &mut m.data[index as usize];
            e["content"] = json!(content);
            e["tags"] = Value::Array(tags);
            m.save();
            true
        })?;
        if !found {
            return Ok(self.base.result("memory with that ID not found!", false));
        }
        Ok(self.base.result(format!("memory {} edited", id), true))
    }
}

#[async_trait]
impl Module for Memory {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "create" => {
                self.create(
                    args.0.get("content").cloned().unwrap_or(Value::Null),
                    args.0.get("tags").cloned().unwrap_or(Value::Null),
                    args.0.get("pinned").cloned().unwrap_or(Value::Bool(false)),
                )
                .await
            }
            "edit" => self.edit(&args.str_or("id", ""), args.raw("content").cloned(), args.raw("tags").cloned()).await,
            "delete" => self.delete(&args.str_or("id", "")).await,
            "pin" => self.set_pinned(&args.str_or("id", ""), true).await,
            "unpin" => self.set_pinned(&args.str_or("id", ""), false).await,
            "search" => {
                let sic = args.0.get("search_in_content").map(truthy).unwrap_or(true);
                self.search(&args.str_or("query", ""), sic).await
            }
            "list_unpinned" => self.list_unpinned(args.str("tag")).await,
            _ => anyhow::bail!("'Memory' object has no attribute '{}'", method),
        }
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        *self.mem.lock() = Some(StorageList::new("memory", StorageType::Msgpack, None)?);
        *self.deleted.lock() = Some(StorageList::new("deleted_memories", StorageType::Json, None)?);
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        let mut pinned_str = String::new();
        if self.base.config.get_bool("put_pinned_memories_in_system_prompt") {
            let max = self.base.config.get_f64("max_pinned_memories").unwrap_or(20.0);
            let mut count = 1.0;
            let mut list = Vec::new();
            for mem in self.entries()?.into_iter().rev() {
                if !truthy_opt(mem.get("pinned")) {
                    continue;
                }
                if count > max {
                    break;
                }
                list.push(mem);
                count += 1.0;
            }
            let pinned: Vec<String> = list
                .iter()
                .map(|m| format!("{}:\n{}", py_str_opt(m.get("id")), py_str_opt(m.get("content"))))
                .collect();
            pinned_str = pinned.join("\n\n");
            if pinned_str.is_empty() {
                pinned_str = "There are currently no pinned memories.".into();
            }
            pinned_str.push_str("\n\n");
        }
        if pinned_str.is_empty() {
            return Ok(None);
        }
        Ok(Some(pinned_str))
    }

    async fn on_end_prompt(&self) -> anyhow::Result<Option<String>> {
        if self.base.config.get_bool("insert_nudge_prompt") {
            return Ok(Some(NUDGE_PROMPT.to_string()));
        }
        Ok(None)
    }

    fn ui_routes(&self) -> Vec<UiRoute> {
        meta::ui_routes().into_iter().map(|(r, _)| r).collect()
    }

    async fn ui_route(&self, path: &str, method: &str, body: Option<Value>, query: Map<String, Value>) -> anyhow::Result<Value> {
        match (path, method) {
            ("tags", "GET") => self.route_tags(&query),
            ("list", "GET") => self.route_list(&query, false),
            ("list_pinned", "GET") => self.route_list(&query, true),
            ("search", "GET") => self.route_search(&query),
            ("pin", "POST") | ("unpin", "POST") | ("update", "POST") | ("delete", "POST") => {
                // python `(body or {}).get("id")`: a truthy non-object body raises AttributeError
                dict_body(&body)?;
                let Some(id) = body_get(&body, "id").filter(truthy) else {
                    return Ok(self.base.result("no memory id given", false));
                };
                match path {
                    "update" => self.route_update(&body, &id).await,
                    _ => {
                        self.reload()?;
                        let Some(id) = self.lookup_id(&id)? else { return Ok(self.not_found()) };
                        match path {
                            "pin" => self.set_pinned(&id, true).await,
                            "unpin" => self.set_pinned(&id, false).await,
                            _ => self.delete(&id).await,
                        }
                    }
                }
            }
            _ => anyhow::bail!("route not found: {}", path),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_matching() {
        let m = json!({"tags": [" Food ", "x"]});
        assert!(matches_tag(&m, Some(&json!("food"))).unwrap());
        assert!(!matches_tag(&m, Some(&json!("foo"))).unwrap());
        assert!(matches_tag(&m, Some(&json!(""))).unwrap());
        assert!(matches_tag(&m, None).unwrap());
    }

    #[test]
    fn ui_item_shape() {
        let m = json!({"id": "A", "content": "c", "tags": null, "pinned": 1});
        let i = ui_item(&m, Some(1.23456));
        assert_eq!(i, json!({"id": "A", "content": "c", "tags": [], "pinned": true, "date_created": "", "score": 1.23}));
    }

    #[test]
    fn index_lookup_strips() {
        let data = vec![json!({"id": "01ABC "}), json!({"id": "02"})];
        assert_eq!(Memory::get_index(&data, " 01ABC"), 0);
        assert_eq!(Memory::get_index(&data, "03"), -1);
    }
}

/// webui route behaviour, driven through `ui_route` the way the ext dispatcher calls it
#[cfg(test)]
mod route_tests {
    use super::*;
    use std::sync::Weak;

    fn query(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    /// a memory module whose store is a temp file, seeded through its own create()
    fn module(dir: &tempfile::TempDir) -> Memory {
        let m = Memory::new(ModuleBase::new("memory", false, Weak::new()));
        let store = StorageList::with_autoload("memory", StorageType::Json, Some(dir.path().to_str().unwrap()), false).unwrap();
        *m.mem.lock() = Some(store);
        m
    }

    /// adds a memory and returns its id, taken from the "memory added. ID: <id>" answer
    async fn seed(m: &Memory, content: &str, tags: Value, pinned: bool) -> String {
        let answer = m.create(json!(content), tags, json!(pinned)).await.unwrap();
        answer["content"].as_str().unwrap().trim_start_matches("memory added. ID: ").to_string()
    }

    async fn get(m: &Memory, path: &str, q: Value) -> Value {
        m.ui_route(path, "GET", None, query(q)).await.unwrap()
    }

    /// the `id` of every item in a route's data array, in order
    fn ids(data: &Value) -> Vec<String> {
        data.as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn py_int_query_follows_python_int() {
        assert_eq!(py_int_query(" +7 "), Some(7));
        assert_eq!(py_int_query("-3"), Some(-3));
        assert_eq!(py_int_query("1_000"), Some(1000));
        for bad in ["_1", "1_", "5__0", "1__0", "", "-", "1.5", "abc"] {
            assert_eq!(py_int_query(bad), None, "{bad:?}");
        }
        // beyond i64 python keeps the value; saturation keeps the clamps identical
        assert_eq!(py_int_query("99999999999999999999999999999999999999999999"), Some(i128::MAX));
        assert!(py_int_query("-99999999999999999999999999999999999999999999").unwrap() < 0);
    }

    #[tokio::test]
    async fn list_is_newest_first_filters_by_whole_tag_and_clamps_limit() {
        let dir = tempfile::tempdir().unwrap();
        let m = module(&dir);
        let a = seed(&m, "likes tea", json!(["Food"]), false).await;
        let b = seed(&m, "owns a cat", json!(["pets"]), true).await;
        let c = seed(&m, "loves tea too", json!(["food", "Work"]), false).await;

        let data = get(&m, "list", json!({})).await;
        assert_eq!(ids(&data), vec![c.clone(), b.clone(), a.clone()]);
        assert_eq!(data[0]["content"], json!("loves tea too"));
        assert_eq!(data[0]["tags"], json!(["food", "Work"]));
        assert_eq!(data[0]["pinned"], json!(false));
        assert!(data[0]["date_created"].is_string());
        assert!(data[0].get("score").is_none(), "a plain list carries no score");

        // exact, case-insensitive match on a whole tag, not a substring
        assert_eq!(ids(&get(&m, "list", json!({"tag": "FOOD"})).await), vec![c.clone(), a.clone()]);
        assert_eq!(get(&m, "list", json!({"tag": "wor"})).await, json!([]));

        // limit clamps to 1..=500; a value int() rejects falls back to the default of 100
        assert_eq!(ids(&get(&m, "list", json!({"limit": "2"})).await), vec![c.clone(), b.clone()]);
        assert_eq!(ids(&get(&m, "list", json!({"limit": "0"})).await), vec![c.clone()]);
        assert_eq!(get(&m, "list", json!({"limit": "abc"})).await.as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn huge_and_malformed_limits_match_python() {
        let dir = tempfile::tempdir().unwrap();
        let m = module(&dir);
        for text in ["first pin", "second pin", "third pin"] {
            seed(&m, text, json!([]), true).await;
        }
        // python: a huge negative limit breaks after one item, a huge positive one never stops early
        assert_eq!(get(&m, "list_pinned", json!({"limit": "-99999999999999999999"})).await.as_array().unwrap().len(), 1);
        assert_eq!(get(&m, "list_pinned", json!({"limit": "99999999999999999999"})).await.as_array().unwrap().len(), 3);
        // "1__0" is a ValueError, so the default 0 (unlimited) applies
        assert_eq!(get(&m, "list_pinned", json!({"limit": "1__0"})).await.as_array().unwrap().len(), 3);
        // "1_0" is 10, which is above the three memories, so all of them come back
        assert_eq!(get(&m, "list", json!({"limit": "1_0"})).await.as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn pin_and_unpin_answer_with_python_messages_and_python_body_errors() {
        let dir = tempfile::tempdir().unwrap();
        let m = module(&dir);
        let id = seed(&m, "remember me", json!([]), false).await;

        let pinned = m.ui_route("pin", "POST", Some(json!({"id": id})), Map::new()).await.unwrap();
        assert_eq!(pinned, json!({"status": "success", "content": format!("memory {id} pinned")}));
        assert_eq!(ids(&get(&m, "list_pinned", json!({})).await), vec![id.clone()]);

        let unpinned = m.ui_route("unpin", "POST", Some(json!({"id": id})), Map::new()).await.unwrap();
        assert_eq!(unpinned, json!({"status": "success", "content": format!("memory {id} unpinned")}));
        assert_eq!(get(&m, "list_pinned", json!({})).await, json!([]));

        // a missing id is a failed answer; an unknown id is pin()'s default not-found answer, still success
        let missing = m.ui_route("pin", "POST", Some(json!({})), Map::new()).await.unwrap();
        assert_eq!(missing, json!({"status": "error", "content": "no memory id given"}));
        let unknown = m.ui_route("pin", "POST", Some(json!({"id": "nope"})), Map::new()).await.unwrap();
        assert_eq!(unknown, json!({"status": "success", "content": "memory with that ID not found!"}));

        // a truthy non-object body raises on .get(); python's exception text is the answer
        let err = m.ui_route("pin", "POST", Some(json!([1])), Map::new()).await.unwrap_err();
        assert_eq!(err.to_string(), "'list' object has no attribute 'get'");
        let err = m.ui_route("unpin", "POST", Some(json!("x")), Map::new()).await.unwrap_err();
        assert_eq!(err.to_string(), "'str' object has no attribute 'get'");

        // a non-string id fails on strip() once memories exist, but is a plain miss on an empty store
        let err = m.ui_route("pin", "POST", Some(json!({"id": 5})), Map::new()).await.unwrap_err();
        assert_eq!(err.to_string(), "'int' object has no attribute 'strip'");
        let empty = module(&tempfile::tempdir().unwrap());
        let miss = empty.ui_route("pin", "POST", Some(json!({"id": 5})), Map::new()).await.unwrap();
        assert_eq!(miss, json!({"status": "success", "content": "memory with that ID not found!"}));
    }

    #[tokio::test]
    async fn string_tags_are_iterated_by_character_and_bad_tag_types_raise() {
        let dir = tempfile::tempdir().unwrap();
        let m = module(&dir);
        let text = seed(&m, "a string tag", json!("fo"), false).await;
        seed(&m, "a list tag", json!(["bar"]), false).await;

        // python iterates the string: its characters are the tags
        assert_eq!(get(&m, "tags", json!({})).await, json!(["bar", "f", "o"]));
        assert_eq!(ids(&get(&m, "list", json!({"tag": "f"})).await), vec![text.clone()]);

        // a number is not iterable: /tags raises, a plain list (no tag filter) does not look at tags
        seed(&m, "a number tag", json!(5), false).await;
        let err = m.ui_route("tags", "GET", None, Map::new()).await.unwrap_err();
        assert_eq!(err.to_string(), "'int' object is not iterable");
        assert_eq!(get(&m, "list", json!({})).await[0]["tags"], json!(5));
    }

    #[tokio::test]
    async fn search_ranks_with_rounded_scores_honours_search_content_and_tag_filter() {
        let dir = tempfile::tempdir().unwrap();
        let m = module(&dir);
        let pasta = seed(&m, "pasta night on friday", json!(["dinner"]), false).await;
        let gym = seed(&m, "gym on monday", json!(["fitness"]), false).await;

        let hits = get(&m, "search", json!({"q": "pasta"})).await;
        assert_eq!(ids(&hits), vec![pasta.clone()]);
        let score = hits[0]["score"].as_f64().unwrap();
        assert_eq!((score * 100.0).round() / 100.0, score, "score is rounded to 2 places");

        // with search_content off only tags are weighted
        assert_eq!(get(&m, "search", json!({"q": "pasta", "search_content": "no"})).await, json!([]));
        assert_eq!(ids(&get(&m, "search", json!({"q": "dinner", "search_content": "no"})).await), vec![pasta.clone()]);

        // the tag filter runs before ranking; a blank query and a miss both answer an empty list
        assert_eq!(ids(&get(&m, "search", json!({"q": "monday", "tag": "fitness"})).await), vec![gym]);
        assert_eq!(get(&m, "search", json!({"q": "monday", "tag": "dinner"})).await, json!([]));
        assert_eq!(get(&m, "search", json!({"q": "  "})).await, json!([]));
        assert_eq!(get(&m, "search", json!({"q": "zebra"})).await, json!([]));
    }

    #[tokio::test]
    async fn tags_are_unique_sorted_case_insensitively_and_scoped_by_tab() {
        let dir = tempfile::tempdir().unwrap();
        let m = module(&dir);
        seed(&m, "pasta night", json!(["Food", "zeta"]), false).await;
        seed(&m, "gym", json!(["Food", "apple"]), false).await;
        seed(&m, "deadline", json!(["Work"]), true).await;

        assert_eq!(get(&m, "tags", json!({})).await, json!(["apple", "Food", "Work", "zeta"]));
        assert_eq!(get(&m, "tags", json!({"tab": "pinned"})).await, json!(["Work"]));
        assert_eq!(get(&m, "tags", json!({"tab": "search", "q": "pasta"})).await, json!(["Food", "zeta"]));
        // tab=search without a query falls back to every memory
        assert_eq!(get(&m, "tags", json!({"tab": "search"})).await, json!(["apple", "Food", "Work", "zeta"]));
    }
}
