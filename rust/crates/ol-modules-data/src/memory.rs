//! Port of modules/memory/module.py: persistent memories with pinning + BM25 search.

#[path = "../../../generated/modules/memory.rs"]
pub mod meta;

use crate::util::{body_get, now, iso, query_int, query_str, round2, truthy_opt};
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

fn tag_list(mem: &Value) -> Vec<Value> {
    match mem.get("tags") {
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    }
}

/// case-insensitive tag filter used by the webui routes
fn matches_tag(mem: &Value, tag: Option<&Value>) -> bool {
    let Some(tag) = tag.filter(|t| truthy(t)) else { return true };
    let wanted = py_str(tag).trim().to_lowercase();
    if wanted.is_empty() {
        return true;
    }
    tag_list(mem).iter().any(|t| py_str(t).trim().to_lowercase() == wanted)
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
        let limit = query_int(query, "limit", 200).clamp(1, 500);
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
            for tag in tag_list(mem) {
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
        let limit = if pinned_only { query_int(query, "limit", 0) } else { query_int(query, "limit", 100).clamp(1, 500) };
        let tag = query.get("tag");
        let mut items = Vec::new();
        for mem in self.entries()?.iter().rev() {
            if pinned_only && !truthy_opt(mem.get("pinned")) {
                continue;
            }
            if !matches_tag(mem, tag) {
                continue;
            }
            items.push(ui_item(mem, None));
            if (!pinned_only || limit != 0) && items.len() as i64 >= limit {
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
        let limit = query_int(query, "limit", 20).clamp(1, 100);
        let tag = query.get("tag");
        let entries: Vec<Value> = self.entries()?.into_iter().filter(|m| matches_tag(m, tag)).collect();
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
        let id = py_str(mem_id);
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
                let Some(id) = body_get(&body, "id").filter(truthy) else {
                    return Ok(self.base.result("no memory id given", false));
                };
                match path {
                    "update" => self.route_update(&body, &id).await,
                    _ => {
                        self.reload()?;
                        let id = py_str(&id);
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
        assert!(matches_tag(&m, Some(&json!("food"))));
        assert!(!matches_tag(&m, Some(&json!("foo"))));
        assert!(matches_tag(&m, Some(&json!(""))));
        assert!(matches_tag(&m, None));
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
