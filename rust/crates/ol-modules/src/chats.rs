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

static JSON_NUMBER_RX: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?$").unwrap());

/// the `\uXXXX` escape starting at `i`, if there is one with four hex digits
fn unicode_escape(b: &[u8], i: usize) -> Option<u32> {
    if b.get(i) != Some(&b'\\') || b.get(i + 1) != Some(&b'u') {
        return None;
    }
    let hex = std::str::from_utf8(b.get(i + 2..i + 6)?).ok()?;
    if !hex.bytes().all(|d| d.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// python's json accepts NaN, Infinity and -Infinity outside strings; each is rewritten to `0.0`, a float that
/// python's flatten loop also treats as neither str, list nor dict
fn non_json_literal(b: &[u8], i: usize) -> Option<(usize, &'static str)> {
    let after_ok = |j: usize| b.get(j).is_none_or(|n| b" \t\r\n,]}".contains(n));
    for t in [&b"-Infinity"[..], b"Infinity", b"NaN"] {
        if b[i..].starts_with(t) && after_ok(i + t.len()) {
            return Some((t.len(), "0.0"));
        }
    }
    None
}

/// a number literal outside strings that serde_json rejects but python reads: a float that overflows f64 is
/// inf in python (rewritten to `0.0`), and an integer too big for i64/u64 is a big int in python (rewritten
/// to `0`). the replacement keeps the python type name, which the flatten-loop errors print.
fn out_of_range_number(b: &[u8], i: usize) -> Option<(usize, &'static str)> {
    if b[i] != b'-' && !b[i].is_ascii_digit() {
        return None;
    }
    let end = (i..b.len()).find(|&j| !(b[j].is_ascii_digit() || b"+-.eE".contains(&b[j]))).unwrap_or(b.len());
    let tok = std::str::from_utf8(&b[i..end]).ok()?;
    if !JSON_NUMBER_RX.is_match(tok) {
        return None;
    }
    if tok.bytes().any(|d| matches!(d, b'.' | b'e' | b'E')) {
        if tok.parse::<f64>().ok()?.is_infinite() {
            return Some((tok.len(), "0.0"));
        }
    } else if tok.parse::<i64>().is_err() && tok.parse::<u64>().is_err() {
        return Some((tok.len(), "0"));
    }
    None
}

/// rewrites what python's json module accepts and serde_json rejects, so the same file parses and then fails or
/// skips in the flatten loop exactly as python does:
/// - NaN, Infinity, -Infinity and out-of-range number literals outside strings (see the helpers above)
/// - a lone UTF-16 surrogate escape inside a string: python keeps it as one code point, which is rewritten to
///   `�`. both are one char, and both split words the same way, so searches match; only a snippet that
///   shows the char can differ. a high surrogate followed by a low one is a valid pair and is left alone.
fn py_json_compat(src: &str) -> String {
    let b = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if c == b'"' {
                in_str = false;
                out.push(c);
                i += 1;
            } else if c == b'\\' {
                if let Some(cp) = unicode_escape(b, i) {
                    let pair = (0xD800..0xDC00).contains(&cp)
                        && unicode_escape(b, i + 6).is_some_and(|lo| (0xDC00..0xE000).contains(&lo));
                    if pair {
                        out.extend_from_slice(&b[i..i + 12]);
                        i += 12;
                    } else if (0xD800..0xE000).contains(&cp) {
                        out.extend_from_slice(b"\\uFFFD");
                        i += 6;
                    } else {
                        out.extend_from_slice(&b[i..i + 6]);
                        i += 6;
                    }
                } else {
                    // any other escape: copy the backslash and the escaped char together
                    let take = if i + 1 < b.len() { 2 } else { 1 };
                    out.extend_from_slice(&b[i..i + take]);
                    i += take;
                }
            } else {
                out.push(c);
                i += 1;
            }
        } else if c == b'"' {
            in_str = true;
            out.push(c);
            i += 1;
        } else {
            let before_ok = out.last().is_none_or(|p| b" \t\r\n[,:".contains(p));
            let rewrite = if before_ok { non_json_literal(b, i).or_else(|| out_of_range_number(b, i)) } else { None };
            match rewrite {
                Some((len, rep)) => {
                    out.extend_from_slice(rep.as_bytes());
                    i += len;
                }
                None => {
                    out.push(c);
                    i += 1;
                }
            }
        }
    }
    // only ASCII tokens were replaced and every other byte was copied as is, so the output is still UTF-8
    String::from_utf8_lossy(&out).into_owned()
}

