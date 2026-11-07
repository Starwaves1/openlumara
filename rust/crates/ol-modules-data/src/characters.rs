//! Port of modules/characters/module.py: AI character profiles (incl. sillytavern-style
//! character card import from JSON and PNG tEXt/iTXt metadata).

#[path = "../../../generated/modules/characters.rs"]
pub mod meta;

use crate::util::{b64decode, b64decode_text, body_get, body_str, dict_body, eq_ci_strip, py_json_loads, py_strip, py_type, py_utf8_decode};
use anyhow::anyhow;
use async_trait::async_trait;
use indexmap::IndexMap;
use ol_core::channel::Channel;
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::{py_str, py_str_opt, truthy};
use ol_core::storage::{StorageDict, StorageType};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------
// pure helpers (character cards)
// ---------------------------------------------------------

fn get_or<'a>(m: &'a Map<String, Value>, key: &str, default: &'a Value) -> &'a Value {
    m.get(key).unwrap_or(default)
}

fn or_value(v: Option<&Value>, fallback: Value) -> Value {
    match v {
        Some(x) if truthy(x) => x.clone(),
        _ => fallback,
    }
}

/// converts a character of any supported format into openlumara's flat internal format
pub fn normalize_character(stored_key: &str, ch: &Value) -> Option<Value> {
    normalize_card(stored_key, ch).ok().flatten()
}

/// a python TypeError raised by a builtin (json.loads on a non-str); Debug mirrors the exception
/// repr so `ol_core::detail_error` renders it the same way it renders other errors
#[derive(Debug)]
struct PyTypeError(String);

impl std::fmt::Display for PyTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// python `_normalize_character`. the error is the TypeError python raises when a truthy non-str
/// profile is concatenated with the personality or example-conversation text
pub fn normalize_card(stored_key: &str, ch: &Value) -> Result<Option<Value>, String> {
    let Some(char_obj) = ch.as_object() else { return Ok(None) };
    let empty = json!("");
    let empty_list = json!([]);
    if char_obj.contains_key("profile") {
        // already internal format; just make sure every field exists
        return Ok(Some(json!({
            "name": or_value(char_obj.get("name"), json!(stored_key)),
            "profile": get_or(char_obj, "profile", &empty),
            "scenario": get_or(char_obj, "scenario", &empty),
            "first_message": get_or(char_obj, "first_message", &empty),
            "post_history": get_or(char_obj, "post_history", &empty),
            "category": char_obj.get("category").cloned().unwrap_or(Value::Null),
            "tags": get_or(char_obj, "tags", &empty_list),
        })));
    }
    // character card V2 wraps everything in a data field, V1 is flat
    let Some(card) = char_obj.get("data").unwrap_or(ch).as_object() else { return Ok(None) };
    let name = or_value(card.get("name"), json!(stored_key));
    let mut profile = card.get("description").cloned().unwrap_or(json!(""));
    if !truthy(&profile) {
        // legacy openlumara format used 'identity' as its profile field
        profile = char_obj.get("identity").cloned().unwrap_or(json!(""));
    }
    if !truthy(&profile) {
        return Ok(None);
    }
    // fold personality and example conversations into the profile
    let mut extras = Vec::new();
    if let Some(p) = card.get("personality").filter(|v| truthy(v)) {
        extras.push(format!("Personality: {}", py_str(p)));
    }
    if let Some(m) = card.get("mes_example").filter(|v| truthy(v)) {
        extras.push(format!("Example conversation:\n{}", py_str(m)));
    }
    if !extras.is_empty() {
        // python `profile + "\n\n"`: only a str concatenates
        let Value::String(text) = &profile else {
            let t = py_type(&profile);
            return Err(if t == "list" {
                "can only concatenate list (not \"str\") to list".to_string()
            } else {
                format!("unsupported operand type(s) for +: '{}' and 'str'", t)
            });
        };
        profile = json!(format!("{}\n\n{}", text, extras.join("\n\n")));
    }
    let first_message = match card.get("first_mes") {
        Some(v) => v.clone(),
        None => char_obj.get("first_message").cloned().unwrap_or(json!("")),
    };
    let tags = match card.get("tags") {
        Some(v) => v.clone(),
        None => char_obj.get("tags").cloned().unwrap_or(json!([])),
    };
    Ok(Some(json!({
        "name": name,
        "profile": profile,
        "scenario": get_or(card, "scenario", &empty),
        "first_message": first_message,
        "post_history": get_or(card, "post_history_instructions", &empty),
        "category": char_obj.get("category").cloned().unwrap_or(Value::Null),
        "tags": tags,
    })))
}

/// extracts an embedded character card from a png's tEXt/iTXt metadata chunks
pub fn extract_card_from_png(bytes: &[u8]) -> Option<Map<String, Value>> {
    fn inner(bytes: &[u8]) -> Result<Option<Map<String, Value>>, ()> {
        if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
            return Ok(None);
        }
        let mut pos = 8;
        loop {
            if bytes.len() < pos + 8 {
                break;
            }
            let length = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            let chunk_type = &bytes[pos + 4..pos + 8];
            pos += 8;
            let end = (pos + length).min(bytes.len());
            let data = &bytes[pos..end];
            pos = (end + 4).min(bytes.len()); // crc, not verified
            if chunk_type != b"tEXt" && chunk_type != b"iTXt" {
                if chunk_type == b"IEND" {
                    break;
                }
                continue;
            }
            let (keyword, mut remainder): (&[u8], Vec<u8>) = match data.iter().position(|b| *b == 0) {
                Some(i) => (&data[..i], data[i + 1..].to_vec()),
                None => (data, Vec::new()),
            };
            if keyword != b"chara" && keyword != b"ccv2" {
                continue;
            }
            if chunk_type == b"iTXt" {
                // compression flag, compression method, language tag, translated keyword
                let compressed = remainder.first().copied().unwrap_or(0);
                remainder = remainder.get(3..).map(|r| r.to_vec()).unwrap_or_default();
                for _ in 0..2 {
                    remainder = match remainder.iter().position(|b| *b == 0) {
                        Some(i) => remainder[i + 1..].to_vec(),
                        None => Vec::new(),
                    };
                }
                if compressed == 1 {
                    use std::io::Read;
                    let mut out = Vec::new();
                    flate2::read::ZlibDecoder::new(&remainder[..]).read_to_end(&mut out).map_err(|_| ())?;
                    remainder = out;
                }
            }
            let decoded = b64decode(&remainder).map_err(|_| ())?;
            let text = String::from_utf8(decoded).map_err(|_| ())?;
            // python json.loads: accepts NaN and Infinity, so the same card is imported
            let card = py_json_loads(&text).map_err(|_| ())?;
            if let Value::Object(m) = card {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }
    inner(bytes).ok().flatten()
}

/// Replaces all occurrences of `old` with `new` in `text`, ignoring case. Mirrors the
/// python implementation exactly (indices found in the lowercased text are applied to
/// the original text).
pub fn case_insensitive_replace(text: &str, old: &str, new: &str) -> String {
    if old.is_empty() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let lower_old: Vec<char> = old.to_lowercase().chars().collect();
    let old_len = old.chars().count();
    let find = |from: usize| -> Option<usize> {
        if lower_old.len() > lower.len() {
            return None;
        }
        (from..=lower.len() - lower_old.len()).find(|&i| lower[i..i + lower_old.len()] == lower_old[..])
    };
    let slice = |a: usize, b: usize| -> String { chars[a.min(chars.len())..b.min(chars.len()).max(a.min(chars.len()))].iter().collect() };
    let mut out = String::new();
    let mut index = 0;
    loop {
        match find(index) {
            None => {
                out.push_str(&slice(index, chars.len()));
                break;
            }
            Some(found) => {
                out.push_str(&slice(index, found));
                out.push_str(new);
                index = found + old_len;
            }
        }
    }
    out
}

/// whether `old` occurs in `text` under python's lowercase comparison (an empty `old` never matches)
fn ci_contains(text: &str, old: &str) -> bool {
    let lower_old: Vec<char> = old.to_lowercase().chars().collect();
    let lower_text: Vec<char> = text.to_lowercase().chars().collect();
    !lower_old.is_empty() && lower_text.windows(lower_old.len()).any(|w| w == lower_old.as_slice())
}

/// python `_case_insensitive_replace(text, old, new)` where `new` can be any stored value. python joins
/// the pieces with `"".join`, so a non-str replacement only raises once a match inserts it
pub fn case_insensitive_replace_value(text: &str, old: &str, new: &Value) -> anyhow::Result<String> {
    if !matches!(new, Value::String(_)) && ci_contains(text, old) {
        // the first inserted replacement sits at index 1 of the join (segment, replacement, segment, ...)
        anyhow::bail!("sequence item 1: expected str instance, {} found", py_type(new));
    }
    Ok(case_insensitive_replace(text, old, &py_str(new)))
}

/// replaces the magic words defined in the character card spec, in python's order. the user name is
/// the stored value (`user_profile.get("name", "user")`), so a non-str name fails on the first user tag
pub fn replace_tags(char_name: &str, user_name: &Value, text: &str) -> anyhow::Result<String> {
    let mut out = text.to_string();
    for word in ["{{char}}", "{char}", "<BOT>"] {
        out = case_insensitive_replace(&out, word, char_name);
    }
    for word in ["{{user}}", "{user}", "<USER>"] {
        out = case_insensitive_replace_value(&out, word, user_name)?;
    }
    Ok(out)
}

/// python `obj[key] = value` on a metadata value that is not a dict
fn item_assignment_error(v: &Value) -> String {
    match v {
        Value::Array(_) => "list indices must be integers or slices, not str".into(),
        other => format!("'{}' object does not support item assignment", py_type(other)),
    }
}

/// tags arrive from the form as a comma-separated string (or a list)
fn clean_tags(raw: Option<&Value>) -> Vec<Value> {
    match raw {
        Some(Value::String(s)) => s.split(',').map(py_strip).filter(|t| !t.is_empty()).map(|t| json!(t)).collect(),
        Some(Value::Array(a)) => a.iter().map(|t| py_strip(&py_str(t)).to_string()).filter(|t| !t.is_empty()).map(Value::String).collect(),
        _ => vec![],
    }
}

/// python's numeric value of a number or bool (True is 1, False is 0)
fn py_number(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        other => other.as_f64(),
    }
}

