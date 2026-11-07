//! Port of modules/chats/module.py

use crate::util;
use async_trait::async_trait;
use indexmap::IndexMap;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use ol_core::pyjson::{py_str, truthy};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

#[path = "../../../generated/modules/chats.rs"]
pub mod meta;

/// Caches flattened chat history text per file, keyed by mtime, so unchanged histories skip re-reading.
static HISTORY_CACHE: Lazy<Mutex<HashMap<String, (SystemTime, String)>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// reads a chat history file and flattens its message contents into one searchable string, using the mtime cache
pub fn history_text(path: &str) -> String {
    let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified()) else { return String::new() };
    if let Some((m, t)) = HISTORY_CACHE.lock().get(path) {
        if *m == mtime {
            return t.clone();
        }
    }
    let Ok(text) = std::fs::read_to_string(path) else { return String::new() };
    let Ok(Value::Array(messages)) = serde_json::from_str::<Value>(&text) else { return String::new() };
    let mut parts = Vec::new();
    for msg in messages {
        let content = match msg.get("content") {
            Some(Value::Array(items)) => items
                .iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .map(|p| p.get("text").map(py_str).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(" "),
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        if !content.trim().is_empty() {
            parts.push(content);
        }
    }
    let t = parts.join("\n");
    HISTORY_CACHE.lock().insert(path.to_string(), (mtime, t.clone()));
    t
}

/// python `_get_categories`: `[c for c in chat.get_categories() if len(c.split(":")) == 1 and c]`
pub fn filter_categories(values: &[Value]) -> anyhow::Result<Vec<String>> {
    let mut cats = Vec::new();
    for c in values {
        let s = match c {
            Value::String(s) => s.clone(),
            other => anyhow::bail!("'{}' object has no attribute 'split'", util::py_type_name(other)),
        };
        if s.split(':').count() == 1 && !s.is_empty() {
            cats.push(s);
        }
    }
    Ok(cats)
}

/// python `search` ranking + line formatting over the flattened chat entries.
/// an empty query (python `not query`) and an empty entry list both give no lines.
pub fn search_lines(entries: &[Value], query: &str) -> Vec<String> {
    let mut weights = IndexMap::new();
    weights.insert("title".to_string(), 2.0);
    weights.insert("category".to_string(), 1.0);
    weights.insert("text".to_string(), 1.0);
    let results = ol_core::search::search(entries, query, "id", Some(&weights), 20);
    let mut lines = Vec::new();
    for hit in results {
        let e = &hit.entry;
        let mut line = format!("[{}] \"{}\" (score {:.2})", py_str(&e["category"]), py_str(&e["title"]), hit.score);
        let text = e["text"].as_str().unwrap_or("");
        if !text.is_empty() {
            if let Some(first) = ol_core::search::make_snippets(text, query, 1, 50).first() {
                line.push_str(&format!("\n  {}", first));
            }
        }
        lines.push(line);
    }
    lines
}

pub struct Chats {
    base: ModuleBase,
    disabled: Mutex<Vec<String>>,
}

impl Chats {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base, disabled: Mutex::new(Vec::new()) }
    }

    fn get_categories(&self) -> anyhow::Result<Vec<String>> {
        let ch = self.base.channel().ok_or_else(util::no_channel)?;
        filter_categories(&ch.chat.get_categories())
    }
}

