//! Port of core/chat.py (Chat) and core/messages.py (Messages).
//!
//! Storage layout (identical to python):
//! - `data/chats/<channel>/index.mp`            msgpack list of chat metadata dicts
//! - `data/chats/<channel>/history/<id>.json`   json list of messages
//! - `data/chats/<channel>/current`             index of the current chat (plain text)

use crate::channel::Channel;
use crate::config;
use crate::globals::{unix_time, utcnow_isoformat};
use crate::pyjson::{self, truthy};
use crate::storage::{StorageList, StorageType};
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::SystemTime;

/// modules that should have their prompts inserted even when tools are off
pub const NONAGENTIC: [&str; 3] = ["characters", "writing_style", "time"];

struct ChatInner {
    data: StorageList,
    current: Option<usize>,
    cached_system_prompt: Option<String>,
    messages: Option<StorageList>,
    search_cache: HashMap<String, (SystemTime, String)>,
}

pub struct Chat {
    ch: once_cell::sync::OnceCell<Weak<Channel>>,
    channel_name: String,
    /// "chats/<channel>" (relative to the data folder)
    pub path: String,
    current_save_path: String,
    inner: Mutex<ChatInner>,
}

/// helper that makes sure we always get the text content as a string, even if it's multimodal
pub fn extract_content(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            for item in items {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    return item.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

impl Chat {
    pub fn new(channel_name: &str) -> anyhow::Result<Self> {
        let path = format!("chats/{}", channel_name);
        let old = crate::paths::get_data_path(Some(&format!("{}_chats.json", channel_name)))?;
        if Path::new(&old).exists() {
            migrate_if_needed(channel_name)?;
        }
        let data = StorageList::new(&format!("{}/index", path), StorageType::Msgpack, None)?;
        let current_save_path = crate::paths::get_data_path(Some(&format!("{}/current", path)))?;
        Ok(Self {
            ch: once_cell::sync::OnceCell::new(),
            channel_name: channel_name.to_string(),
            path,
            current_save_path,
            inner: Mutex::new(ChatInner { data, current: None, cached_system_prompt: None, messages: None, search_cache: HashMap::new() }),
        })
    }

    pub fn set_channel(&self, ch: Weak<Channel>) {
        let _ = self.ch.set(ch);
    }

    fn ch(&self) -> Option<Arc<Channel>> {
        self.ch.get()?.upgrade()
    }

    /// loads the last used chat if applicable, otherwise creates a new chat
    pub async fn autoload(&self) {
        if Path::new(&self.current_save_path).exists() && config::get_bool(&["core", "auto_resume_chats"], false) {
            match std::fs::read_to_string(&self.current_save_path).map(|s| s.trim().parse::<usize>()) {
                Ok(Ok(target)) => {
                    let len = self.inner.lock().data.len();
                    if target < len {
                        self.set_current(target).await;
                        return;
                    }
                }
                Ok(Err(e)) => {
                    if let Some(ch) = self.ch() {
                        ch.log_error("couldn't autoresume chat", &e.to_string());
                    }
                }
                Err(e) => {
                    if let Some(ch) = self.ch() {
                        ch.log_error("couldn't autoresume chat", &e.to_string());
                    }
                }
            }
        }
        let _ = self.new_chat("general", "New chat", None).await;
    }

    // ------------------
    // helper functions
    // ------------------

    fn active_character(&self) -> Option<Value> {
        self.get("metadata").and_then(|m| m.get("character").cloned()).filter(|v| truthy(v))
    }

    pub fn cached_system_prompt(&self) -> Option<String> {
        self.inner.lock().cached_system_prompt.clone()
    }

    pub fn invalidate_system_prompt(&self) {
        self.inner.lock().cached_system_prompt = None;
    }

    pub async fn get_system_prompt(&self) -> String {
        if let Some(c) = self.inner.lock().cached_system_prompt.clone() {
            if !c.is_empty() {
                return c;
            }
        }
        let Some(ch) = self.ch() else { return String::new() };
        let manager = ch.manager();
        if manager.pure_mode() {
            return String::new();
        }
        let active_character = self.active_character();
        let use_tools = config::get_bool(&["model", "use_tools"], false);
        let disabled_prompts = config::get_str_list(&["modules", "disabled_prompts"]);
        let modules = manager.modules();

        let mut top = Vec::new();
        let mut middle = Vec::new();
        let mut bottom = Vec::new();

        for (module_name, module) in modules.iter() {
            if !use_tools && !NONAGENTIC.contains(&module_name.as_str()) {
                continue;
            }
            if manager.is_broken(module_name) {
                continue;
            }
            let mut exempt = vec!["characters"];
            if modules.contains_key("writing_style")
                && modules.get("characters").map(|c| c.base().config.get_bool("use_writing_style")).unwrap_or(false)
            {
                exempt.push("writing_style");
            }
            if active_character.is_some() && !exempt.contains(&module_name.as_str()) && modules.contains_key("characters") {
                let disable = modules["characters"].base().config.get_bool("disable_agent_prompts_when_character_active");
                if disable {
                    continue;
                }
            }
            let sp = match module.on_system_prompt().await {
                Ok(s) => s,
                Err(e) => {
                    ch.log("module error", &format!("{}: in on_system_prompt(): {}", module_name, crate::detail_error(&e)));
                    manager.mark_broken(module_name);
                    continue;
                }
            };
            let Some(sp) = sp.filter(|s| !s.is_empty()) else { continue };
            if disabled_prompts.contains(module_name) {
                continue;
            }
            let header = module.header().filter(|h| !h.is_empty()).unwrap_or_else(|| pyjson::capitalize(&module_name.split('_').collect::<Vec<_>>().join(" ")));
            let chunk = format!("# {}\n{}", header, sp.trim());
            match module_name.as_str() {
                "agent_framework_awareness" | "identity" | "memory" | "writing_style" => top.push(chunk),
                "time" | "system" => bottom.push(chunk),
                _ => middle.push(chunk),
            }
        }
        let all: Vec<String> = top.into_iter().chain(middle).chain(bottom).collect();
        if all.is_empty() {
            return String::new();
        }
        let s = all.join("\n\n");
        self.inner.lock().cached_system_prompt = Some(s.clone());
        s
    }

    pub async fn get_end_prompt(&self, prevent_recursion: bool) -> String {
        let Some(ch) = self.ch() else { return String::new() };
        let manager = ch.manager();
        let active_character = self.active_character();
        let use_tools = config::get_bool(&["model", "use_tools"], false);
        let mut parts = Vec::new();
        for (module_name, module) in manager.modules().iter() {
            if manager.is_broken(module_name) {
                continue;
            }
            if active_character.is_some() && module_name != "characters" {
                continue;
            }
            if prevent_recursion && module_name == "token_threshold" {
                continue;
            }
            if !use_tools && !NONAGENTIC.contains(&module_name.as_str()) {
                continue;
            }
            let sp = match module.on_end_prompt().await {
                Ok(s) => s,
                Err(e) => {
                    ch.log("module error", &format!("{}: in on_end_prompt(): {}", module_name, crate::detail_error(&e)));
                    manager.mark_broken(module_name);
                    continue;
                }
            };
            if let Some(sp) = sp.filter(|s| !s.is_empty()) {
                let header = module.header().filter(|h| !h.is_empty()).unwrap_or_else(|| pyjson::capitalize(&module_name.split('_').collect::<Vec<_>>().join(" ")));
                parts.push(format!("# {}\n{}", header, sp.trim()));
            }
        }
        parts.join("\n\n")
    }

    /// load a chat and its messages by index
    async fn set_current(&self, index: usize) {
        let (old_id, new_id) = {
            let mut inner = self.inner.lock();
            let old_id = match inner.current {
                Some(c) if c < inner.data.len() => inner.data.data[c].get("id").cloned(),
                _ => None,
            };
            let new_id = inner.data.data.get(index).and_then(|c| c.get("id").cloned());
            inner.current = Some(index);
            let _ = std::fs::write(&self.current_save_path, index.to_string());
            // load this chat's messages
            let id = new_id.as_ref().and_then(|v| v.as_str()).unwrap_or("").to_string();
            inner.messages = match StorageList::new(&format!("{}/history/{}", self.path, id), StorageType::Json, None) {
                Ok(m) => Some(m),
                Err(e) => {
                    crate::log("error", &format!("Could not load chat messages: {}", e));
                    None
                }
            };
            inner.cached_system_prompt = None;
            (old_id, new_id)
        };
        if old_id != new_id {
            if let Some(ch) = self.ch() {
                ch.tool_loader.reset_for_new_chat();
                ch.tool_loader.restore_chat_tools();
            }
        }
        self.inner.lock().cached_system_prompt = None;
    }

    /// find index of the chat with that ID (case-insensitive)
    pub fn find_index(&self, id: &str) -> Option<usize> {
        let inner = self.inner.lock();
        inner.data.data.iter().position(|c| c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_uppercase() == id.to_uppercase())
    }

    pub fn current(&self) -> Option<usize> {
        self.inner.lock().current
    }

    /// the list of modules whose tools must be persisted in this chat
    pub fn get_loaded_modules(&self) -> Vec<String> {
        let inner = self.inner.lock();
        let Some(cur) = inner.current else { return vec![] };
        inner
            .data
            .data
            .get(cur)
            .and_then(|c| c.get("metadata"))
            .and_then(|m| m.get("loaded_modules"))
            .and_then(|l| l.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default()
    }

    /// persist a list of module names into this chat's metadata
    pub fn set_loaded_modules(&self, names: Vec<String>) {
        let mut inner = self.inner.lock();
        let Some(cur) = inner.current else { return };
        let Some(chat) = inner.data.data.get_mut(cur) else { return };
        if !chat.get("metadata").map(|m| m.is_object()).unwrap_or(false) {
            chat["metadata"] = json!({});
        }
        let new = json!(names);
        let existing = chat["metadata"].get("loaded_modules").cloned().unwrap_or(json!([]));
        if existing == new {
            return;
        }
        chat["metadata"]["loaded_modules"] = new;
        inner.data.save();
    }

    pub async fn update_timestamp(&self) -> anyhow::Result<()> {
        let no_current = {
            let mut inner = self.inner.lock();
            match inner.current {
                None => true,
                Some(cur) => {
                    if let Some(c) = inner.data.data.get_mut(cur) {
                        c["updated"] = json!(utcnow_isoformat());
                    }
                    false
                }
            }
        };
        if no_current {
            anyhow::bail!("No chat is currently loaded!");
        }
        self.save().await;
        Ok(())
    }

    // ------------------
    // Chat Manipulation
    // ------------------

    /// create a new chat, returns its id
    pub async fn new_chat(&self, category: &str, title: &str, metadata: Option<Value>) -> String {
        let now = utcnow_isoformat();
        let ulid = crate::globals::new_ulid();
        let new_id = ulid[ulid.len() - 8..].to_string();
        let index = {
            let mut inner = self.inner.lock();
            inner.data.data.push(json!({
                "id": new_id,
                "title": title,
                "category": category,
                "tags": [],
                "token_usage": 0,
                "token_usage_mark": 0,
                "metadata": metadata.unwrap_or_else(|| json!({})),
                "created": now,
                "updated": now,
            }));
            inner.data.len() - 1
        };
        self.set_current(index).await;
        if let Some(ch) = self.ch() {
            let tokens = ch.context.get_total_tokens().await;
            let _ = self.set("token_usage", json!(tokens), None);
        }
        self.inner.lock().data.save();
        new_id
    }

    pub async fn clear(&self) -> anyhow::Result<bool> {
        if self.current().is_none() {
            anyhow::bail!("No chat is currently loaded!");
        }
        if let Some(ch) = self.ch() {
            ch.tool_loader.reset_for_new_chat();
        }
        self.set_loaded_modules(vec![]);
        self.messages_clear();
        let _ = self.set("token_usage", json!(0), None);
        {
            let mut inner = self.inner.lock();
            if let Some(cur) = inner.current {
                if let Some(c) = inner.data.data.get_mut(cur) {
                    c["token_usage_mark"] = json!(0);
                }
            }
        }
        self.save().await;
        Ok(true)
    }

    /// delete an entire chat. returns the new current index
    pub async fn delete(&self, id: &str) -> Option<usize> {
        let index = self.find_index(id)?;
        if let Ok(p) = crate::paths::get_data_path(Some(&format!("chats/{}/history/{}.json", self.channel_name, id))) {
            let _ = std::fs::remove_file(p);
        }
        let (current, empty) = {
            let mut inner = self.inner.lock();
            inner.data.data.remove(index);
            inner.data.save();
            (inner.current, inner.data.is_empty())
        };
        if let Some(cur) = current {
            if cur == index {
                if !empty {
                    let len = self.inner.lock().data.len();
                    self.set_current(index.min(len - 1)).await;
                } else {
                    Box::pin(self.autoload()).await;
                }
            } else if cur > index {
                self.set_current(cur - 1).await;
            }
        }
        self.current()
    }

    pub async fn save(&self) -> bool {
        if self.current().is_none() {
            Box::pin(self.new_chat("general", "New chat", None)).await;
        }
        self.inner.lock().data.save()
    }

    /// save the chat index without the current-chat check
    pub fn save_index(&self) -> bool {
        self.inner.lock().data.save()
    }

    /// load a chat by id. Ok(false) if it was already loaded
    pub async fn load(&self, id: &str) -> anyhow::Result<bool> {
        let Some(index) = self.find_index(id) else {
            anyhow::bail!("tried to load a blank chat id!");
        };
        if self.current() == Some(index) {
            return Ok(false);
        }
        self.set_current(index).await;
        Ok(true)
    }

    // ----------------
    // Data Retrieval
    // ----------------

    /// exports the current chat to a human readable string
    pub async fn export(&self) -> anyhow::Result<String> {
        if self.current().is_none() {
            anyhow::bail!("No chat is currently loaded!");
        }
        let turns = crate::turns::group_history(&self.messages_get());
        let mut turn_export = Vec::new();
        for turn in turns {
            let mut lines = vec![format!("--- {} ---", pyjson::py_str_opt(turn.get("role")))];
            let mut items: Vec<(&str, String)> = Vec::new();
            for message in turn["messages"].as_array().cloned().unwrap_or_default() {
                let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
                let content = message.get("content");
                let toolcalls = message.get("tool_calls").filter(|t| truthy(t));
                if content.map(truthy).unwrap_or(false) && role != "tool" && toolcalls.is_none() {
                    match content.unwrap() {
                        Value::String(s) => items.push(("content", s.trim().to_string())),
                        Value::Array(parts) => {
                            for part in parts {
                                if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    items.push(("content", part.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string()));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(Value::Array(tcs)) = toolcalls {
                    for tc in tcs {
                        items.push(("tool", crate::toolcalls::display_call(tc)));
                    }
                }
            }
            let mut blocks: Vec<Vec<String>> = Vec::new();
            let mut cur_type: Option<&str> = None;
            let mut cur_items: Vec<String> = Vec::new();
            for (t, c) in items {
                if Some(t) != cur_type {
                    if !cur_items.is_empty() {
                        blocks.push(std::mem::take(&mut cur_items));
                    }
                    cur_type = Some(t);
                }
                cur_items.push(c);
            }
            if !cur_items.is_empty() {
                blocks.push(cur_items);
            }
            let formatted: Vec<String> = blocks.iter().map(|b| b.join("\n")).collect();
            lines.push(formatted.join("\n\n"));
            turn_export.push(lines.join("\n"));
        }
        Ok(turn_export.join("\n\n"))
    }

    /// chat.get(key): value of the current chat's field
    pub fn get(&self, key: &str) -> Option<Value> {
        let inner = self.inner.lock();
        let cur = inner.current?;
        inner.data.data.get(cur)?.get(key).cloned()
    }

    pub fn get_str(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|v| v.as_str().map(|s| s.to_string()))
    }

    /// chat.get(): the whole current chat dict
    pub fn get_chat(&self) -> Value {
        let inner = self.inner.lock();
        inner.current.and_then(|c| inner.data.data.get(c).cloned()).unwrap_or(json!({}))
    }

    /// chat.get(key, index=i)
    pub fn get_at(&self, index: usize, key: &str) -> Option<Value> {
        self.inner.lock().data.data.get(index)?.get(key).cloned()
    }

    pub fn set(&self, key: &str, value: Value, index: Option<usize>) -> anyhow::Result<bool> {
        let mut inner = self.inner.lock();
        let idx = match index.or(inner.current) {
            Some(i) => i,
            None => anyhow::bail!("No chat is currently loaded!"),
        };
        let Some(chat) = inner.data.data.get_mut(idx).and_then(|c| c.as_object_mut()) else {
            anyhow::bail!("No chat is currently loaded!");
        };
        if chat.contains_key(key) {
            chat.insert(key.to_string(), value);
            return Ok(true);
        }
        anyhow::bail!("{} is not a valid chat property", key)
    }

    /// snapshot of the chat index (all chat metadata dicts)
    pub fn data(&self) -> Vec<Value> {
        self.inner.lock().data.data.clone()
    }

    /// mutate the chat index in place (call save_index afterwards to persist)
    pub fn with_data_mut<R>(&self, f: impl FnOnce(&mut Vec<Value>) -> R) -> R {
        let mut inner = self.inner.lock();
        f(&mut inner.data.data)
    }

    /// all chats sorted by updated date (most recent first)
    pub fn get_all(&self) -> Vec<Value> {
        let mut all = self.data();
        all.sort_by(|a, b| {
            let ua = a.get("updated").and_then(|u| u.as_str()).unwrap_or("");
            let ub = b.get("updated").and_then(|u| u.as_str()).unwrap_or("");
            ub.cmp(ua)
        });
        all
    }

    pub fn get_categories(&self) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        for chat in self.data() {
            let c = chat.get("category").cloned().unwrap_or(Value::Null);
            if !out.contains(&c) {
                out.push(c);
            }
        }
        out
    }

    fn history_text(&self, chat_id: &str) -> String {
        let Ok(path) = crate::paths::get_data_path(Some(&format!("{}/history/{}.json", self.path, chat_id))) else { return String::new() };
        let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else { return String::new() };
        if let Some((m, t)) = self.inner.lock().search_cache.get(&path) {
            if *m == mtime {
                return t.clone();
            }
        }
        let Ok(text) = std::fs::read_to_string(&path) else { return String::new() };
        let Ok(Value::Array(messages)) = serde_json::from_str::<Value>(&text) else { return String::new() };
        let mut parts = Vec::new();
        for m in messages {
            if m.get("role").and_then(|r| r.as_str()) == Some("tool") {
                continue;
            }
            let content = match m.get("content") {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .map(|p| p.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            if !content.trim().is_empty() {
                parts.push(content);
            }
        }
        let t = parts.join("\n");
        self.inner.lock().search_cache.insert(path, (mtime, t.clone()));
        t
    }

    /// search across all chats, ranked by BM25
    pub fn search(&self, query: &str, max_results: usize, search_in_content: bool) -> Vec<Value> {
        if query.trim().is_empty() {
            return vec![];
        }
        let mut entries = Vec::new();
        let mut metas: HashMap<String, Value> = HashMap::new();
        for meta in self.data() {
            let Some(id) = meta.get("id").and_then(|i| i.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string()) else { continue };
            let title = meta.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let category = meta.get("category").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let text = if search_in_content { self.history_text(&id) } else { String::new() };
            if title.is_empty() && text.is_empty() {
                continue;
            }
            metas.insert(id.clone(), meta);
            entries.push(json!({"id": id, "title": title, "category": category, "text": text}));
        }
        let mut weights = IndexMap::new();
        weights.insert("title".to_string(), 2.0);
        weights.insert("category".to_string(), 1.0);
        if search_in_content {
            weights.insert("text".to_string(), 1.0);
        }
        let ranked = crate::search::search(&entries, query, "id", Some(&weights), max_results);
        let q = query.trim().to_lowercase();
        let mut found = Vec::new();
        for hit in ranked {
            let id = hit.entry["id"].as_str().unwrap_or("").to_string();
            let mut f = metas.get(&id).cloned().unwrap_or(json!({}));
            if !q.is_empty() && hit.entry["title"].as_str().unwrap_or("").to_lowercase().contains(&q) {
                f["title_match"] = json!(true);
            }
            let text = hit.entry["text"].as_str().unwrap_or("");
            if !text.is_empty() {
                let snippets = crate::search::make_snippets(text, query, 3, 50);
                if !snippets.is_empty() {
                    f["messages_found"] = json!(snippets.len());
                    f["message_snippets"] = json!(snippets);
                }
            }
            found.push(f);
        }
        found
    }

    // ------------------
    // Messages (core/messages.py)
    // ------------------

    /// get message history of current chat
    pub fn messages_get(&self) -> Vec<Value> {
        let mut inner = self.inner.lock();
        match inner.messages.as_mut() {
            Some(m) => {
                if !crate::storage::temporary() {
                    m.load();
                }
                m.data.clone()
            }
            None => vec![],
        }
    }

    pub fn messages_len(&self) -> usize {
        self.inner.lock().messages.as_ref().map(|m| m.len()).unwrap_or(0)
    }

    pub fn messages_get_at(&self, index: usize) -> anyhow::Result<Value> {
        let inner = self.inner.lock();
        inner.messages.as_ref().and_then(|m| m.data.get(index).cloned()).ok_or_else(|| anyhow::anyhow!("Invalid message index"))
    }

    /// saves messages and updates the chat's timestamp
    pub async fn messages_save(&self) -> bool {
        let _ = self.update_timestamp().await;
        let mut inner = self.inner.lock();
        inner.messages.as_mut().map(|m| m.save()).unwrap_or(false)
    }

    /// add message to current chat (Messages.add)
    pub async fn messages_add(&self, message: &Value, cmd: bool, ghost: bool) -> bool {
        let Some(ch) = self.ch() else { return false };
        let mut new_message = message.clone();
        if !new_message.is_object() {
            return false;
        }
        if !new_message.get("_metadata").map(|m| m.is_object()).unwrap_or(false) {
            new_message["_metadata"] = json!({});
        }
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("").to_string();

        let title = self.get_str("title").unwrap_or_default();
        if (title.is_empty() || title == "New chat") && role == "user" && !cmd {
            let is_str_like = matches!(new_message.get("content"), Some(Value::String(_)) | Some(Value::Array(_)));
            let content = extract_content(&new_message);
            if is_str_like || message.get("content").is_none() {
                let t = if pyjson::char_len(&content) > 100 { format!("{}..", pyjson::take_chars(&content, 100)) } else { content };
                let _ = self.set("title", json!(t), None);
            }
        }

        new_message["_metadata"]["timestamp"] = json!(unix_time());
        if ghost {
            new_message["_metadata"]["ghost"] = json!(true);
        }
        if cmd {
            new_message["_metadata"]["is_cmd"] = json!(true);
        }

        if role == "user" {
            let mut injections = Vec::new();
            for (_, module) in ch.manager().modules().iter() {
                match module.on_message_inject().await {
                    Ok(Some(inj)) if !inj.is_empty() => injections.push(inj),
                    Ok(_) => {}
                    Err(e) => ch.log("module error", &format!("{}: in on_message_inject(): {}", module.name(), crate::detail_error(&e))),
                }
            }
            if !injections.is_empty() {
                new_message["_metadata"]["injection"] = json!(injections.join("\n\n"));
            }
        }

        if role == "user" && !cmd && Box::pin(ch.context.is_over_threshold()).await {
            Box::pin(ch.context.compress()).await;
        }

        {
            let mut inner = self.inner.lock();
            match inner.messages.as_mut() {
                Some(m) => m.data.push(new_message),
                None => return false,
            }
        }
        self.messages_save().await;
        true
    }

    /// edit message by its index
    pub async fn messages_edit(&self, index: usize, message: Value) -> bool {
        {
            let mut inner = self.inner.lock();
            let Some(m) = inner.messages.as_mut() else { return false };
            if index >= m.len() {
                return false;
            }
            m.data[index] = message;
        }
        self.messages_save().await;
        true
    }

    /// delete message from current chat
    pub async fn messages_delete(&self, index: usize) -> i64 {
        {
            let mut inner = self.inner.lock();
            if let Some(m) = inner.messages.as_mut() {
                if index < m.len() {
                    m.data.remove(index);
                }
            }
        }
        self.messages_save().await;
        self.messages_len() as i64 - 1
    }

    /// deletes the message at the given index and all messages after it
    pub async fn messages_delete_from(&self, index: usize) -> anyhow::Result<bool> {
        {
            let mut inner = self.inner.lock();
            let Some(m) = inner.messages.as_mut() else { anyhow::bail!("Invalid message index") };
            if index >= m.len() {
                anyhow::bail!("Invalid message index");
            }
            m.data.truncate(index);
        }
        self.messages_save().await;
        Ok(true)
    }

    pub fn messages_clear(&self) {
        if let Some(m) = self.inner.lock().messages.as_mut() {
            m.data.clear();
        }
    }

    /// gets the latest message with the specified role (-1 if none)
    pub fn get_last_message_with_role(&self, role: &str, cutoff_index: Option<i64>) -> i64 {
        let data = self.messages_get();
        if data.is_empty() {
            return -1;
        }
        let role_of = |i: usize| data[i].get("role").and_then(|r| r.as_str()) == Some(role);
        if data.len() == 1 {
            return if role_of(0) { 0 } else { -1 };
        }
        let start = match cutoff_index {
            Some(c) => c.min(data.len() as i64 - 1),
            None => data.len() as i64 - 1,
        };
        let mut i = start;
        while i >= 0 {
            if role_of(i as usize) {
                return i;
            }
            i -= 1;
        }
        -1
    }

    /// update a field of the current chat's index entry directly (used by context for token usage)
    pub fn set_current_field(&self, key: &str, value: Value) {
        let mut inner = self.inner.lock();
        if let Some(cur) = inner.current {
            if let Some(c) = inner.data.data.get_mut(cur) {
                c[key] = value;
            }
        }
    }
}

/// Automatically migrate old format chat files if detected (`data/<channel>_chats.json`).
fn migrate_if_needed(channel_name: &str) -> anyhow::Result<()> {
    let old = crate::paths::get_data_path(Some(&format!("{}_chats.json", channel_name)))?;
    if !Path::new(&old).exists() {
        return Ok(());
    }
    println!("[MIGRATE] Old format detected for '{}', migrating...", channel_name);
    let backup_dir = crate::paths::get_data_path(Some("chat_migration_backups"))?;
    std::fs::create_dir_all(&backup_dir)?;
    let backup_name = format!("{}_chats.json.bak", channel_name);
    let backup_path = Path::new(&backup_dir).join(&backup_name);
    if let Err(e) = std::fs::copy(&old, &backup_path) {
        crate::log("core", &format!("FATAL ERROR: Could not back up chats file for channel {}. Aborting: {}", channel_name, e));
        std::process::exit(1);
    }
    println!("[MIGRATE] Backed up old file to {}", backup_name);
    let old_chats: Value = serde_json::from_str(&std::fs::read_to_string(&old)?)?;
    let Value::Array(old_chats) = old_chats else {
        println!("[MIGRATE] Invalid old format, skipping");
        return Ok(());
    };
    let new_dir = crate::paths::get_data_path(Some(&format!("chats/{}", channel_name)))?;
    std::fs::create_dir_all(Path::new(&new_dir).join("history"))?;
    let mut new_chats = Vec::new();
    for oc in old_chats {
        let id = oc.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
        if id.is_empty() {
            println!("skipping {}", id);
            continue;
        }
        let messages = oc.get("messages").cloned().unwrap_or(json!([]));
        let mp = Path::new(&new_dir).join("history").join(format!("{}.json", id));
        std::fs::write(mp, pyjson::dumps_opts(&messages, pyjson::DumpOpts { indent: Some(2), ensure_ascii: false, sort_keys: false }))?;
        new_chats.push(json!({
            "id": id,
            "title": oc.get("title").cloned().unwrap_or(json!("")),
            "category": oc.get("category").cloned().unwrap_or(json!("general")),
            "tags": oc.get("tags").cloned().unwrap_or(json!([])),
            "token_usage": oc.get("token_usage").cloned().unwrap_or(json!(0)),
            "token_usage_mark": oc.get("token_usage_mark").cloned().unwrap_or(json!(0)),
            "metadata": oc.get("custom_data").cloned().unwrap_or(json!({})),
            "created": oc.get("created").cloned().unwrap_or(json!("")),
            "updated": oc.get("updated").cloned().unwrap_or(json!("")),
        }));
    }
    std::fs::write(Path::new(&new_dir).join("index.mp"), crate::storage::to_msgpack(&Value::Array(new_chats.clone()))?)?;
    let old_current = crate::paths::get_data_path(Some(&format!("{}_current_chat", channel_name)))?;
    if let Ok(s) = std::fs::read_to_string(&old_current) {
        if let Ok(ci) = s.trim().parse::<usize>() {
            let safe = if new_chats.is_empty() { 0 } else { ci.min(new_chats.len() - 1) };
            let _ = std::fs::write(Path::new(&new_dir).join("current"), safe.to_string());
        }
    }
    std::fs::remove_file(&old)?;
    println!("[MIGRATE] Migrated {} chats for '{}'", new_chats.len(), channel_name);
    Ok(())
}

/// project a chat dict to a Map (helper for callers)
pub fn as_map(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}