/// python dict key for a truthy category: 1, 1.0 and true are one key (the first inserted value is
/// kept), a str never equals a number
fn category_key(v: &Value) -> String {
    match v {
        Value::String(s) => format!("s{s}"),
        other => match py_number(other) {
            Some(f) => format!("n{}", f.to_bits()),
            None => format!("o{other}"),
        },
    }
}

/// python dict key rendering in json.dumps: a bool is `true`/`false`, anything else is its str()
fn json_key_text(v: &Value) -> String {
    match v {
        Value::Bool(true) => "true".into(),
        Value::Bool(false) => "false".into(),
        other => py_str(other),
    }
}

/// python `_route_chats_count`: `counts[char] = counts.get(char, 0) + 1` for each truthy character. The dict
/// key is the stored value itself, so True, 1 and 1.0 share one entry, and the first stored value decides how
/// the key is rendered
fn chats_count_from(characters: Vec<Value>) -> anyhow::Result<Value> {
    let mut counts: IndexMap<String, (String, i64)> = IndexMap::new();
    for c in characters {
        if !truthy(&c) {
            continue;
        }
        // python uses the stored value as a dict key: a list or dict is unhashable
        if matches!(c, Value::Array(_) | Value::Object(_)) {
            anyhow::bail!("unhashable type: '{}'", py_type(&c));
        }
        counts.entry(category_key(&c)).or_insert_with(|| (json_key_text(&c), 0)).1 += 1;
    }
    let out: IndexMap<String, i64> = counts.into_values().collect();
    Ok(json!(out))
}

/// python `name.lower()`: only a str has the method, so a truthy non-str receiver raises AttributeError
fn str_for_lower(v: &Value) -> anyhow::Result<&str> {
    match v {
        Value::String(s) => Ok(s),
        other => anyhow::bail!("'{}' object has no attribute 'lower'", py_type(other)),
    }
}

/// python `self._find_char_name(name) or char.get("name")`: an empty stored key is falsy and falls back
/// to the card's own name. `char` is always truthy here, so a non-dict card raises python's `.get` error
fn canonical_name(found: Option<String>, ch: &Value) -> anyhow::Result<Value> {
    match found.filter(|n| !n.is_empty()) {
        Some(n) => Ok(json!(n)),
        None => match ch {
            Value::Object(o) => Ok(o.get("name").cloned().unwrap_or(Value::Null)),
            other => anyhow::bail!("'{}' object has no attribute 'get'", py_type(other)),
        },
    }
}

/// python `char.get("first_message", "")` on a stored card: a truthy non-dict card raises the `.get` AttributeError
fn card_first_message(ch: &Value) -> anyhow::Result<Value> {
    match ch {
        Value::Object(o) => Ok(o.get("first_message").cloned().unwrap_or(json!(""))),
        other => anyhow::bail!("'{}' object has no attribute 'get'", py_type(other)),
    }
}

/// python route_edit rename loop: every chat whose character is `key` is re-stamped to `new_name`.
/// python `chat.get("metadata") or {}`: a falsy metadata is an empty dict, a truthy non-dict raises the
/// `.get` AttributeError and stops the loop
fn restamp_chat_characters(chats: &mut [Value], key: &str, new_name: &str) -> anyhow::Result<()> {
    for chat in chats.iter_mut() {
        match chat.get_mut("metadata") {
            Some(Value::Object(meta)) => {
                if meta.get("character") == Some(&json!(key)) {
                    meta.insert("character".into(), json!(new_name));
                }
            }
            Some(other) if truthy(other) => anyhow::bail!("'{}' object has no attribute 'get'", py_type(other)),
            _ => {}
        }
    }
    Ok(())
}

/// python `_route_current`: `chat.get("metadata").get("character") or None`. a missing or non-dict
/// metadata raises the `.get` AttributeError
fn current_route_value(metadata: Option<Value>) -> anyhow::Result<Value> {
    let meta = metadata.unwrap_or(Value::Null);
    let Some(obj) = meta.as_object() else {
        anyhow::bail!("'{}' object has no attribute 'get'", py_type(&meta));
    };
    let c = obj.get("character").cloned().unwrap_or(Value::Null);
    Ok(if truthy(&c) { c } else { Value::Null })
}

/// python `a < b` for two category keys: str with str, numbers and bools with each other.
/// any other pair raises TypeError, returned as its message
fn category_order(a: &Value, b: &Value) -> Result<std::cmp::Ordering, String> {
    match (a, b) {
        (Value::String(x), Value::String(y)) => Ok(x.cmp(y)),
        _ => match (py_number(a), py_number(b)) {
            (Some(x), Some(y)) => Ok(x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)),
            _ => Err(format!("'<' not supported between instances of '{}' and '{}'", py_type(a), py_type(b))),
        },
    }
}

// ---------------------------------------------------------
// Module
// ---------------------------------------------------------

pub struct Characters {
    base: ModuleBase,
    characters: Mutex<Option<StorageDict>>,
    user_profile: Mutex<Option<StorageDict>>,
    disabled: Mutex<Vec<String>>,
}