#[async_trait]
impl Module for Chats {
    fn base(&self) -> &ModuleBase {
        &self.base
    }
    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }
    fn disabled_tools(&self) -> Vec<String> {
        self.disabled.lock().clone()
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        // python checks a setting that isn't in the schema, so get_categories is always disabled
        if !self.base.config.get_bool("insert_system_prompt") {
            self.disabled.lock().push("get_categories".into());
        }
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        let mut output = Vec::new();
        if self.base.config.get_bool("insert_chat_id") {
            let ch = self.base.channel().ok_or_else(util::no_channel)?;
            match ch.chat.get("id") {
                Some(Value::String(id)) => output.push(format!("current chat ID: {}", id)),
                other => anyhow::bail!(
                    "can only concatenate str (not \"{}\") to str",
                    other.as_ref().map(util::py_type_name).unwrap_or("NoneType")
                ),
            }
        }
        if self.base.config.get_bool("insert_categories") {
            let cats = self.get_categories()?;
            if cats.len() > 1 {
                output.push(format!("Available categories to categorise chat into: {}", cats.join(", ")));
            }
        }
        if output.is_empty() {
            return Ok(None);
        }
        Ok(Some(output.join("\n\n")))
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "get_categories" => {
                let cats = self.get_categories()?;
                if cats.is_empty() {
                    return Ok(self.base.result(json!("There are no categories yet. Create one!"), true));
                }
                Ok(self.base.result(json!(cats), true))
            }
            "organize" => {
                let new_name = args.raw("new_name").cloned().unwrap_or(Value::Null);
                if !truthy(&new_name) {
                    return Ok(self.base.result(json!("name must not be blank"), false));
                }
                let category = args.raw("category").cloned().unwrap_or(Value::Null);
                let tags = args.raw("tags").cloned().unwrap_or(json!([]));
                let ch = self.base.channel().ok_or_else(util::no_channel)?;
                ch.chat.set("title", new_name, None)?;
                ch.chat.set("category", category, None)?;
                ch.chat.set("tags", tags, None)?;
                Ok(self.base.result(json!("chat organised!"), true))
            }
            "search" => {
                // python passes query=None through; core search returns [] for `not query`
                let query = args.str("query").unwrap_or_default();
                let ch = self.base.channel().ok_or_else(util::no_channel)?;
                let mut entries = Vec::new();
                for meta in ch.chat.get_all() {
                    let Some(chat_id) = meta.get("id").filter(|v| truthy(v)).map(py_str) else { continue };
                    let title = meta.get("title").filter(|v| truthy(v)).cloned().unwrap_or(json!(""));
                    let category = meta.get("category").filter(|v| truthy(v)).cloned().unwrap_or(json!(""));
                    let path = ol_core::paths::get_data_path(Some(&format!("{}/history/{}.json", ch.chat.path, chat_id)))?;
                    let text = history_text(&path);
                    if !truthy(&title) && text.is_empty() {
                        continue;
                    }
                    entries.push(json!({"id": chat_id, "title": title, "category": category, "text": text}));
                }
                let lines = search_lines(&entries, &query);
                if lines.is_empty() {
                    return Ok(self.base.result(json!("no results found"), true));
                }
                Ok(self.base.result(json!(lines.join("\n")), true))
            }
            _ => anyhow::bail!("'Chats' object has no attribute '{}'", method),
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
    fn history_text_flattens_string_and_text_parts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        let msgs = json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "a"},
                {"type": "image", "url": "x"},
                {"type": "text", "text": "b"}
            ]},
            {"role": "user", "content": "   "},
            {"role": "user", "content": null},
            {"role": "system"}
        ]);
        std::fs::write(&path, msgs.to_string()).unwrap();
        assert_eq!(history_text(path.to_str().unwrap()), "hello\na b");
    }

    #[test]
    fn history_text_is_empty_for_missing_or_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert_eq!(history_text(missing.to_str().unwrap()), "");
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert_eq!(history_text(bad.to_str().unwrap()), "");
    }

    #[test]
    fn categories_drop_nested_and_blank_names() {
        let vals = vec![json!("work"), json!("work:sub"), json!(""), json!("home")];
        assert_eq!(filter_categories(&vals).unwrap(), vec!["work".to_string(), "home".to_string()]);
    }

    #[test]
    fn search_lines_format_category_title_score_and_snippet() {
        let entries = vec![json!({"id": "c1", "title": "Diet", "category": "food", "text": "I take vitamins daily"})];
        let lines = search_lines(&entries, "vitamins");
        assert_eq!(lines.len(), 1);
        let parts: Vec<&str> = lines[0].split('\n').collect();
        assert_eq!(parts.len(), 2, "{}", lines[0]);
        assert!(parts[0].starts_with("[food] \"Diet\" (score "), "{}", parts[0]);
        assert!(parts[0].ends_with(')'), "{}", parts[0]);
        assert_eq!(parts[1], "  I take vitamins daily");
        assert!(search_lines(&entries, "").is_empty());
        assert!(search_lines(&entries, "   ").is_empty());
        assert!(search_lines(&entries, "zebra").is_empty());
    }

    #[test]
    fn tool_names_match_python_class() {
        let names: Vec<String> = Chats::new(ol_core::module::ModuleBase::detached("chats")).tools().into_iter().map(|t| t.method.to_string()).collect();
        assert_eq!(names, vec!["get_categories", "organize", "search"]);
    }

    #[tokio::test]
    async fn organize_rejects_blank_name_with_python_message() {
        let m = Chats::new(ol_core::module::ModuleBase::detached("chats"));
        let args = ol_core::module::ToolArgs::new(json!({"new_name": "", "category": "x"}).as_object().unwrap().clone());
        let out = m.call_tool("organize", args).await.unwrap();
        assert_eq!(out, json!({"status": "error", "content": "name must not be blank"}));
    }
}
