//! Port of modules/characters/module.py: AI character profiles (incl. sillytavern-style
//! character card import from JSON and PNG tEXt/iTXt metadata).

#[path = "../../../generated/modules/characters.rs"]
pub mod meta;

use crate::util::{b64decode, body_get, body_str, eq_ci_strip};
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
    let char_obj = ch.as_object()?;
    let empty = json!("");
    let empty_list = json!([]);
    if char_obj.contains_key("profile") {
        // already internal format; just make sure every field exists
        return Some(json!({
            "name": or_value(char_obj.get("name"), json!(stored_key)),
            "profile": get_or(char_obj, "profile", &empty),
            "scenario": get_or(char_obj, "scenario", &empty),
            "first_message": get_or(char_obj, "first_message", &empty),
            "post_history": get_or(char_obj, "post_history", &empty),
            "category": char_obj.get("category").cloned().unwrap_or(Value::Null),
            "tags": get_or(char_obj, "tags", &empty_list),
        }));
    }
    // character card V2 wraps everything in a data field, V1 is flat
    let card = char_obj.get("data").unwrap_or(ch).as_object()?;
    let name = or_value(card.get("name"), json!(stored_key));
    let mut profile = card.get("description").cloned().unwrap_or(json!(""));
    if !truthy(&profile) {
        // legacy openlumara format used 'identity' as its profile field
        profile = char_obj.get("identity").cloned().unwrap_or(json!(""));
    }
    if !truthy(&profile) {
        return None;
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
        profile = json!(format!("{}\n\n{}", py_str(&profile), extras.join("\n\n")));
    }
    let first_message = match card.get("first_mes") {
        Some(v) => v.clone(),
        None => char_obj.get("first_message").cloned().unwrap_or(json!("")),
    };
    let tags = match card.get("tags") {
        Some(v) => v.clone(),
        None => char_obj.get("tags").cloned().unwrap_or(json!([])),
    };
    Some(json!({
        "name": name,
        "profile": profile,
        "scenario": get_or(card, "scenario", &empty),
        "first_message": first_message,
        "post_history": get_or(card, "post_history_instructions", &empty),
        "category": char_obj.get("category").cloned().unwrap_or(Value::Null),
        "tags": tags,
    }))
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
            let card: Value = serde_json::from_str(&text).map_err(|_| ())?;
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

/// replaces the magic words defined in the character card spec
pub fn replace_tags(char_name: &str, user_name: &str, text: &str) -> String {
    let map = [("{{char}}", char_name), ("{char}", char_name), ("<BOT>", char_name), ("{{user}}", user_name), ("{user}", user_name), ("<USER>", user_name)];
    let mut out = text.to_string();
    for (word, rep) in map {
        out = case_insensitive_replace(&out, word, rep);
    }
    out
}

/// tags arrive from the form as a comma-separated string (or a list)
fn clean_tags(raw: Option<&Value>) -> Vec<Value> {
    match raw {
        Some(Value::String(s)) => s.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()).map(|t| json!(t)).collect(),
        Some(Value::Array(a)) => a.iter().map(|t| py_str(t).trim().to_string()).filter(|t| !t.is_empty()).map(Value::String).collect(),
        _ => vec![],
    }
}