impl Characters {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base, characters: Mutex::new(None), user_profile: Mutex::new(None), disabled: Mutex::new(Vec::new()) }
    }

    fn with_chars<R>(&self, f: impl FnOnce(&mut StorageDict) -> R) -> anyhow::Result<R> {
        let mut g = self.characters.lock();
        let s = g.as_mut().ok_or_else(|| anyhow!("'Characters' object has no attribute 'characters'"))?;
        Ok(f(s))
    }

    fn with_user<R>(&self, f: impl FnOnce(&mut StorageDict) -> R) -> anyhow::Result<R> {
        let mut g = self.user_profile.lock();
        let s = g.as_mut().ok_or_else(|| anyhow!("'Characters' object has no attribute 'user_profile'"))?;
        Ok(f(s))
    }

    /// snapshot of all characters (key -> character)
    pub fn all(&self) -> Map<String, Value> {
        self.with_chars(|c| c.data.clone()).unwrap_or_default()
    }

    fn save_chars(&self) -> anyhow::Result<()> {
        self.with_chars(|c| c.save())?
    }

    /// python StorageDict.get(key) (reloads from disk first)
    fn user_get(&self, key: &str) -> Option<Value> {
        self.with_user(|u| u.get(key).cloned()).ok().flatten()
    }

    /// searches for a character, case insensitive. python callers all test the result for truthiness
    /// (`if not char`), so the first match is a miss when its stored value is falsy (e.g. an empty dict)
    fn find_character(&self, name: &str) -> Option<Value> {
        self.all().into_iter().find(|(k, _)| eq_ci_strip(k, name)).map(|(_, v)| v).filter(truthy)
    }

    /// searches for a character and returns the stored key with correct case
    fn find_char_name(&self, name: &str) -> Option<String> {
        self.all().into_iter().find(|(k, _)| eq_ci_strip(k, name)).map(|(k, _)| k)
    }

    /// python `self.user_profile.get("name", "user")`: the stored value as-is, not coerced to a string
    fn user_name(&self) -> Value {
        self.with_user(|u| u.get("name").cloned()).ok().flatten().unwrap_or_else(|| json!("user"))
    }

    fn replace(&self, char_name: &str, text: &str) -> anyhow::Result<String> {
        replace_tags(char_name, &self.user_name(), text)
    }

    fn channel(&self) -> anyhow::Result<Arc<Channel>> {
        self.base.channel().ok_or_else(|| anyhow!("'NoneType' object has no attribute 'context'"))
    }

    /// python `self.channel.context.chat.get("metadata")`: `Chat.get` raises when no chat is loaded,
    /// and otherwise returns the metadata (None when the chat has none)
    fn chat_metadata(&self) -> anyhow::Result<Option<Value>> {
        let ch = self.channel()?;
        if ch.chat.current().is_none() {
            anyhow::bail!("No chat is currently loaded!");
        }
        Ok(ch.chat.get("metadata"))
    }

    /// python `_route_current` (see `current_route_value`)
    fn route_current(&self) -> anyhow::Result<Value> {
        current_route_value(self.chat_metadata()?)
    }

    /// `chat.get("metadata").get("character") or None` (same chain as the route)
    fn current_character(&self) -> anyhow::Result<Value> {
        current_route_value(self.chat_metadata()?)
    }

    /// `chat.get("metadata")["character"] = value` (in-memory, persisted on the next chat save). the
    /// metadata must be a dict, python raises on a missing or non-dict value
    fn set_current_character(&self, value: Value) -> anyhow::Result<()> {
        let mut meta = self.chat_metadata()?.unwrap_or(Value::Null);
        if !meta.is_object() {
            anyhow::bail!(item_assignment_error(&meta));
        }
        meta["character"] = value;
        self.channel()?.chat.set_current_field("metadata", meta);
        Ok(())
    }

    // ---------------- startup ----------------

    /// migrates the old single-file characters.json into the folder storage, if it exists
    fn migrate_if_needed(&self) -> anyhow::Result<()> {
        let old_file = ol_core::paths::get_data_path(Some("characters.json"))?;
        if !Path::new(&old_file).exists() {
            return Ok(());
        }
        let folder = self.with_chars(|c| c.path.clone())?;
        let has_json = |folder: &str| -> usize {
            std::fs::read_dir(folder)
                .map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".json")).count())
                .unwrap_or(0)
        };
        if Path::new(&folder).exists() && has_json(&folder) > 0 {
            ol_core::log("warning", "old characters.json found but folder storage already contains characters, skipping migration");
            return Ok(());
        }
        println!("[MIGRATE] Old single-file character storage detected, migrating...");
        let backup_dir = ol_core::paths::get_data_path(Some("character_migration_backups"))?;
        std::fs::create_dir_all(&backup_dir)?;
        let backup_path = Path::new(&backup_dir).join("characters.json.bak");
        let copied = std::fs::copy(&old_file, &backup_path).map_err(|e| e.to_string()).and_then(|_| {
            if backup_path.exists() {
                Ok(())
            } else {
                Err("Backup file not created".to_string())
            }
        });
        match copied {
            Ok(()) => println!("[MIGRATE] Backed up old character storage to {}", backup_path.to_string_lossy()),
            Err(e) => {
                ol_core::log("error", &format!("FATAL ERROR: could not back up characters.json before migrating. Aborting: {}", e));
                std::process::exit(1);
            }
        }
        let old_chars: Value = match std::fs::read_to_string(&old_file).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) {
            Ok(v) => v,
            Err(e) => {
                ol_core::log("error", &format!("could not read old character storage, leaving it in place: {}", e));
                return Ok(());
            }
        };
        let Value::Object(old_chars) = old_chars else {
            ol_core::log("error", "old character storage is not a dict, leaving it in place");
            return Ok(());
        };
        let mut migrated = Map::new();
        for (name, ch) in &old_chars {
            match normalize_character(name, ch) {
                Some(n) => {
                    migrated.insert(name.clone(), n);
                }
                None => ol_core::log("warning", &format!("skipping unreadable character '{}' during migration (preserved in backup)", name)),
            }
        }
        let count = migrated.len();
        self.with_chars(|c| {
            for (k, v) in migrated {
                c.data.insert(k, v);
            }
            c.save()
        })??;
        if has_json(&folder) < count {
            ol_core::log("error", "migration verification failed, old characters.json left in place");
            return Ok(());
        }
        std::fs::remove_file(&old_file)?;
        println!("[MIGRATE] migrated {} characters to folder storage", count);
        Ok(())
    }

    /// converts any card-pngs in the characters folder into regular json characters
    fn ingest_pngs(&self) -> anyhow::Result<()> {
        let folder = self.with_chars(|c| c.path.clone())?;
        let Ok(rd) = std::fs::read_dir(&folder) else { return Ok(()) };
        let mut changed = false;
        for e in rd.flatten() {
            let filename = e.file_name().to_string_lossy().to_string();
            if !filename.to_lowercase().ends_with(".png") {
                continue;
            }
            let Ok(bytes) = std::fs::read(e.path()) else { continue };
            let Some(card) = extract_card_from_png(&bytes) else { continue };
            let name: String = filename.chars().take(filename.chars().count() - 4).collect();
            let normalized = normalize_character(&name, &Value::Object(card));
            let Some(mut normalized) = normalized.filter(|n| n.get("profile").map(truthy).unwrap_or(false)) else {
                ol_core::log("warning", &format!("png '{}' had no usable character card, skipping", filename));
                continue;
            };
            if !normalized.get("name").map(truthy).unwrap_or(false) {
                normalized["name"] = json!(name);
            }
            let cname = py_str(&normalized["name"]);
            if self.find_character(&cname).is_some() {
                continue;
            }
            self.with_chars(|c| c.data.insert(cname, normalized))?;
            changed = true;
        }
        if changed {
            self.save_chars()?;
        }
        Ok(())
    }

    /// normalizes all stored characters, so manually dropped-in card files also work
    fn normalize_all(&self) -> anyhow::Result<()> {
        let changed = self.with_chars(|c| {
            let mut changed = false;
            let keys: Vec<String> = c.data.keys().cloned().collect();
            for k in keys {
                let ch = c.data[&k].clone();
                if let Some(n) = normalize_character(&k, &ch) {
                    if n != ch {
                        c.data.insert(k, n);
                        changed = true;
                    }
                }
            }
            changed
        })?;
        if changed {
            self.save_chars()?;
        }
        Ok(())
    }

    // ---------------- commands / tools ----------------

    /// list all your characters
    pub fn list_characters(&self) -> String {
        let all = self.all();
        if all.is_empty() {
            return "You have no characters yet".into();
        }
        let mut by_cat: IndexMap<String, Vec<String>> = IndexMap::new();
        for (name, ch) in &all {
            let cat = ch.get("category").filter(|c| truthy(c)).map(py_str).unwrap_or_else(|| "unsorted".into());
            by_cat.entry(cat).or_default().push(name.clone());
        }
        by_cat.iter().map(|(cat, names)| format!("{}: {}", cat, names.join(", "))).collect::<Vec<_>>().join("\n")
    }

    async fn cmd_switch(&self, args: &[String]) -> anyhow::Result<String> {
        let name = args.join(" ");
        if name.is_empty() {
            let ch = self.current_character()?;
            return Ok(if truthy(&ch) { format!("currently active character: {}", py_str(&ch)) } else { "please provide a character name.".into() });
        } else if name == "reset" || name == "default" {
            self.set_current_character(json!(""))?;
            return Ok("character has been reset to default".into());
        }
        let Some(character) = self.find_character(&name) else {
            return Ok(format!("character {} does not exist!", name));
        };
        let char_name = self.find_char_name(&name).unwrap_or_default();
        self.switch(&char_name).await?;
        let first = character.get("first_message").cloned().unwrap_or(json!(""));
        if truthy(&first) && self.base.config.get_bool("use_first_messages") {
            let first = self.replace(&char_name, str_for_lower(&first)?)?;
            return Ok(format!("character switched to {}\n\n{}", char_name, first));
        }
        Ok(format!("character switched to {}", char_name))
    }

    pub async fn switch(&self, name: &str) -> anyhow::Result<Value> {
        let Some(ch) = self.find_character(name) else {
            return Ok(self.base.result("character not found", false));
        };
        let char_name = canonical_name(self.find_char_name(name), &ch)?;
        self.set_current_character(char_name.clone())?;
        let char_name = py_str(&char_name);
        // python `char.get("first_message", "")` runs after the metadata is set, so a non-dict card raises here
        let first = card_first_message(&ch)?;
        if truthy(&first) && self.base.config.get_bool("use_first_messages") {
            let first = self.replace(&char_name, str_for_lower(&first)?)?;
            return Ok(self.base.result(
                format!("Switched to {}. Begin your reply now with this exact greeting, verbatim, and add nothing else:\n\n{}", char_name, first),
                true,
            ));
        }
        Ok(self.base.result("Switch successful. Write your response as the character's first message.", true))
    }

    async fn add(&self, args: &ToolArgs) -> anyhow::Result<Value> {
        let name = args.str_or("name", "");
        if name.trim().is_empty() {
            return Ok(self.base.result("character name cannot be empty", false));
        }
        let tags = args.raw("tags").cloned().unwrap_or(json!([]));
        if self.find_character(&name).is_some() {
            return Ok(self.base.result("character already exists", false));
        }
        let profile = args.0.get("profile").cloned().unwrap_or(Value::Null);
        if !truthy(&profile) {
            return Ok(self.base.result("character profile must not be blank.", true));
        }
        let category = args.raw("category").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null);
        let entry = json!({
            "name": name,
            "profile": profile,
            "scenario": args.0.get("scenario").cloned().unwrap_or(json!("")),
            "first_message": "",
            "post_history": "",
            "category": category,
            "tags": tags,
        });
        self.with_chars(|c| {
            c.data.insert(name.clone(), entry);
            c.save()
        })??;
        Ok(self.base.result("character added", true))
    }

    async fn edit(&self, args: &ToolArgs) -> anyhow::Result<Value> {
        let name = args.str_or("name", "");
        if name.trim().is_empty() {
            return Ok(self.base.result("character name cannot be empty", false));
        }
        let Some(ch) = self.find_character(&name) else {
            return Ok(self.base.result("character doesn't exist!", false));
        };
        let canonical = self.find_char_name(&name).unwrap_or_default();
        let pick = |k: &str, existing: &str, default: Value| -> Value {
            match args.raw(k) {
                Some(v) => v.clone(),
                None => ch.get(existing).cloned().unwrap_or(default),
            }
        };
        let entry = json!({
            "name": canonical,
            "profile": pick("profile", "profile", json!("")),
            "scenario": pick("scenario", "scenario", json!("")),
            "first_message": pick("first_message", "first_message", json!("")),
            "post_history": pick("post_history", "post_history", json!("")),
            "category": pick("category", "category", Value::Null),
            "tags": pick("tags", "tags", json!([])),
        });
        self.with_chars(|c| {
            c.data.insert(canonical.clone(), entry);
            c.save()
        })??;
        Ok(self.base.result("character edited", true))
    }

    async fn delete(&self, name: &str) -> anyhow::Result<Value> {
        if let Some(key) = self.find_char_name(name) {
            self.with_chars(|c| {
                c.data.shift_remove(&key);
                c.save()
            })??;
            return Ok(self.base.result(format!("character {} deleted", key), true));
        }
        Ok(self.base.result("character doesn't exist!", false))
    }

    async fn set_user_persona(&self, name: Value, profile: Value) -> anyhow::Result<Value> {
        self.with_user(|u| {
            u.set("name", name);
            u.set("profile", profile);
            u.save()
        })??;
        Ok(self.base.result("user persona set", true))
    }

    /// python `existing_name = self._find_char_name(name)` then `key = existing_name or name`, for the
    /// normalized name of an imported card. a non-str name only reaches python's dict when the store is
    /// empty, because the lookup calls `.lower()` on it for every stored key
    fn card_storage_key(&self, name: &Value) -> anyhow::Result<String> {
        match name {
            Value::String(card_name) => Ok(self.find_char_name(card_name).filter(|k| !k.is_empty()).unwrap_or_else(|| card_name.clone())),
            other => {
                if !self.all().is_empty() {
                    anyhow::bail!("'{}' object has no attribute 'lower'", py_type(other));
                }
                // python `self.characters[name] = ...` with an empty store: a list or dict is unhashable
                if matches!(other, Value::Array(_) | Value::Object(_)) {
                    anyhow::bail!("unhashable type: '{}'", py_type(other));
                }
                // a truthy scalar name (int, float, bool) is stored under its text; python keeps the
                // number as the dict key, but every later name lookup or /all sort raises on `.lower()`
                Ok(py_str(other))
            }
        }
    }

    pub async fn import_json(&self, json_code: &str) -> anyhow::Result<Value> {
        let obj: Value = match py_json_loads(json_code) {
            Ok(v) => v,
            // python import_json: `core.detail_error(e)`
            Err(e) => return Ok(self.base.result(format!("error: {}", ol_core::detail_error(&e)), false)),
        };
        if !obj.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            return Ok(self.base.result("error: character card was empty", false));
        }
        let normalized = normalize_card("", &obj).map_err(anyhow::Error::msg)?;
        let normalized = normalized
            .filter(|n| n.get("name").map(truthy).unwrap_or(false) && n.get("profile").map(truthy).unwrap_or(false));
        let Some(normalized) = normalized else {
            return Ok(self.base.result("error: failed to extract a usable name and profile from this character card", false));
        };
        let key = self.card_storage_key(&normalized["name"])?;
        self.with_chars(|c| {
            c.data.insert(key, normalized);
            c.save()
        })??;
        Ok(self.base.result("character successfully imported", true))
    }

    // ---------------- webui routes ----------------

    /// python `_route_list`: `char.get("category") or "general"` keeps a truthy category as its raw value
    fn route_list(&self) -> anyhow::Result<Value> {
        let mut grouped: IndexMap<String, (Value, Vec<Value>)> = IndexMap::new();
        for (name, ch) in self.all() {
            // python `char.get(...)` on a stored value that is not a dict raises
            if !ch.is_object() {
                anyhow::bail!("'{}' object has no attribute 'get'", py_type(&ch));
            }
            let cat = match ch.get("category") {
                Some(c) if truthy(c) => c.clone(),
                _ => json!("general"),
            };
            // python dict keys must be hashable
            if matches!(cat, Value::Array(_) | Value::Object(_)) {
                anyhow::bail!("unhashable type: '{}'", py_type(&cat));
            }
            grouped.entry(category_key(&cat)).or_insert_with(|| (cat.clone(), Vec::new())).1.push(json!({
                "name": or_value(ch.get("name"), json!(name)),
                "tags": or_value(ch.get("tags"), json!([])),
            }));
        }
        let mut cats: Vec<(Value, Vec<Value>)> = grouped.into_values().collect();
        // python sorted(grouped.items()): categories compare with `<`, a mixed str/number pair raises
        let mut order_err = None;
        cats.sort_by(|a, b| match category_order(&a.0, &b.0) {
            Ok(o) => o,
            Err(e) => {
                order_err.get_or_insert(e);
                std::cmp::Ordering::Equal
            }
        });
        if let Some(e) = order_err {
            anyhow::bail!(e);
        }
        // python computes every member's sort key (`c["name"].lower()`) before sorting, so a
        // non-str name raises even in a one-member category
        let mut categories = Vec::with_capacity(cats.len());
        for (cat, chars) in cats {
            let mut keyed = Vec::with_capacity(chars.len());
            for c in chars {
                let key = str_for_lower(&c["name"])?.to_lowercase();
                keyed.push((key, c));
            }
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            let chars: Vec<Value> = keyed.into_iter().map(|(_, c)| c).collect();
            categories.push(json!({"name": cat, "characters": chars}));
        }
        Ok(json!({"categories": categories}))
    }

    /// python `_route_all`: a stored value that is not a dict raises at `.get`, and the sort key
    /// `c["name"].lower()` raises for a non-str name
    fn route_all(&self) -> anyhow::Result<Value> {
        // python builds every entry first (`.get` raises for a non-dict), and only then sorts
        let mut built: Vec<(Value, Value)> = Vec::new();
        for (key, ch) in self.all() {
            if !ch.is_object() {
                anyhow::bail!("'{}' object has no attribute 'get'", py_type(&ch));
            }
            let name = or_value(ch.get("name"), json!(key));
            let entry = json!({
                "key": key,
                "name": name.clone(),
                "profile": ch.get("profile").cloned().unwrap_or(json!("")),
                "scenario": ch.get("scenario").cloned().unwrap_or(json!("")),
                "first_message": ch.get("first_message").cloned().unwrap_or(json!("")),
                "post_history": ch.get("post_history").cloned().unwrap_or(json!("")),
                "category": or_value(ch.get("category"), json!("")),
                "tags": or_value(ch.get("tags"), json!([])),
            });
            built.push((name, entry));
        }
        // the sort key `c["name"].lower()` is computed for every entry, in order, after the build
        let mut chars: Vec<(String, Value)> = Vec::with_capacity(built.len());
        for (name, entry) in built {
            chars.push((str_for_lower(&name)?.to_lowercase(), entry));
        }
        // python list.sort is stable, and sort_by is too
        chars.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Value::Array(chars.into_iter().map(|(_, v)| v).collect()))
    }

    fn form_entry(body: &Option<Value>, name: &str, profile: &str) -> Value {
        let category = py_strip(&body_str(body, "category", "")).to_string();
        json!({
            "name": name,
            "profile": profile,
            "scenario": body_str(body, "scenario", ""),
            "first_message": body_str(body, "first_message", ""),
            "post_history": body_str(body, "post_history", ""),
            "category": if category.is_empty() { Value::Null } else { json!(category) },
            "tags": clean_tags(body_get(body, "tags").as_ref()),
        })
    }

    fn route_add(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let name = py_strip(&body_str(body, "name", "")).to_string();
        let profile = py_strip(&body_str(body, "profile", "")).to_string();
        if name.is_empty() {
            return Ok(self.base.result("character name cannot be empty", false));
        }
        if profile.is_empty() {
            return Ok(self.base.result("character profile must not be blank", false));
        }
        if self.find_character(&name).is_some() {
            return Ok(self.base.result("a character with that name already exists", false));
        }
        let entry = Self::form_entry(body, &name, &profile);
        self.with_chars(|c| {
            c.data.insert(name.clone(), entry);
            c.save()
        })??;
        Ok(self.base.result(format!("character {} added", name), true))
    }

    fn route_edit(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let Some(key) = self.find_char_name(&body_str(body, "key", "")).filter(|k| !k.is_empty()) else {
            return Ok(self.base.result("character not found", false));
        };
        let new_name = py_strip(&body_str(body, "name", "")).to_string();
        let profile = py_strip(&body_str(body, "profile", "")).to_string();
        if new_name.is_empty() {
            return Ok(self.base.result("character name cannot be empty", false));
        }
        if profile.is_empty() {
            return Ok(self.base.result("character profile must not be blank", false));
        }
        if let Some(clash) = self.find_char_name(&new_name).filter(|c| !c.is_empty()) {
            if clash.to_lowercase() != key.to_lowercase() {
                return Ok(self.base.result("a character with that name already exists", false));
            }
        }
        let entry = Self::form_entry(body, &new_name, &profile);
        let renamed = new_name != key;
        self.with_chars(|c| {
            c.data.insert(key.clone(), entry);
            if renamed {
                if let Some(v) = c.data.shift_remove(&key) {
                    c.data.insert(new_name.clone(), v);
                }
            }
        })?;
        if renamed {
            // chats reference characters by their stored key, so re-stamp any chat that
            // used the old one (best effort: in memory, persisted on the next index save)
            if let Some(ch) = self.base.channel() {
                ch.chat.with_data_mut(|chats| restamp_chat_characters(chats, &key, &new_name))?;
                // the current chat's metadata is read through the same index entry
            }
        }
        self.save_chars()?;
        Ok(self.base.result("character edited", true))
    }

    /// python `(body or {}).get("name", "")` handed to `_find_character`, which calls `name.lower()` once per
    /// stored character: a non-str name only fails when there is a character to compare it with
    fn lookup_name(&self, raw: &Value) -> anyhow::Result<String> {
        if !self.all().is_empty() {
            str_for_lower(raw)?;
        }
        Ok(py_str(raw))
    }

    /// python `_route_delete`: a missing or falsy character is "character not found"
    fn route_delete(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let raw = body_get(body, "name").unwrap_or_else(|| json!(""));
        let name = self.lookup_name(&raw)?;
        if self.find_character(&name).is_none() {
            return Ok(self.base.result("character not found", false));
        }
        let Some(key) = self.find_char_name(&name) else {
            return Ok(self.base.result("character not found", false));
        };
        self.with_chars(|c| {
            c.data.shift_remove(&key);
            c.save()
        })??;
        Ok(self.base.result(format!("deleted character {}", key), true))
    }

    /// python `_route_import`: a dict or list is dumped back to text, any other non-str value reaches
    /// `json.loads` and raises its TypeError
    async fn route_import(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let raw = body_get(body, "json_code").unwrap_or_else(|| json!(""));
        match raw {
            Value::Object(_) | Value::Array(_) => self.import_json(&ol_core::pyjson::dumps(&raw)).await,
            Value::String(code) => self.import_json(&code).await,
            other => {
                // python: import_json's `except Exception as e` -> `core.detail_error(e)`
                let e = PyTypeError(format!("the JSON object must be str, bytes or bytearray, not {}", py_type(&other)));
                Ok(self.base.result(format!("error: {}", ol_core::detail_error(&e)), false))
            }
        }
    }

    /// python `_route_import_file`: `(body or {})` reads, then json or png by the filename
    async fn route_import_file(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let filename = py_strip(&body_str(body, "filename", "")).to_string();
        let mut data_b64 = body_str(body, "data", "");
        if filename.is_empty() || data_b64.is_empty() {
            return Ok(self.base.result("error: no file data received", false));
        }
        // strip a data-URL prefix if the browser sent one
        if data_b64.contains(',') && py_strip(&data_b64).starts_with("data:") {
            data_b64 = data_b64.split_once(',').map(|(_, b)| b.to_string()).unwrap_or_default();
        }
        let lower = filename.to_lowercase();
        if lower.ends_with(".json") {
            let decoded = b64decode_text(&data_b64).and_then(|b| py_utf8_decode(&b));
            return match decoded {
                Ok(code) => self.import_json(&code).await,
                Err(e) => Ok(self.base.result(format!("error: could not decode file: {}", ol_core::detail_error(&e)), false)),
            };
        }
        if lower.ends_with(".png") {
            let raw = match b64decode_text(&data_b64) {
                Ok(r) => r,
                Err(e) => return Ok(self.base.result(format!("error: could not decode file: {}", ol_core::detail_error(&e)), false)),
            };
            // python `if not card:` is false for an empty dict too
            let Some(card) = extract_card_from_png(&raw).filter(|c| !c.is_empty()) else {
                return Ok(self.base.result("error: no character card found inside this png", false));
            };
            let name = py_strip(&filename.chars().take(filename.chars().count() - 4).collect::<String>()).to_string();
            // python `_normalize_character` errors (a non-str profile joined with extras) propagate as an exception
            let normalized = normalize_card(&name, &Value::Object(card)).map_err(anyhow::Error::msg)?
                .filter(|n| n.get("profile").map(truthy).unwrap_or(false));
            let Some(mut normalized) = normalized else {
                return Ok(self.base.result("error: failed to extract a usable name and profile from this png", false));
            };
            if !normalized.get("name").map(truthy).unwrap_or(false) {
                normalized["name"] = json!(name);
            }
            let key = self.card_storage_key(&normalized["name"])?;
            self.with_chars(|c| {
                c.data.insert(key, normalized);
                c.save()
            })??;
            return Ok(self.base.result("character successfully imported", true));
        }
        Ok(self.base.result("error: unsupported file type, use .json or .png", false))
    }

    /// python `_chat_character`: `(chat.get("metadata") or {}).get("character") or ""`. the value is kept as
    /// stored; a truthy metadata that is not a dict raises the `.get` AttributeError
    fn chat_character(chat: &Value) -> anyhow::Result<Value> {
        let meta = chat.get("metadata").cloned().unwrap_or(Value::Null);
        let meta = if truthy(&meta) { meta } else { json!({}) };
        let Some(obj) = meta.as_object() else {
            anyhow::bail!("'{}' object has no attribute 'get'", py_type(&meta));
        };
        let c = obj.get("character").cloned().unwrap_or(Value::Null);
        Ok(if truthy(&c) { c } else { json!("") })
    }

    /// python `_route_names`: `e["name"].lower()` is the sort key for every entry, so a truthy non-str
    /// display name raises for the whole list
    fn route_names(&self) -> anyhow::Result<Value> {
        let mut rows: Vec<(String, Value)> = Vec::new();
        for (key, ch) in self.all() {
            // python `char.get(...)` on a stored value that is not a dict raises
            if !ch.is_object() {
                anyhow::bail!("'{}' object has no attribute 'get'", py_type(&ch));
            }
            let name = or_value(ch.get("name"), json!(&key));
            let sort_key = str_for_lower(&name)?.to_lowercase();
            rows.push((sort_key, json!({"key": key, "name": name})));
        }
        // stable sort, like python's sorted()
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Value::Array(rows.into_iter().map(|(_, v)| v).collect()))
    }

    fn route_chats_count(&self) -> anyhow::Result<Value> {
        let ch = self.channel()?;
        let characters = ch.chat.get_all().iter().map(Self::chat_character).collect::<anyhow::Result<Vec<_>>>()?;
        chats_count_from(characters)
    }

    /// python `self._find_char_name(name) or char.get("name")`: the canonical stored key, or the card's name
    /// when the key is missing or empty
    fn canonical_char_name(&self, name: &str, ch: &Value) -> anyhow::Result<Value> {
        canonical_name(self.find_char_name(name), ch)
    }

    fn route_apply(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let name = py_strip(&body_str(body, "name", "")).to_string();
        if name.is_empty() {
            self.set_current_character(json!(""))?;
            return Ok(self.base.result("character cleared", true));
        }
        let Some(ch) = self.find_character(&name) else {
            return Ok(self.base.result("character not found", false));
        };
        let char_name = self.canonical_char_name(&name, &ch)?;
        self.set_current_character(char_name.clone())?;
        Ok(self.base.result(char_name, true))
    }

    fn route_chats(&self, query: &Map<String, Value>) -> anyhow::Result<Value> {
        let character = query.get("character").map(py_str).unwrap_or_default();
        let ch = self.channel()?;
        let mut items: Vec<Value> = Vec::new();
        for c in ch.chat.get_all() {
            // python `==`: only a stored str equals the query string
            if !matches!(Self::chat_character(&c)?, Value::String(s) if s == character) {
                continue;
            }
            items.push(json!({
                "id": c.get("id").cloned().unwrap_or(Value::Null),
                "title": or_value(c.get("title"), json!("New chat")),
                "updated": or_value(c.get("updated"), json!("")),
            }));
        }
        Ok(Value::Array(items))
    }

    async fn route_create_chat(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let name = py_strip(&body_str(body, "name", "")).to_string();
        let Some(ch) = self.find_character(&name) else {
            return Ok(self.base.result("character not found", false));
        };
        let key = self.canonical_char_name(&name, &ch)?;
        // python `self.config.get("category") or "general"` is passed to chat.new() unchanged
        let category = self.base.config.get1("category").filter(truthy).unwrap_or_else(|| json!("general"));
        let channel = self.channel()?;
        // ol-core's new_chat takes a &str, so a non-str category is stored by replacing the fresh chat's field
        let category_text = py_str(&category);
        let id = channel.chat.new_chat(&category_text, "New chat", Some(json!({"character": key}))).await;
        if !category.is_string() {
            channel.chat.with_data_mut(|d| {
                if let Some(chat) = d.iter_mut().find(|c| c["id"].as_str() == Some(id.as_str())) {
                    chat["category"] = category;
                }
            });
            channel.chat.save_index();
        }
        Ok(json!({"id": id}))
    }
}