/// reads a chat history file and flattens its message contents into one searchable string, using the mtime cache.
/// an unreadable or unparseable file gives "" (python's json.load try/except); the flatten loop is outside that
/// try in python, so a shape it cannot iterate or a non-str text part is an error for the whole search call.
pub fn history_text(path: &str) -> anyhow::Result<String> {
    let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified()) else { return Ok(String::new()) };
    if let Some((m, t)) = HISTORY_CACHE.lock().get(path) {
        if *m == mtime {
            return Ok(t.clone());
        }
    }
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(String::new()) };
    let Ok(parsed) = serde_json::from_str::<Value>(&py_json_compat(&text)) else { return Ok(String::new()) };
    let messages = match parsed {
        Value::Array(items) => items,
        // python iterates a dict's keys or a str's characters: none for an empty one, a str for a non-empty one
        Value::Object(m) if m.is_empty() => Vec::new(),
        Value::String(s) if s.is_empty() => Vec::new(),
        Value::Object(_) | Value::String(_) => anyhow::bail!("'str' object has no attribute 'get'"),
        other => anyhow::bail!("'{}' object is not iterable", util::py_type_name(&other)),
    };
    let mut parts = Vec::new();
    for msg in messages {
        let Value::Object(msg) = msg else {
            anyhow::bail!("'{}' object has no attribute 'get'", util::py_type_name(&msg));
        };
        let content = match msg.get("content") {
            None => String::new(),
            Some(Value::Array(items)) => {
                let mut texts = Vec::new();
                for p in items {
                    let Value::Object(p) = p else { continue };
                    if p.get("type").and_then(|t| t.as_str()) != Some("text") {
                        continue;
                    }
                    texts.push(match p.get("text") {
                        None => String::new(),
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => anyhow::bail!(
                            "sequence item {}: expected str instance, {} found",
                            texts.len(),
                            util::py_type_name(other)
                        ),
                    });
                }
                texts.join(" ")
            }
            Some(Value::String(s)) => s.clone(),
            Some(_) => continue,
        };
        if !util::py_strip(&content).is_empty() {
            parts.push(content);
        }
    }
    let t = parts.join("\n");
    HISTORY_CACHE.lock().insert(path.to_string(), (mtime, t.clone()));
    Ok(t)
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