/// `str(x).lower()` sort key
fn lower_key(v: &Value) -> String {
    py_str(v).to_lowercase()
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

    /// searches for a character, case insensitive
    fn find_character(&self, name: &str) -> Option<Value> {
        self.all().into_iter().find(|(k, _)| eq_ci_strip(k, name)).map(|(_, v)| v)
    }

    /// searches for a character and returns the stored key with correct case
    fn find_char_name(&self, name: &str) -> Option<String> {
        self.all().into_iter().find(|(k, _)| eq_ci_strip(k, name)).map(|(k, _)| k)
    }

    fn user_name(&self) -> String {
        self.with_user(|u| {
            u.get("name");
            u.data.get("name").map(py_str).unwrap_or_else(|| "user".to_string())
        })
        .unwrap_or_else(|_| "user".into())
    }

    fn replace(&self, char_name: &str, text: &str) -> String {
        replace_tags(char_name, &self.user_name(), text)
    }

    fn channel(&self) -> anyhow::Result<Arc<Channel>> {
        self.base.channel().ok_or_else(|| anyhow!("'NoneType' object has no attribute 'context'"))
    }

    /// `chat.get("metadata").get("character")`
    fn current_character(&self) -> anyhow::Result<Value> {
        let ch = self.channel()?;
        Ok(ch.chat.get("metadata").and_then(|m| m.get("character").cloned()).unwrap_or(Value::Null))
    }

    /// `chat.get("metadata")["character"] = value` (in-memory, persisted on the next chat save)
    fn set_current_character(&self, value: Value) -> anyhow::Result<()> {
        let ch = self.channel()?;
        let mut meta = ch.chat.get("metadata").filter(|m| m.is_object()).unwrap_or_else(|| json!({}));
        meta["character"] = value;
        ch.chat.set_current_field("metadata", meta);
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
            let first = self.replace(&char_name, &py_str(&first));
            return Ok(format!("character switched to {}\n\n{}", char_name, first));
        }
        Ok(format!("character switched to {}", char_name))
    }

    pub async fn switch(&self, name: &str) -> anyhow::Result<Value> {
        let Some(ch) = self.find_character(name) else {
            return Ok(self.base.result("character not found", false));
        };
        let char_name = match self.find_char_name(name) {
            Some(n) => json!(n),
            None => ch.get("name").cloned().unwrap_or(Value::Null),
        };
        self.set_current_character(char_name.clone())?;
        let char_name = py_str(&char_name);
        let first = ch.get("first_message").cloned().unwrap_or(json!(""));
        if truthy(&first) && self.base.config.get_bool("use_first_messages") {
            let first = self.replace(&char_name, &py_str(&first));
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

    pub async fn import_json(&self, json_code: &str) -> anyhow::Result<Value> {
        let obj: Value = match serde_json::from_str(json_code) {
            Ok(v) => v,
            Err(e) => return Ok(self.base.result(format!("error: {}", ol_core::detail_error(&e)), false)),
        };
        if !obj.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            return Ok(self.base.result("error: character card was empty", false));
        }
        let normalized = normalize_character("", &obj)
            .filter(|n| n.get("name").map(truthy).unwrap_or(false) && n.get("profile").map(truthy).unwrap_or(false));
        let Some(normalized) = normalized else {
            return Ok(self.base.result("error: failed to extract a usable name and profile from this character card", false));
        };
        let nname = py_str(&normalized["name"]);
        let key = self.find_char_name(&nname).unwrap_or(nname);
        self.with_chars(|c| {
            c.data.insert(key, normalized);
            c.save()
        })??;
        Ok(self.base.result("character successfully imported", true))
    }

    // ---------------- webui routes ----------------

    fn route_list(&self) -> Value {
        let mut grouped: IndexMap<String, Vec<Value>> = IndexMap::new();
        for (name, ch) in self.all() {
            let cat = ch.get("category").filter(|c| truthy(c)).map(py_str).unwrap_or_else(|| "general".into());
            grouped.entry(cat).or_default().push(json!({
                "name": or_value(ch.get("name"), json!(name)),
                "tags": or_value(ch.get("tags"), json!([])),
            }));
        }
        let mut cats: Vec<(String, Vec<Value>)> = grouped.into_iter().collect();
        cats.sort_by(|a, b| a.0.cmp(&b.0));
        let categories: Vec<Value> = cats
            .into_iter()
            .map(|(cat, mut chars)| {
                chars.sort_by_key(|c| lower_key(&c["name"]));
                json!({"name": cat, "characters": chars})
            })
            .collect();
        json!({"categories": categories})
    }

    fn route_all(&self) -> Value {
        let mut chars: Vec<Value> = self
            .all()
            .into_iter()
            .map(|(key, ch)| {
                json!({
                    "key": key,
                    "name": or_value(ch.get("name"), json!(key)),
                    "profile": ch.get("profile").cloned().unwrap_or(json!("")),
                    "scenario": ch.get("scenario").cloned().unwrap_or(json!("")),
                    "first_message": ch.get("first_message").cloned().unwrap_or(json!("")),
                    "post_history": ch.get("post_history").cloned().unwrap_or(json!("")),
                    "category": or_value(ch.get("category"), json!("")),
                    "tags": or_value(ch.get("tags"), json!([])),
                })
            })
            .collect();
        chars.sort_by_key(|c| lower_key(&c["name"]));
        Value::Array(chars)
    }

    fn form_entry(body: &Option<Value>, name: &str, profile: &str) -> Value {
        let category = body_str(body, "category", "").trim().to_string();
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
        let name = body_str(body, "name", "").trim().to_string();
        let profile = body_str(body, "profile", "").trim().to_string();
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
        let Some(key) = self.find_char_name(&body_str(body, "key", "")) else {
            return Ok(self.base.result("character not found", false));
        };
        let new_name = body_str(body, "name", "").trim().to_string();
        let profile = body_str(body, "profile", "").trim().to_string();
        if new_name.is_empty() {
            return Ok(self.base.result("character name cannot be empty", false));
        }
        if profile.is_empty() {
            return Ok(self.base.result("character profile must not be blank", false));
        }
        if let Some(clash) = self.find_char_name(&new_name) {
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
                ch.chat.with_data_mut(|chats| {
                    for chat in chats.iter_mut() {
                        if let Some(meta) = chat.get_mut("metadata").and_then(|m| m.as_object_mut()) {
                            if meta.get("character") == Some(&json!(key)) {
                                meta.insert("character".into(), json!(new_name));
                            }
                        }
                    }
                });
                // the current chat's metadata is read through the same index entry
            }
        }
        self.save_chars()?;
        Ok(self.base.result("character edited", true))
    }

    async fn route_import_file(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let filename = body_str(body, "filename", "").trim().to_string();
        let mut data_b64 = body_str(body, "data", "");
        if filename.is_empty() || data_b64.is_empty() {
            return Ok(self.base.result("error: no file data received", false));
        }
        // strip a data-URL prefix if the browser sent one
        if data_b64.contains(',') && data_b64.trim().starts_with("data:") {
            data_b64 = data_b64.split_once(',').map(|(_, b)| b.to_string()).unwrap_or_default();
        }
        let lower = filename.to_lowercase();
        if lower.ends_with(".json") {
            let decoded = b64decode(data_b64.as_bytes()).and_then(|b| String::from_utf8(b).map_err(|e| e.to_string()));
            return match decoded {
                Ok(code) => self.import_json(&code).await,
                Err(e) => Ok(self.base.result(format!("error: could not decode file: {}", e), false)),
            };
        }
        if lower.ends_with(".png") {
            let raw = match b64decode(data_b64.as_bytes()) {
                Ok(r) => r,
                Err(e) => return Ok(self.base.result(format!("error: could not decode file: {}", e), false)),
            };
            let Some(card) = extract_card_from_png(&raw) else {
                return Ok(self.base.result("error: no character card found inside this png", false));
            };
            let name: String = filename.chars().take(filename.chars().count() - 4).collect::<String>().trim().to_string();
            let normalized = normalize_character(&name, &Value::Object(card)).filter(|n| n.get("profile").map(truthy).unwrap_or(false));
            let Some(mut normalized) = normalized else {
                return Ok(self.base.result("error: failed to extract a usable name and profile from this png", false));
            };
            if !normalized.get("name").map(truthy).unwrap_or(false) {
                normalized["name"] = json!(name);
            }
            let nname = py_str(&normalized["name"]);
            let key = self.find_char_name(&nname).unwrap_or(nname);
            self.with_chars(|c| {
                c.data.insert(key, normalized);
                c.save()
            })??;
            return Ok(self.base.result("character successfully imported", true));
        }
        Ok(self.base.result("error: unsupported file type, use .json or .png", false))
    }

    fn chat_character(chat: &Value) -> String {
        chat.get("metadata").and_then(|m| m.get("character")).filter(|c| truthy(c)).map(py_str).unwrap_or_default()
    }

    fn route_names(&self) -> Value {
        let mut entries: Vec<Value> = self.all().into_iter().map(|(k, ch)| json!({"key": k, "name": or_value(ch.get("name"), json!(k))})).collect();
        entries.sort_by_key(|e| lower_key(&e["name"]));
        Value::Array(entries)
    }

    fn route_chats_count(&self) -> anyhow::Result<Value> {
        let ch = self.channel()?;
        let mut counts: IndexMap<String, i64> = IndexMap::new();
        for chat in ch.chat.get_all() {
            let c = Self::chat_character(&chat);
            if !c.is_empty() {
                *counts.entry(c).or_insert(0) += 1;
            }
        }
        Ok(json!(counts))
    }

    fn route_apply(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let name = body_str(body, "name", "").trim().to_string();
        if name.is_empty() {
            self.set_current_character(json!(""))?;
            return Ok(self.base.result("character cleared", true));
        }
        let Some(ch) = self.find_character(&name) else {
            return Ok(self.base.result("character not found", false));
        };
        let char_name = match self.find_char_name(&name) {
            Some(n) => json!(n),
            None => ch.get("name").cloned().unwrap_or(Value::Null),
        };
        self.set_current_character(char_name.clone())?;
        Ok(self.base.result(char_name, true))
    }

    fn route_chats(&self, query: &Map<String, Value>) -> anyhow::Result<Value> {
        let character = query.get("character").map(py_str).unwrap_or_default();
        let ch = self.channel()?;
        let items: Vec<Value> = ch
            .chat
            .get_all()
            .into_iter()
            .filter(|c| Self::chat_character(c) == character)
            .map(|c| {
                json!({
                    "id": c.get("id").cloned().unwrap_or(Value::Null),
                    "title": or_value(c.get("title"), json!("New chat")),
                    "updated": or_value(c.get("updated"), json!("")),
                })
            })
            .collect();
        Ok(Value::Array(items))
    }

    async fn route_create_chat(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let name = body_str(body, "name", "").trim().to_string();
        let Some(ch) = self.find_character(&name) else {
            return Ok(self.base.result("character not found", false));
        };
        let key = match self.find_char_name(&name) {
            Some(n) => json!(n),
            None => ch.get("name").cloned().unwrap_or(Value::Null),
        };
        let category = self.base.config.get1("category").filter(truthy).map(|v| py_str(&v)).unwrap_or_else(|| "general".into());
        let channel = self.channel()?;
        let id = channel.chat.new_chat(&category, "New chat", Some(json!({"character": key}))).await;
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
        let mut profile = self.replace(&char_name, &py_str(&profile)).replace("\\n", "\n");
        let scenario = if truthy(&scenario) { self.replace(&char_name, &py_str(&scenario)).replace("\\n", "\n") } else { String::new() };
        // keep python's ordering of operations: tags first, then escaped newlines
        profile = profile.to_string();

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
                let first = self.replace(&char_name, &py_str(&first));
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
            ("list", "GET") => Ok(self.route_list()),
            ("current", "GET") => {
                let c = self.current_character()?;
                Ok(if truthy(&c) { c } else { Value::Null })
            }
            ("get", "GET") => {
                let name = query.get("name").map(py_str).unwrap_or_default();
                Ok(match self.find_character(&name) {
                    Some(c) => self.base.result(c, true),
                    None => self.base.result("character not found", false),
                })
            }
            ("switch", "POST") => {
                let name = body_get(&body, "name").filter(truthy);
                match name {
                    Some(n) => self.switch(&py_str(&n)).await,
                    None => Ok(self.base.result("error: no character name given", false)),
                }
            }
            ("switch_default", "POST") => {
                self.set_current_character(json!(""))?;
                Ok(self.base.result("switched to default character", true))
            }
            ("delete", "POST") => {
                let name = body_str(&body, "name", "");
                let Some(key) = self.find_char_name(&name) else {
                    return Ok(self.base.result("character not found", false));
                };
                self.with_chars(|c| {
                    c.data.shift_remove(&key);
                    c.save()
                })??;
                Ok(self.base.result(format!("deleted character {}", key), true))
            }
            ("import", "POST") => {
                let code = match body_get(&body, "json_code") {
                    Some(v @ (Value::Object(_) | Value::Array(_))) => ol_core::pyjson::dumps(&v),
                    Some(v) => py_str(&v),
                    None => String::new(),
                };
                self.import_json(&code).await
            }
            ("remove", "POST") => self.delete(&body_str(&body, "name", "")).await,
            ("all", "GET") => Ok(self.route_all()),
            ("add", "POST") => self.route_add(&body),
            ("edit", "POST") => self.route_edit(&body),
            ("import_file", "POST") => self.route_import_file(&body).await,
            ("names", "GET") => Ok(self.route_names()),
            ("chats_count", "GET") => self.route_chats_count(),
            ("apply", "POST") => self.route_apply(&body),
            ("chats", "GET") => self.route_chats(&query),
            ("create_chat", "POST") => self.route_create_chat(&body).await,
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
        assert_eq!(replace_tags("Ann", "Bob", "Hi {{USER}}, I am {{Char}} aka <bot>."), "Hi Bob, I am Ann aka Ann.");
        assert_eq!(case_insensitive_replace("aXbxc", "x", "-"), "a-b-c");
        assert_eq!(case_insensitive_replace("abc", "", "-"), "abc");
    }

    #[test]
    fn tags_cleanup() {
        assert_eq!(clean_tags(Some(&json!("a, b,, c "))), vec![json!("a"), json!("b"), json!("c")]);
        assert_eq!(clean_tags(Some(&json!([" x ", 1, ""]))), vec![json!("x"), json!("1")]);
        assert!(clean_tags(None).is_empty());
    }
}