#[async_trait]
impl Module for Characters {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    fn disabled_tools(&self) -> Vec<String> {
        self.disabled.lock().clone()
    }

    fn header(&self) -> Option<String> {
        meta::HEADER.map(|h| h.to_string())
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "get_all" => Ok(self.base.result(self.list_characters(), true)),
            "switch" => self.switch(&args.str_or("name", "")).await,
            "switch_to_default" => {
                self.set_current_character(json!(""))?;
                Ok(json!("success"))
            }
            "add" => self.add(&args).await,
            "edit" => self.edit(&args).await,
            "read" => match self.find_character(&args.str_or("name", "")) {
                Some(c) => Ok(self.base.result(c, true)),
                None => Ok(json!("character does not exist!")),
            },
            "delete" => self.delete(&args.str_or("name", "")).await,
            "set_user_persona" => {
                self.set_user_persona(args.0.get("name").cloned().unwrap_or(Value::Null), args.0.get("profile").cloned().unwrap_or(Value::Null))
                    .await
            }
            "import_json" => self.import_json(&args.str_or("json_code", "")).await,
            _ => anyhow::bail!("'Characters' object has no attribute '{}'", method),
        }
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        *self.characters.lock() = Some(StorageDict::new("characters", StorageType::JsonFolder, None)?);
        *self.user_profile.lock() = Some(StorageDict::new("character_user", StorageType::Json, None)?);
        self.migrate_if_needed()?;
        self.ingest_pngs()?;
        self.normalize_all()?;
        if self.base.config.get_bool("insert_system_prompt") {
            // disable character listing tool
            self.disabled.lock().push("get_all".into());
        }
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        let curr = self.current_character()?;
        let use_tools = ol_core::config::get(&["model", "use_tools"]).map(|v| truthy(&v)).unwrap_or(false);
        let tool_text = if use_tools && self.base.config.get_bool("insert_system_prompt") && !truthy(&curr) {
            format!("Characters available to switch yourself to:\n{}", self.list_characters())
        } else {
            String::new()
        };
        let none_or = |t: String| if t.is_empty() { None } else { Some(t) };
        if !truthy(&curr) {
            return Ok(none_or(tool_text));
        }
        let curr_name = py_str(&curr);
        let Some(ch) = self.find_character(&curr_name) else {
            // deleted character / stale metadata: clean it up
            self.set_current_character(json!(""))?;
            return Ok(none_or(tool_text));
        };
        let char_name = ch.get("name").map(py_str).unwrap_or(curr_name);
        let profile = ch.get("profile").cloned().unwrap_or(json!(""));
        if !truthy(&profile) {
            return Ok(Some("Failed to extract character profile from character data".into()));
        }
        let scenario = ch.get("scenario").cloned().unwrap_or(json!(""));
        // python: tags first, then escaped newlines
        let profile = self.replace(&char_name, &py_str(&profile))?.replace("\\n", "\n");
        let scenario = if truthy(&scenario) { self.replace(&char_name, &py_str(&scenario))?.replace("\\n", "\n") } else { String::new() };