/// python `_extract` keeps a bool inside a list-valued field (bool is an int subclass) as str(True) = "True".
/// ol-core's extract keeps only strings and numbers, so the bools are spelled out for ranking only.
fn spell_list_bools(v: &Value) -> Value {
    match v {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| match i {
                    Value::Bool(b) => Value::String(if *b { "True" } else { "False" }.to_string()),
                    other => other.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// python `search` ranking + line formatting over the flattened chat entries. `query` is the tool argument as
/// python receives it, not converted: core search ranks on str(query), but python's make_snippets calls
/// query.strip() on the raw value, so a non-str query with a hit that has text raises.
/// a falsy query (python `not query`: None, "", 0, false, [], {}) and an empty entry list both give no lines.
/// ranking runs on copies that carry the entry's position in "idx", so lines are formatted from the original values.
pub fn search_lines(entries: &[Value], query: &Value) -> anyhow::Result<Vec<String>> {
    if !truthy(query) {
        return Ok(Vec::new());
    }
    let query_text = py_str(query);
    let mut weights = IndexMap::new();
    weights.insert("title".to_string(), 2.0);
    weights.insert("category".to_string(), 1.0);
    weights.insert("text".to_string(), 1.0);
    let ranking_entries: Vec<Value> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            json!({
                "id": e["id"],
                "idx": i,
                "title": spell_list_bools(&e["title"]),
                "category": spell_list_bools(&e["category"]),
                "text": e["text"],
            })
        })
        .collect();
    let results = ol_core::search::search(&ranking_entries, &query_text, "id", Some(&weights), 20);
    let mut lines = Vec::new();
    for hit in results {
        let Some(e) = hit.entry["idx"].as_u64().and_then(|i| entries.get(i as usize)) else { continue };
        let mut line = format!("[{}] \"{}\" (score {:.2})", py_str(&e["category"]), py_str(&e["title"]), hit.score);
        let text = e["text"].as_str().unwrap_or("");
        if !text.is_empty() {
            // python make_snippets calls query.strip() on the raw query, which only a str has
            let Value::String(q) = query else {
                anyhow::bail!("'{}' object has no attribute 'strip'", util::py_type_name(query));
            };
            if let Some(first) = util::make_snippets(text, q, 1, 50).first() {
                line.push_str(&format!("\n  {}", first));
            }
        }
        lines.push(line);
    }
    Ok(lines)
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
            // python Chat.get() raises before the key lookup when no chat is loaded
            if ch.chat.current().is_none() {
                anyhow::bail!("No chat is currently loaded!");
            }
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
                // python passes the raw query value through, so it is not converted here (see search_lines)
                let query = args.raw("query").cloned().unwrap_or(Value::Null);
                let ch = self.base.channel().ok_or_else(util::no_channel)?;
                let mut entries = Vec::new();
                for meta in ch.chat.get_all() {
                    let Some(chat_id) = meta.get("id").filter(|v| truthy(v)).map(py_str) else { continue };
                    let title = meta.get("title").filter(|v| truthy(v)).cloned().unwrap_or(json!(""));
                    let category = meta.get("category").filter(|v| truthy(v)).cloned().unwrap_or(json!(""));
                    let path = ol_core::paths::get_data_path(Some(&format!("{}/history/{}.json", ch.chat.path, chat_id)))?;
                    let text = history_text(&path)?;
                    if !truthy(&title) && text.is_empty() {
                        continue;
                    }
                    entries.push(json!({"id": chat_id, "title": title, "category": category, "text": text}));
                }
                let lines = search_lines(&entries, &query)?;
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
        assert_eq!(history_text(path.to_str().unwrap()).unwrap(), "hello\na b");
    }

    fn history_file(dir: &tempfile::TempDir, body: &str) -> String {
        let path = dir.path().join("h.json");
        std::fs::write(&path, body).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn history_text_is_empty_for_missing_or_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert_eq!(history_text(missing.to_str().unwrap()).unwrap(), "");
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert_eq!(history_text(bad.to_str().unwrap()).unwrap(), "");
    }

    #[test]
    fn history_text_errors_where_python_flatten_loop_raises() {
        let dir = tempfile::tempdir().unwrap();
        let non_list_top = history_file(&dir, r#"{"role": "user", "content": "hi"}"#);
        assert_eq!(history_text(&non_list_top).unwrap_err().to_string(), "'str' object has no attribute 'get'");
        let number_top = history_file(&dir, "5");
        assert_eq!(history_text(&number_top).unwrap_err().to_string(), "'int' object is not iterable");
        let bare_string_msg = history_file(&dir, r#"["just a string"]"#);
        assert_eq!(history_text(&bare_string_msg).unwrap_err().to_string(), "'str' object has no attribute 'get'");
        let null_text = history_file(&dir, r#"[{"content": [{"type": "text", "text": null}]}]"#);
        assert_eq!(
            history_text(&null_text).unwrap_err().to_string(),
            "sequence item 0: expected str instance, NoneType found"
        );
        let number_text = history_file(&dir, r#"[{"content": "ok"}, {"content": [{"type": "text", "text": "a"}, {"type": "text", "text": 5}]}]"#);
        assert_eq!(
            history_text(&number_text).unwrap_err().to_string(),
            "sequence item 1: expected str instance, int found"
        );
    }

    #[test]
    fn history_text_accepts_out_of_range_numbers_like_python() {
        // python reads 1e400 as inf and a 400-digit integer as a big int, so the file parses and the other messages count
        let dir = tempfile::tempdir().unwrap();
        let big = "1".repeat(400);
        let body = format!(
            r#"[{{"content": "ok", "w": 1e400}}, {{"content": "x", "w": -1E400, "v": {big}, "u": 1e-400}}]"#
        );
        let path = history_file(&dir, &body);
        assert_eq!(history_text(&path).unwrap(), "ok\nx");
        // the replacement keeps the python type name, so the flatten-loop error reads the same
        let inf_top = history_file(&dir, "1e400");
        assert_eq!(history_text(&inf_top).unwrap_err().to_string(), "'float' object is not iterable");
        let big_top = history_file(&dir, &big);
        assert_eq!(history_text(&big_top).unwrap_err().to_string(), "'int' object is not iterable");
        // not a JSON number, so it stays a parse failure (python: "Expecting value" or delimiter error)
        let bad_number = history_file(&dir, r#"[{"content": "a", "w": 1.e400}]"#);
        assert_eq!(history_text(&bad_number).unwrap(), "");
    }

    #[test]
    fn history_text_handles_lone_surrogate_escapes_like_python() {
        // python keeps a lone \uD800 as one code point that is searchable text; a valid pair is one emoji
        let dir = tempfile::tempdir().unwrap();
        let path = history_file(&dir, r#"[{"content": "a\uD800b vitamin \uD83D\uDE00 \\uD800 \uDC00"}]"#);
        assert_eq!(history_text(&path).unwrap(), "a\u{FFFD}b vitamin \u{1F600} \\uD800 \u{FFFD}");
        let found = search_lines(&[json!({"id": "c1", "title": "", "category": "", "text": history_text(&path).unwrap()})], &json!("vitamin")).unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn history_text_accepts_python_nonfinite_literals_and_leaves_strings_alone() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"[{"role": "user", "content": "NaN Infinity", "w": NaN},
            {"role": "assistant", "content": [{"type": "text", "text": "x"}], "w": -Infinity, "v": Infinity}]"#;
        let path = history_file(&dir, body);
        assert_eq!(history_text(&path).unwrap(), "NaN Infinity\nx");
        let empty_top = history_file(&dir, "{}");
        assert_eq!(history_text(&empty_top).unwrap(), "");
    }

    #[test]
    fn categories_drop_nested_and_blank_names() {
        let vals = vec![json!("work"), json!("work:sub"), json!(""), json!("home")];
        assert_eq!(filter_categories(&vals).unwrap(), vec!["work".to_string(), "home".to_string()]);
    }

    #[test]
    fn search_lines_format_category_title_score_and_snippet() {
        let entries = vec![json!({"id": "c1", "title": "Diet", "category": "food", "text": "I take vitamins daily"})];
        let lines = search_lines(&entries, &json!("vitamins")).unwrap();
        assert_eq!(lines.len(), 1);
        let parts: Vec<&str> = lines[0].split('\n').collect();
        assert_eq!(parts.len(), 2, "{}", lines[0]);
        assert!(parts[0].starts_with("[food] \"Diet\" (score "), "{}", parts[0]);
        assert!(parts[0].ends_with(')'), "{}", parts[0]);
        assert_eq!(parts[1], "  I take vitamins daily");
        assert!(search_lines(&entries, &json!("")).unwrap().is_empty());
        assert!(search_lines(&entries, &json!("   ")).unwrap().is_empty());
        assert!(search_lines(&entries, &json!("zebra")).unwrap().is_empty());
    }

    #[test]
    fn search_falsy_non_string_query_finds_nothing_like_python() {
        // python core search returns [] for `not query`, so a 0 or false never matches the "0" or "false" in a chat
        let entries = vec![json!({"id": "c1", "title": "Notes", "category": "", "text": "chapter 0 and false alarms"})];
        for query in [json!(0), json!(0.0), json!(false), json!([]), json!({}), Value::Null] {
            assert!(search_lines(&entries, &query).unwrap().is_empty(), "{}", query);
        }
    }

    #[test]
    fn search_truthy_non_string_query_raises_at_snippet_like_python() {
        // python ranks on str(5) = "5", and the hit's text is then sent through make_snippets, which calls 5.strip()
        let with_text = vec![json!({"id": "c1", "title": "Room", "category": "", "text": "room 5 is ready"})];
        let err = search_lines(&with_text, &json!(5)).unwrap_err();
        assert_eq!(err.to_string(), "'int' object has no attribute 'strip'");
        // a hit with no text has no snippet call, so python returns the line
        let title_only = vec![json!({"id": "c2", "title": "Room 5", "category": "", "text": ""})];
        let lines = search_lines(&title_only, &json!(5)).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("[] \"Room 5\" (score "), "{}", lines[0]);
    }

    #[test]
    fn snippet_slices_original_text_at_python_lowercase_position() {
        // python lowers 'İ' to two code points, so the match position is one past the real one and the slice shifts
        assert_eq!(util::make_snippets("İxyz", "xyz", 1, 1), vec!["...xyz".to_string()]);
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