        let mut build = vec![format!("## You: {}\n{}", char_name, profile)];
        if !scenario.is_empty() {
            build.push(format!("## Scenario\n{}", scenario));
        }
        if let Some(up) = self.user_get("profile").filter(truthy) {
            let user_name = py_str_opt(self.user_get("name").as_ref());
            build.push(format!("## The user: {}", user_name));
            build.push(py_str(&up));
        }
        let text = build.join("\n\n");

        // empty chat: insert the first message into history by pushing it
        let first = ch.get("first_message").cloned().unwrap_or(json!(""));
        if self.base.config.get_bool("use_first_messages") && truthy(&first) {
            let channel = self.channel()?;
            if channel.chat.messages_len() == 0 {
                let first = self.replace(&char_name, &py_str(&first))?;
                channel.push(json!({"role": "assistant", "content": first}), true).await;
            }
        }
        Ok(Some(text))
    }

    async fn on_end_prompt(&self) -> anyhow::Result<Option<String>> {
        if !self.base.config.get_bool("use_post_history_instructions") {
            return Ok(None);
        }
        let curr = self.current_character()?;
        if !truthy(&curr) {
            return Ok(None);
        }
        let Some(ch) = self.find_character(&py_str(&curr)) else { return Ok(None) };
        Ok(ch.get("post_history").filter(|v| truthy(v)).map(py_str))
    }

    fn commands(&self) -> Vec<CommandSpec> {
        meta::commands().into_iter().map(|(c, _)| c).collect()
    }

    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        match name {
            "characters" => Ok(Some(self.list_characters())),
            "character" => Ok(Some(self.cmd_switch(&args).await?)),
            "username" => {
                let n = args.join(" ");
                self.with_user(|u| {
                    u.set("name", json!(n));
                    u.save()
                })??;
                Ok(Some("Your name has been set!".into()))
            }
            _ => anyhow::bail!("unknown command {}", name),
        }
    }

    fn ui_routes(&self) -> Vec<UiRoute> {
        meta::ui_routes().into_iter().map(|(r, _)| r).collect()
    }

    async fn ui_route(&self, path: &str, method: &str, body: Option<Value>, query: Map<String, Value>) -> anyhow::Result<Value> {
        match (path, method) {
            ("list", "GET") => self.route_list(),
            ("current", "GET") => self.route_current(),
            ("get", "GET") => {
                let name = query.get("name").map(py_str).unwrap_or_default();
                Ok(match self.find_character(&name) {
                    Some(c) => self.base.result(c, true),
                    None => self.base.result("character not found", false),
                })
            }
            ("switch", "POST") => {
                // python: `(body or {}).get("name", "")`, then `if not name`
                dict_body(&body)?;
                match body_get(&body, "name").filter(truthy) {
                    None => Ok(self.base.result("error: no character name given", false)),
                    Some(Value::String(name)) => self.switch(&name).await,
                    // python switch() -> _find_character() calls `name.lower()` once per stored character,
                    // so a non-string name only fails when there is a character to compare it with
                    Some(other) => {
                        if self.all().is_empty() {
                            Ok(self.base.result("character not found", false))
                        } else {
                            anyhow::bail!("'{}' object has no attribute 'lower'", py_type(&other))
                        }
                    }
                }
            }
            ("switch_default", "POST") => {
                self.set_current_character(json!(""))?;
                Ok(self.base.result("switched to default character", true))
            }
            ("delete", "POST") => {
                dict_body(&body)?;
                self.route_delete(&body)
            }
            ("import", "POST") => {
                dict_body(&body)?;
                self.route_import(&body).await
            }
            ("remove", "POST") => {
                dict_body(&body)?;
                self.delete(&body_str(&body, "name", "")).await
            }
            ("all", "GET") => self.route_all(),
            ("add", "POST") => {
                dict_body(&body)?;
                self.route_add(&body)
            }
            ("edit", "POST") => {
                dict_body(&body)?;
                self.route_edit(&body)
            }
            ("import_file", "POST") => {
                dict_body(&body)?;
                self.route_import_file(&body).await
            }
            ("names", "GET") => self.route_names(),
            ("chats_count", "GET") => self.route_chats_count(),
            ("apply", "POST") => {
                dict_body(&body)?;
                self.route_apply(&body)
            }
            ("chats", "GET") => self.route_chats(&query),
            ("create_chat", "POST") => {
                dict_body(&body)?;
                self.route_create_chat(&body).await
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

    fn png_with_text(chunk_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut push = |t: &[u8], d: &[u8]| {
            out.extend_from_slice(&(d.len() as u32).to_be_bytes());
            out.extend_from_slice(t);
            out.extend_from_slice(d);
            out.extend_from_slice(&[0, 0, 0, 0]);
        };
        push(b"IHDR", &[0; 13]);
        push(chunk_type, payload);
        push(b"IEND", &[]);
        out
    }

    #[test]
    fn png_text_chunk() {
        use base64::Engine;
        let card = json!({"spec": "chara_card_v2", "data": {"name": "Ann", "description": "A pirate", "first_mes": "Ahoy {{user}}"}});
        let b64 = base64::engine::general_purpose::STANDARD.encode(card.to_string());
        let mut payload = b"chara\0".to_vec();
        payload.extend_from_slice(b64.as_bytes());
        let png = png_with_text(b"tEXt", &payload);
        let got = extract_card_from_png(&png).unwrap();
        assert_eq!(Value::Object(got), card);
        // non-card keyword is ignored
        let png = png_with_text(b"tEXt", b"Comment\0aGVsbG8=");
        assert!(extract_card_from_png(&png).is_none());
        assert!(extract_card_from_png(b"not a png").is_none());
    }

    #[test]
    fn png_itxt_compressed() {
        use base64::Engine;
        use std::io::Write;
        let card = json!({"name": "Bo", "description": "d"});
        let b64 = base64::engine::general_purpose::STANDARD.encode(card.to_string());
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b64.as_bytes()).unwrap();
        let compressed = enc.finish().unwrap();
        let mut payload = b"ccv2\0".to_vec();
        payload.extend_from_slice(&[1, 0]); // compression flag + method
        payload.extend_from_slice(b"en\0"); // language tag
        payload.extend_from_slice(b"\0"); // translated keyword
        // python skips 3 bytes after the keyword (flag, method, first lang byte) then two NULs
        let mut fixed = b"ccv2\0".to_vec();
        fixed.extend_from_slice(&[1, 0, b'e']);
        fixed.extend_from_slice(b"n\0\0");
        fixed.extend_from_slice(&compressed);
        let png = png_with_text(b"iTXt", &fixed);
        assert_eq!(Value::Object(extract_card_from_png(&png).unwrap()), card);
        let _ = payload;
    }

    #[test]
    fn normalize_formats() {
        // internal format
        let n = normalize_character("Key", &json!({"profile": "p", "name": ""})).unwrap();
        assert_eq!(n, json!({"name": "Key", "profile": "p", "scenario": "", "first_message": "", "post_history": "", "category": null, "tags": []}));
        // V2 card
        let v2 = json!({"spec": "chara_card_v2", "category": "fun", "data": {"name": "Ann", "description": "desc", "personality": "bold", "mes_example": "<START>hi", "first_mes": "Ahoy", "scenario": "ship", "post_history_instructions": "stay in character", "tags": ["pirate"]}});
        let n = normalize_character("", &v2).unwrap();
        assert_eq!(
            n,
            json!({"name": "Ann", "profile": "desc\n\nPersonality: bold\n\nExample conversation:\n<START>hi", "scenario": "ship", "first_message": "Ahoy", "post_history": "stay in character", "category": "fun", "tags": ["pirate"]})
        );
        // legacy identity format
        let n = normalize_character("Old", &json!({"identity": "legacy", "first_message": "yo"})).unwrap();
        assert_eq!(n["name"], "Old");
        assert_eq!(n["profile"], "legacy");
        assert_eq!(n["first_message"], "yo");
        // unusable
        assert!(normalize_character("x", &json!({"name": "n"})).is_none());
        assert!(normalize_character("x", &json!("str")).is_none());
    }

    #[test]
    fn tag_replacement() {
        assert_eq!(replace_tags("Ann", &json!("Bob"), "Hi {{USER}}, I am {{Char}} aka <bot>.").unwrap(), "Hi Bob, I am Ann aka Ann.");
        assert_eq!(case_insensitive_replace("aXbxc", "x", "-"), "a-b-c");
        assert_eq!(case_insensitive_replace("abc", "", "-"), "abc");
    }

    #[test]
    fn non_str_user_name_only_fails_once_a_user_tag_matches() {
        // python: "".join raises at the first inserted replacement (sequence item 1)
        assert_eq!(replace_tags("Ann", &json!(5), "Hi there").unwrap(), "Hi there");
        assert_eq!(replace_tags("Ann", &json!(5), "Hi {{user}}").unwrap_err().to_string(), "sequence item 1: expected str instance, int found");
        assert_eq!(replace_tags("Ann", &Value::Null, "<USER>").unwrap_err().to_string(), "sequence item 1: expected str instance, NoneType found");
        // the char tag is always a str in this path and still replaces
        assert_eq!(replace_tags("Ann", &Value::Null, "{{char}}").unwrap(), "Ann");
    }

    #[test]
    fn metadata_item_assignment_errors_match_python() {
        assert_eq!(item_assignment_error(&Value::Null), "'NoneType' object does not support item assignment");
        assert_eq!(item_assignment_error(&json!("x")), "'str' object does not support item assignment");
        assert_eq!(item_assignment_error(&json!([1])), "list indices must be integers or slices, not str");
    }

    #[test]
    fn tags_cleanup() {
        assert_eq!(clean_tags(Some(&json!("a, b,, c "))), vec![json!("a"), json!("b"), json!("c")]);
        assert_eq!(clean_tags(Some(&json!([" x ", 1, ""]))), vec![json!("x"), json!("1")]);
        assert!(clean_tags(None).is_empty());
    }

    #[test]
    fn category_keys_follow_python_dict_equality() {
        assert_eq!(category_key(&json!(1)), category_key(&json!(1.0)));
        assert_eq!(category_key(&json!(1)), category_key(&json!(true)));
        assert_ne!(category_key(&json!(1)), category_key(&json!("1")));
        assert_ne!(category_key(&json!("general")), category_key(&json!(2)));
    }

    #[test]
    fn lower_needs_a_str_receiver_like_python() {
        assert_eq!(str_for_lower(&json!("Ann")).unwrap(), "Ann");
        assert_eq!(str_for_lower(&json!(5)).unwrap_err().to_string(), "'int' object has no attribute 'lower'");
        assert_eq!(str_for_lower(&json!(["x"])).unwrap_err().to_string(), "'list' object has no attribute 'lower'");
    }

    #[test]
    fn empty_stored_key_falls_back_to_the_card_name_like_python_or() {
        assert_eq!(canonical_name(Some(String::new()), &json!({"name": "Ann"})).unwrap(), json!("Ann"));
        assert_eq!(canonical_name(Some("ann".into()), &json!({"name": "Ann"})).unwrap(), json!("ann"));
        assert_eq!(canonical_name(None, &json!({"name": 5})).unwrap(), json!(5));
        assert_eq!(canonical_name(Some(String::new()), &json!({})).unwrap(), Value::Null);
        // python `char.get("name")` on a truthy non-dict card
        assert_eq!(canonical_name(Some(String::new()), &json!("card")).unwrap_err().to_string(), "'str' object has no attribute 'get'");
        assert_eq!(canonical_name(Some(String::new()), &json!(["x"])).unwrap_err().to_string(), "'list' object has no attribute 'get'");
        // a non-empty stored key never reaches `.get`
        assert_eq!(canonical_name(Some("Ann".into()), &json!("card")).unwrap(), json!("Ann"));
    }

    #[test]
    fn current_route_follows_the_python_get_chain() {
        assert_eq!(current_route_value(Some(json!({"character": "Ann"}))).unwrap(), json!("Ann"));
        assert_eq!(current_route_value(Some(json!({"character": 0}))).unwrap(), Value::Null);
        assert_eq!(current_route_value(Some(json!({}))).unwrap(), Value::Null);
        assert_eq!(current_route_value(None).unwrap_err().to_string(), "'NoneType' object has no attribute 'get'");
        assert_eq!(current_route_value(Some(json!("x"))).unwrap_err().to_string(), "'str' object has no attribute 'get'");
    }

    #[test]
    fn import_json_errors_are_python_json_decode_errors() {
        // python json.loads texts, shown in the import modal as `error: <text>`
        assert_eq!(py_json_loads("").unwrap_err(), "Expecting value: line 1 column 1 (char 0)");
        assert_eq!(py_json_loads("{bad").unwrap_err(), "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)");
        assert_eq!(py_json_loads("{} x").unwrap_err(), "Extra data: line 1 column 4 (char 3)");
        assert_eq!(py_json_loads("\u{feff}{}").unwrap_err(), "Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)");
    }

    #[test]
    fn python_json_accepts_nan_and_infinity() {
        // python reads these as floats: a top-level NaN is a non-dict card, and a nested one is kept
        assert_eq!(py_json_loads("NaN").unwrap(), json!(true));
        assert_eq!(py_json_loads("{\"a\": -Infinity, \"b\": 1e400}").unwrap(), json!({"a": true, "b": true}));
    }

    #[test]
    fn non_str_profile_with_extras_is_a_python_type_error() {
        // python `profile + "\n\n"` on an int profile raises; a str profile folds the extras in
        let card = |profile: Value| json!({"data": {"name": "Ann", "description": profile, "personality": "kind"}});
        assert_eq!(
            normalize_card("", &card(json!(5))).unwrap_err(),
            "unsupported operand type(s) for +: 'int' and 'str'"
        );
        assert_eq!(normalize_card("", &card(json!([1]))).unwrap_err(), "can only concatenate list (not \"str\") to list");
        let ok = normalize_card("", &card(json!("A pirate"))).unwrap().unwrap();
        assert_eq!(ok["profile"], json!("A pirate\n\nPersonality: kind"));
        // without personality or example text the non-str profile is stored as it is
        let plain = normalize_card("", &json!({"data": {"name": "Ann", "description": 5}})).unwrap().unwrap();
        assert_eq!(plain["profile"], json!(5));
    }

    #[test]
    fn import_file_decoding_matches_python_errors() {
        use crate::util::{b64decode_text, py_utf8_decode};
        // python base64.b64decode: missing padding, the real data-character count, and non-ASCII str input
        assert_eq!(b64decode_text("QQ").unwrap_err(), "Incorrect padding");
        assert_eq!(b64decode_text("QQ==").unwrap(), b"A");
        assert_eq!(b64decode_text("QUI=").unwrap(), b"AB");
        assert_eq!(
            b64decode_text("QUFEF").unwrap_err(),
            "Invalid base64-encoded string: number of data characters (5) cannot be 1 more than a multiple of 4"
        );
        assert_eq!(
            b64decode_text("QUFEFGHIJ").unwrap_err(),
            "Invalid base64-encoded string: number of data characters (9) cannot be 1 more than a multiple of 4"
        );
        assert_eq!(b64decode_text("caf\u{e9}").unwrap_err(), "string argument should contain only ASCII characters");
        // python bytes.decode("utf-8") messages
        assert_eq!(py_utf8_decode(b"\xff").unwrap_err(), "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte");
        assert_eq!(py_utf8_decode(b"\xe4A").unwrap_err(), "'utf-8' codec can't decode byte 0xe4 in position 0: invalid continuation byte");
        assert_eq!(py_utf8_decode(b"A\xc3").unwrap_err(), "'utf-8' codec can't decode byte 0xc3 in position 1: unexpected end of data");
        assert_eq!(py_utf8_decode(b"\xe4\xb8A").unwrap_err(), "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte");
    }

    #[test]
    fn png_card_is_read_with_python_json_and_empty_card_is_falsy() {
        use base64::Engine;
        let png = |json_text: &str| {
            let b64 = base64::engine::general_purpose::STANDARD.encode(json_text);
            let mut payload = b"chara\0".to_vec();
            payload.extend_from_slice(b64.as_bytes());
            png_with_text(b"tEXt", &payload)
        };
        // python json.loads reads NaN, so this card is found and keeps its name
        let card = extract_card_from_png(&png(r#"{"data": {"name": "Ann", "description": NaN}}"#)).unwrap();
        assert_eq!(card["data"]["name"], json!("Ann"));
        // an empty object is a card for extraction; the route treats it as "no card" (python `if not card`)
        let empty = extract_card_from_png(&png("{}")).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn chat_character_follows_python_get_chain() {
        // a missing or falsy metadata reads as {} and a falsy character reads as ""
        assert_eq!(Characters::chat_character(&json!({})).unwrap(), json!(""));
        assert_eq!(Characters::chat_character(&json!({"metadata": {"character": ""}})).unwrap(), json!(""));
        assert_eq!(Characters::chat_character(&json!({"metadata": {"character": "Ann"}})).unwrap(), json!("Ann"));
        // a non-str character is kept as-is, so python's `==` against a str query never matches it
        assert_eq!(Characters::chat_character(&json!({"metadata": {"character": 5}})).unwrap(), json!(5));
        // truthy non-dict metadata raises the python AttributeError
        assert_eq!(
            Characters::chat_character(&json!({"metadata": "x"})).unwrap_err().to_string(),
            "'str' object has no attribute 'get'"
        );
    }

    #[test]
    fn non_object_route_bodies_raise_like_python() {
        // python `(body or {}).get(...)`: falsy bodies read as an empty dict, truthy non-dicts raise
        assert!(dict_body(&Some(json!([]))).is_ok());
        assert!(dict_body(&None).is_ok());
        assert_eq!(dict_body(&Some(json!(["x"]))).unwrap_err().to_string(), "'list' object has no attribute 'get'");
    }

    #[test]
    fn chats_count_merges_python_equal_keys() {
        // python: counts[True], counts[1], counts[1.0] are one dict entry keyed by the first stored value,
        // which json.dumps renders as "true"; a str "a" is its own key; falsy characters are skipped
        let counts = chats_count_from(vec![json!(true), json!(1), json!(1.0), json!("a"), json!(""), json!(null)]).unwrap();
        assert_eq!(counts, json!({"true": 3, "a": 1}));
    }

    #[test]
    fn chats_count_keeps_the_first_stored_spelling() {
        // python keeps the first key object: 1.0 first renders as "1.0", and a later 1 joins that entry
        let counts = chats_count_from(vec![json!(1.0), json!(1), json!("Ann"), json!("Ann")]).unwrap();
        assert_eq!(counts, json!({"1.0": 2, "Ann": 2}));
        // an unhashable character raises like python's dict key TypeError
        assert_eq!(chats_count_from(vec![json!(["x"])]).unwrap_err().to_string(), "unhashable type: 'list'");
    }

    #[test]
    fn switch_first_message_needs_a_dict_card() {
        // python switch(): `char.get("first_message", "")` raises for a truthy non-dict stored character
        assert_eq!(card_first_message(&json!({"first_message": "hi"})).unwrap(), json!("hi"));
        assert_eq!(card_first_message(&json!({})).unwrap(), json!(""));
        assert_eq!(card_first_message(&json!("card")).unwrap_err().to_string(), "'str' object has no attribute 'get'");
        assert_eq!(card_first_message(&json!(["card"])).unwrap_err().to_string(), "'list' object has no attribute 'get'");
    }

    #[test]
    fn rename_restamps_chats_and_raises_on_truthy_non_dict_metadata() {
        let mut chats = vec![
            json!({"metadata": {"character": "Ann"}}),
            json!({"metadata": null}),
            json!({"metadata": {"character": "Bob"}}),
        ];
        restamp_chat_characters(&mut chats, "Ann", "Anna").unwrap();
        assert_eq!(chats[0]["metadata"]["character"], "Anna");
        assert_eq!(chats[2]["metadata"]["character"], "Bob");
        // a falsy metadata is an empty dict in python, so it is skipped
        let mut falsy = vec![json!({"metadata": ""}), json!({})];
        restamp_chat_characters(&mut falsy, "Ann", "Anna").unwrap();
        let mut bad = vec![json!({"metadata": "Ann"})];
        assert_eq!(restamp_chat_characters(&mut bad, "Ann", "Anna").unwrap_err().to_string(), "'str' object has no attribute 'get'");
        let mut bad_list = vec![json!({"metadata": ["Ann"]})];
        assert_eq!(restamp_chat_characters(&mut bad_list, "Ann", "Anna").unwrap_err().to_string(), "'list' object has no attribute 'get'");
    }
}
