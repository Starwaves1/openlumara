//! Port of the `/api` routes in channels/webui/channel.py that are not static, ui-extension or
//! websocket: chat load/current/export/list/days/categories/tags/search/new/rename/delete,
//! stream state, shell input history, settings, models and connection, module libraries and the
//! system routes. Each handler follows its python body; every answer goes through
//! `server::api_result`, and a python exception that would reach starlette becomes the plain 500.

use crate::listing::{
    chats_for_category, day_label_from_key, group_key, highlight_query, local_day, project_chat, sorted_updated_desc,
    updated_of, updated_strict, validate_category_format, validate_category_name,
};
use crate::server::{api_result, get_only, internal_error, Shared};
use crate::settings::{build_settings_structure, flatten_categories, get_module_info, MODULE_CATEGORY_KEYS};
use crate::util::{
    export_chat_text, get_strip, or_strip, parse_body, py_contains, py_eq, py_int, py_int_str, py_iter,
    py_json_loads, py_json_loads_int_rejects_python_only, py_json_loads_lenient, py_json_loads_python_only_true, py_slice, py_sorted_unique, py_strip,
    py_type_name, BodyJsonError,
};
use crate::ws::{ensure_chat_set_target, op_chat_delete, op_chat_new, op_chat_rename, op_chat_switch};
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Json;
use axum::Router;
use indexmap::IndexMap;
use ol_core::channel::Channel;
use ol_core::context::GetOpts;
use ol_core::storage::StorageList;
use ol_core::pyjson::{py_repr, py_str, truthy};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

mod system;

/// registers the `/api` chat, stream, input history, settings, library and system routes
pub fn add_api_routes(router: Router<Arc<Shared>>) -> Router<Arc<Shared>> {
    let router = router
        .route("/api/chat/load/{chat_id}", get_only(chat_load))
        .route("/api/chat/current", get_only(chat_current))
        .route("/api/stream/state", get_only(stream_state))
        .route("/api/chat/export", get_only(chat_export))
        .route("/api/chats", get_only(get_chats))
        .route("/api/chats/days", get_only(get_chat_days))
        .route("/api/chats/day", get_only(get_chats_for_day))
        .route("/api/chats/categories", get_only(get_chat_categories))
        .route("/api/chats/categories/count", get_only(get_chat_category_counts))
        .route("/api/chats/tags", get_only(get_chat_tags))
        .route("/api/chats/categories/create", post(create_chat_category))
        .route("/api/chats/categories/delete", post(delete_chat_category))
        .route("/api/chats/search", post(search_chats))
        .route("/api/chat/prompt", get_only(get_prompt))
        .route("/api/chat/new", post(chat_new))
        .route("/api/chat/rename/{chat_id}", post(chat_rename))
        .route("/api/chat/delete/{chat_id}", post(chat_delete_by_id))
        .route("/api/chat/set-category/{chat_id}", post(chat_set_category))
        .route("/api/input_history", get_only(input_history_get).post(input_history_add))
        .route("/api/settings/load", get_only(settings_load))
        .route("/api/check_connection", get_only(check_connection))
        .route("/api/models", get_only(models_get))
        .route("/api/model/set", post(model_set))
        .route("/api/settings/save", post(settings_save))
        .route("/api/reconnect", post(reconnect))
        .route("/api/libraries/status", get_only(libraries_status))
        .route("/api/libraries/sync", post(libraries_sync))
        .route("/api/libraries/install", post(libraries_install))
        .route("/api/libraries/update", post(libraries_update))
        .route("/api/libraries/uninstall", post(libraries_uninstall));
    system::add_routes(router)
}

// ------------------
// helpers
// ------------------

/// `api_result(data, success)` as a json response
fn reply(data: Value, success: bool) -> Response {
    Json(api_result(data, success)).into_response()
}

/// `request.query_params`: starlette keeps every occurrence, and `.get` answers the last one
struct QueryParams(Vec<(String, String)>);

impl QueryParams {
    fn parse(raw: Option<&str>) -> Self {
        Self(
            url::form_urlencoded::parse(raw.unwrap_or("").as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// `query_params.getlist(key)`
    fn get_all(&self, key: &str) -> Vec<String> {
        self.0.iter().filter(|(k, _)| k == key).map(|(_, v)| v.clone()).collect()
    }

    /// `int(query_params.get(key, default))`; None where python raises ValueError
    fn int(&self, key: &str, default: i64) -> Option<i64> {
        match self.get(key) {
            Some(s) => py_int_str(s),
            None => Some(default),
        }
    }
}

/// `await request.json()` for a body that must be a dict; None where python raises. python's json.loads
/// takes NaN and Infinity, read here as `true` (a truthy non-string, as python's NaN is)
fn json_object(body: &[u8]) -> Option<Map<String, Value>> {
    py_json_loads_python_only_true(body).ok()?.as_object().cloned()
}

/// python `int(data.get("tz_offset") or 0)` on a search body; None where `int()` raises (nan, inf)
fn search_tz_offset(body: &[u8]) -> Option<i64> {
    let raw = py_json_loads_int_rejects_python_only(body).ok()?.get("tz_offset").cloned();
    tz_offset_of(raw.as_ref())
}

/// `await request.json()` for the library routes, which read string keys with `(data.get(k) or "").strip()`.
/// python's json.loads takes NaN and Infinity; `py_json_loads_python_only_true` keeps those bodies.
fn library_body(body: &[u8]) -> Option<Map<String, Value>> {
    py_json_loads_python_only_true(body).ok()?.as_object().cloned()
}

/// `int(data.get(key) or 0)`; None where python raises
fn tz_offset_of(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(v) if truthy(v) => py_int(v),
        _ => Some(0),
    }
}

/// `category or None` for a query parameter (python `if category:`)
fn category_filter(raw: Option<&str>) -> Option<&str> {
    raw.filter(|c| !c.is_empty())
}

/// python `chat_store.get_all()`: the index sorted by `updated`, newest first (TypeError as Err)
fn all_chats(ch: &Channel) -> Result<Vec<Value>, String> {
    sorted_updated_desc(ch.chat.data())
}

/// python `_chats_for_category(category, tags)` over `get_all()`
fn chats_in(ch: &Channel, category: Option<&str>, tags: &[String]) -> Result<Vec<Value>, String> {
    let all = all_chats(ch)?;
    chats_for_category(&all, category, tags).map(|v| v.into_iter().cloned().collect())
}

/// python `chats[offset:offset + limit]` and `has_more`
fn page_of(chats: &[Value], offset: i64, limit: i64) -> (Vec<Value>, bool) {
    let stop = offset.saturating_add(limit);
    (py_slice(chats, offset, stop), stop < chats.len() as i64)
}

/// python `_group_key` / `_local_day` for every chat: (group key, newest local day, count) in first-seen order
fn group_counts(chats: &[Value], tz: i64) -> Result<IndexMap<String, (i64, String)>, String> {
    let mut groups: IndexMap<String, (i64, String)> = IndexMap::new();
    for chat in chats {
        let updated = updated_strict(chat)?;
        let key = group_key(&updated, tz)?;
        let day = local_day(&updated, tz)?;
        let entry = groups.entry(key).or_insert((0, String::new()));
        entry.0 += 1;
        if day > entry.1 {
            entry.1 = day;
        }
    }
    Ok(groups)
}

/// python `chat.get(...)` dict of the loaded chat, with turn history and live token usage
/// None where python's `get_total_tokens` raises: its `int(chat_data.get(key, 0))` on a stored value python
/// cannot int() (null, list, dict, a non-numeric string) fails before anything else is read
async fn chat_with_details(ch: &Channel) -> Option<Value> {
    if !["token_usage", "token_usage_mark"].iter().all(|key| ch.chat.get(key).is_none_or(|v| py_int(&v).is_some())) {
        return None;
    }
    let mut chat = ch.chat.get_chat();
    chat["turn_history"] = json!(ch.group_history());
    chat["token_usage"] = json!(ch.context.get_total_tokens().await);
    Some(chat)
}

/// python `sorted(set(c or "general" for c in chat_store.get_categories()))` with "general" moved first
pub fn category_names(categories: &[Value]) -> Result<Vec<Value>, String> {
    let mapped: Vec<Value> = categories.iter().map(|c| if truthy(c) { c.clone() } else { json!("general") }).collect();
    let mut out = py_sorted_unique(mapped)?;
    let general = json!("general");
    if let Some(pos) = out.iter().position(|c| *c == general) {
        let g = out.remove(pos);
        out.insert(0, g);
    }
    Ok(out)
}

/// python `{chat.get("category") or "general": count}` over the chat index, first-seen key order.
/// keys compare like python dict keys (True == 1 == 1.0), and json.dumps writes the first-seen key
/// object as its string form (True becomes "true").
pub fn category_counts(chats: &[Value]) -> Result<Map<String, Value>, String> {
    // equality key -> (json key text, count)
    let mut counts: IndexMap<String, (String, i64)> = IndexMap::new();
    for chat in chats {
        // python `chat.get` on a non-dict index entry raises AttributeError
        let Some(obj) = chat.as_object() else {
            return Err(format!("'{}' object has no attribute 'get'", py_type_name(chat)));
        };
        let cat = match obj.get("category") {
            Some(c) if truthy(c) => c.clone(),
            _ => json!("general"),
        };
        if matches!(cat, Value::Array(_) | Value::Object(_)) {
            return Err(format!("unhashable type: '{}'", py_type_name(&cat)));
        }
        counts
            .entry(py_dict_eq_key(&cat))
            .and_modify(|(_, n)| *n += 1)
            .or_insert_with(|| (py_json_key_text(&cat), 1));
    }
    Ok(counts.into_values().map(|(key, n)| (key, json!(n))).collect())
}

/// equality class of a hashable scalar under python `==`/hash: True, 1 and 1.0 share one class
fn py_dict_eq_key(v: &Value) -> String {
    match v {
        Value::Bool(b) => format!("n{}", *b as i64),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => format!("n{i}"),
            (_, Some(u), _) => format!("n{u}"),
            (_, _, Some(f)) if f.fract() == 0.0 && f >= i64::MIN as f64 && f < i64::MAX as f64 => format!("n{}", f as i64),
            (_, _, Some(f)) => format!("f{}", f.to_bits()),
            _ => py_str(v),
        },
        other => format!("s{}", py_str(other)),
    }
}

/// text json.dumps writes for a dict key: str as-is, bool as true/false, numbers as python str()
fn py_json_key_text(v: &Value) -> String {
    match v {
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        other => py_str(other),
    }
}

/// python `sorted(set(t for chat in chats for t in (chat.get("tags") or []) if t))`
pub fn tag_names(chats: &[Value]) -> Result<Vec<Value>, String> {
    let mut items = Vec::new();
    for chat in chats {
        let Some(tags) = chat.get("tags").filter(|t| truthy(t)) else { continue };
        items.extend(py_iter(tags)?.into_iter().filter(truthy));
    }
    py_sorted_unique(items)
}

/// python `all(t in (r.get("tags") or []) for t in tags)`, short-circuiting like `all`
fn has_all_tags(chat: &Value, tags: &[Value]) -> Result<bool, String> {
    let have = match chat.get("tags") {
        Some(v) if truthy(v) => v,
        _ => return Ok(tags.is_empty()),
    };
    for t in tags {
        if !py_contains(have, t)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// python `re.sub(r'[\\/:*?"<>|]', "_", title).strip() or "chat-export"` + ".txt"
pub fn export_filename(title: &str) -> String {
    static UNSAFE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"[\\/:*?"<>|]"#).unwrap());
    let safe = UNSAFE.replace_all(title, "_");
    let stripped = py_strip(&safe);
    let base = if stripped.is_empty() { "chat-export" } else { stripped };
    format!("{}.txt", base)
}

/// python `input_history` append: dedupes against the last entry, keeps the newest 100. false when the
/// message is the last entry, which python answers without saving the file
pub fn push_input_history(history: &mut Vec<Value>, message: &str) -> bool {
    if history.last().and_then(Value::as_str) == Some(message) {
        return false;
    }
    history.push(json!(message));
    while history.len() > 100 {
        history.remove(0);
    }
    true
}

/// python `StorageList.load()` (json) raises when `json.loads` rejects the file text: a plain 500 that
/// leaves the file as it is. an empty file and an unreadable file do not raise (they load as no entries)
fn input_history_file_parses(path: &str) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => text.is_empty() || !matches!(py_json_loads(text.as_bytes()), Err(BodyJsonError::Decode(_))),
        Err(_) => true,
    }
}

/// python `history.load()` followed by the handler's use of it. ol-core's load clears the list before it
/// reads the file, so a file that json.loads rejects leaves the list empty (python's load raises after the
/// clear, and the next GET reads []). false when the file does not parse: the POST is a 500 and is not saved.
/// the file is read on every POST here, where python re-reads only when the file's mtime changed
fn input_history_reload(history: &mut StorageList) -> bool {
    history.load();
    input_history_file_parses(&history.path)
}

/// python `(config.get(key) or {}).get(k)` for `k` in enabled and disabled: `{"enabled", "disabled"}` of a
/// module section, None where python raises AttributeError (a truthy section that is not a dict)
fn module_section(section: Option<&Value>) -> Option<Value> {
    let obj = match section {
        Some(v) if truthy(v) => v.clone(),
        _ => json!({}),
    };
    let Value::Object(map) = &obj else { return None };
    Some(json!({
        "enabled": map.get("enabled").cloned().unwrap_or(Value::Null),
        "disabled": map.get("disabled").cloned().unwrap_or(Value::Null),
    }))
}

/// python settings-save diff: (requires_restart, requires_reconnect). None where python raises while
/// reading a module section. python's `any()` stops at the first differing module key, so a bad section
/// after that key is never read
pub fn restart_flags(old: &Map<String, Value>, new: &Map<String, Value>) -> Option<(bool, bool)> {
    let mut requires_restart = false;
    for key in MODULE_CATEGORY_KEYS {
        if !py_eq(&module_section(old.get(key))?, &module_section(new.get(key))?) {
            requires_restart = true;
            break;
        }
    }
    let requires_reconnect =
        !requires_restart && !py_eq(old.get("api").unwrap_or(&Value::Null), new.get("api").unwrap_or(&Value::Null));
    Some((requires_restart, requires_reconnect))
}

// ------------------
// chat load / current / export / stream state
// ------------------

async fn chat_load(State(app): State<Arc<Shared>>, Path(chat_id): Path<String>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    if !op_chat_switch(&app, &ch, &chat_id).await {
        return reply(json!("error while loading chat"), false);
    }
    let Some(chat) = chat_with_details(&ch).await else { return internal_error() };
    reply(chat, true)
}

async fn chat_current(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    // python `chat.get()` raises 'No chat is currently loaded!' when nothing is loaded
    if ch.chat.current().is_none() {
        return internal_error();
    }
    let Some(chat) = chat_with_details(&ch).await else { return internal_error() };
    reply(chat, true)
}

async fn stream_state(State(app): State<Arc<Shared>>) -> Response {
    reply(json!({"state": app.ws.stream_state()}), true)
}

async fn chat_export(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Ok(content) = export_chat_text(&ch.chat) else { return internal_error() };
    // python `chat.get("title") or ""` fed to re.sub: a truthy non-string title raises TypeError
    let title = match ch.chat.get("title") {
        Some(v) if truthy(&v) => match v {
            Value::String(s) => s,
            _ => return internal_error(),
        },
        _ => String::new(),
    };
    reply(json!({"content": content, "filename": export_filename(&title)}), true)
}

// ------------------
// chat listings
// ------------------

async fn get_chats(State(app): State<Arc<Shared>>, RawQuery(raw): RawQuery) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let q = QueryParams::parse(raw.as_deref());
    let (Some(offset), Some(limit)) = (q.int("offset", 0), q.int("limit", 50)) else { return internal_error() };
    let category = category_filter(q.get("category"));
    let Ok(sorted) = all_chats(&ch) else { return internal_error() };
    let all = match category {
        Some(c) => sorted.into_iter().filter(|x| x.get("category") == Some(&json!(c))).collect(),
        None => sorted,
    };
    let (page, has_more) = page_of(&all, offset, limit);
    let messages: Vec<Value> = page.iter().map(|c| project_chat(c, &[])).collect();
    reply(json!({"messages": messages, "has_more": has_more}), true)
}

async fn get_chat_days(State(app): State<Arc<Shared>>, RawQuery(raw): RawQuery) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let q = QueryParams::parse(raw.as_deref());
    let Some(tz) = q.int("tz_offset", 0) else { return internal_error() };
    let tags = q.get_all("tags");
    let Ok(chats) = chats_in(&ch, category_filter(q.get("category")), &tags) else { return internal_error() };
    let Ok(groups) = group_counts(&chats, tz) else { return internal_error() };

    let mut entries: Vec<(String, i64, String)> = groups.into_iter().map(|(k, (c, newest))| (k, c, newest)).collect();
    // python: groups ordered by their newest member, newest first (stable)
    entries.sort_by(|a, b| b.2.cmp(&a.2));
    let mut out = Vec::new();
    for (key, count, _) in entries {
        let Ok(label) = day_label_from_key(&key, tz) else { return internal_error() };
        out.push(json!({"key": key, "count": count, "label": label}));
    }
    reply(json!(out), true)
}

async fn get_chats_for_day(State(app): State<Arc<Shared>>, RawQuery(raw): RawQuery) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let q = QueryParams::parse(raw.as_deref());
    let day = q.get("day").unwrap_or("").to_string();
    let (Some(offset), Some(limit), Some(tz)) = (q.int("offset", 0), q.int("limit", 50), q.int("tz_offset", 0)) else {
        return internal_error();
    };
    let tags = q.get_all("tags");
    let Ok(chats) = chats_in(&ch, category_filter(q.get("category")), &tags) else { return internal_error() };

    let mut day_chats = Vec::new();
    for chat in chats {
        let Ok(key) = updated_strict(&chat).and_then(|updated| group_key(&updated, tz)) else { return internal_error() };
        if key == day {
            day_chats.push(chat);
        }
    }
    let (page, has_more) = page_of(&day_chats, offset, limit);
    let messages: Vec<Value> = page.iter().map(|c| project_chat(c, &[])).collect();
    reply(json!({"messages": messages, "has_more": has_more, "count": day_chats.len()}), true)
}

async fn get_chat_categories(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Ok(names) = category_names(&ch.chat.get_categories()) else { return internal_error() };
    reply(json!(names), true)
}

async fn get_chat_category_counts(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Ok(counts) = category_counts(&ch.chat.data()) else { return internal_error() };
    reply(Value::Object(counts), true)
}

async fn get_chat_tags(State(app): State<Arc<Shared>>, RawQuery(raw): RawQuery) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let q = QueryParams::parse(raw.as_deref());
    let Ok(chats) = chats_in(&ch, category_filter(q.get("category")), &[]) else { return internal_error() };
    let Ok(tags) = tag_names(&chats) else { return internal_error() };
    reply(json!(tags), true)
}

// ------------------
// categories
// ------------------

async fn create_chat_category(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(obj) = json_object(&body) else { return internal_error() };
    let name = match obj.get("name") {
        Some(v) if truthy(v) => match v.as_str() {
            Some(s) => s.to_string(),
            None => return internal_error(),
        },
        _ => String::new(),
    };
    // python checks the format first; then `(chat.get("category") or "general").lower()` over every chat
    // raises AttributeError for a non-dict entry or a truthy non-string category, which reaches starlette as the plain 500
    if let Err(error) = validate_category_format(&name) {
        return reply(json!(error), false);
    }
    let chats = ch.chat.data();
    if chats.iter().any(|c| !c.is_object() || matches!(c.get("category"), Some(v) if truthy(v) && !v.is_string())) {
        return internal_error();
    }
    let category = match validate_category_name(&name, &chats) {
        Ok(c) => c,
        Err(error) => return reply(json!(error), false),
    };
    let new_id = op_chat_new(&app, &ch, &category).await;
    reply(json!({"id": new_id}), true)
}

async fn delete_chat_category(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(obj) = json_object(&body) else { return internal_error() };
    let Ok(name) = or_strip(obj.get("name"), "") else { return internal_error() };
    if name.is_empty() || name == "general" {
        return reply(json!("'general' cannot be deleted"), false);
    }
    // python raises AttributeError on a non-dict index entry: starlette's plain 500, and nothing is saved
    let Ok(moved) = ch.chat.with_data_mut(|chats| move_category_chats(chats, &name)) else { return internal_error() };
    if moved > 0 {
        ch.chat.save_index();
    }
    reply(json!({"moved": moved}), true)
}

/// python `delete_chat_category` loop: every chat whose category equals `name` moves to 'general'.
/// A non-dict index entry raises on `chat.get`; the moves made before it stay in memory (python does not roll back)
fn move_category_chats(chats: &mut [Value], name: &str) -> Result<usize, String> {
    let mut moved = 0usize;
    for chat in chats.iter_mut() {
        if !chat.is_object() {
            return Err(format!("'{}' object has no attribute 'get'", py_type_name(chat)));
        }
        if chat.get("category").and_then(Value::as_str) == Some(name) {
            chat["category"] = json!("general");
            moved += 1;
        }
    }
    Ok(moved)
}

// ------------------
// search
// ------------------

/// python `search_chats` projection of one hit: group fields and highlighted title/snippets
fn search_entry(chat: &Value, query: &str, tz: i64) -> Result<Value, String> {
    let group = group_key(&updated_strict(chat)?, tz)?;
    let mut entry = project_chat(chat, &["title_match"]);
    entry["group_key"] = json!(if group.is_empty() { "undated".to_string() } else { group.clone() });
    entry["group_label"] = json!(day_label_from_key(&group, tz)?);
    // python `highlight_query(chat.get("title") or "", query)`; a truthy non-string title is rejected in search_chats first
    let title = chat.get("title").and_then(|t| t.as_str()).unwrap_or("");
    entry["title_highlighted"] = json!(highlight_query(Some(title), query));
    // python `[highlight_query(s, query) for s in (chat.get("message_snippets") or [])]`: a null snippet stays null
    let snippets = match chat.get("message_snippets") {
        Some(v) if truthy(v) => py_iter(v)?,
        _ => Vec::new(),
    };
    let snippets: Vec<Value> = snippets
        .iter()
        .map(|s| match s {
            Value::Null => Value::Null,
            other => {
                    let text = py_str(other);
                    json!(highlight_query(Some(text.as_str()), query))
                }
        })
        .collect();
    entry["snippets_highlighted"] = json!(snippets);
    Ok(entry)
}

async fn search_chats(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(obj) = json_object(&body) else { return internal_error() };
    let Ok(query) = get_strip(obj.get("query"), "") else { return internal_error() };
    let search_in_content = obj.get("search_in_content").map(truthy).unwrap_or(true);
    let category = obj.get("category").cloned().unwrap_or(Value::Null);
    let Some(tz) = search_tz_offset(&body) else { return internal_error() };
    if query.is_empty() {
        return reply(json!([]), true);
    }

    let mut results = ch.chat.search(&query, 100, search_in_content);
    // python core ChatStore.search runs `(entry.get("title") or "").lower()` for every ranked hit, before any
    // filter, so a truthy non-string title raises AttributeError: the plain 500
    if results.iter().any(|r| matches!(r.get("title"), Some(v) if truthy(v) && !v.is_string())) {
        return internal_error();
    }
    let general = json!("general");
    if truthy(&category) && category != general {
        results.retain(|r| r.get("category") == Some(&category));
    } else if category == general {
        results.retain(|r| {
            let c = r.get("category").cloned().unwrap_or(Value::Null);
            !truthy(&c) || c == general
        });
    }

    // python `data.get("tags") or []` is iterated per result, so a non-iterable tags value only fails
    // when there is at least one result to test
    if let Some(tags_value) = obj.get("tags").filter(|t| truthy(t)) {
        if !results.is_empty() {
            let Ok(tags) = py_iter(tags_value) else { return internal_error() };
            let mut kept = Vec::new();
            for r in results {
                match has_all_tags(&r, &tags) {
                    Ok(true) => kept.push(r),
                    Ok(false) => {}
                    Err(_) => return internal_error(),
                }
            }
            results = kept;
        }
    }

    // python: updated-descending unless sort=relevance (the BM25 order is kept then)
    let sort = obj.get("sort").filter(|v| truthy(v)).cloned().unwrap_or(json!("updated"));
    if sort != json!("relevance") {
        results.sort_by_key(|a| std::cmp::Reverse(updated_of(a)));
    }

    let mut projected = Vec::new();
    for chat in &results {
        match search_entry(chat, &query, tz) {
            Ok(entry) => projected.push(entry),
            Err(_) => return internal_error(),
        }
    }
    reply(json!(projected), true)
}

// ------------------
// prompt / new / rename / delete / set-category
// ------------------

async fn get_prompt(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let opts = GetOpts { system_prompt: true, end_prompt: false, history: false, ..Default::default() };
    match ch.context.get(opts).await {
        Ok(Some(messages)) => match messages.last() {
            Some(last) => reply(last.get("content").cloned().unwrap_or(Value::Null), true),
            None => internal_error(),
        },
        Ok(None) => internal_error(),
        // python returns the APIError object itself as `data`; fastapi encodes its vars(): message, then exc
        Err(e) => reply(json!({"message": e.message, "exc": e.exc}), false),
    }
}

async fn chat_new(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    // python: any failure reading the body keeps the category at 'general'. python's json.loads takes
    // NaN and Infinity too, which the lenient reader reads as null: the same 'general' for this key
    let mut category = "general".to_string();
    if let Some(obj) = py_json_loads_lenient(&body).ok().and_then(|v| v.as_object().cloned()) {
        if let Ok(c) = or_strip(obj.get("category"), "general") {
            category = if c.is_empty() { "general".to_string() } else { c };
        }
    }
    if category != "general" {
        match validate_category_format(&category) {
            Ok(c) => category = c,
            Err(error) => return reply(json!(error), false),
        }
    }
    let new_id = op_chat_new(&app, &ch, &category).await;
    reply(json!({"id": new_id}), true)
}

async fn chat_rename(State(app): State<Arc<Shared>>, Path(chat_id): Path<String>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let data = match parse_body(&body) {
        Ok(v) => v,
        Err(message) => return reply(json!(message), false),
    };
    let Some(obj) = data.as_object() else {
        return reply(json!(format!("'{}' object has no attribute 'get'", py_type_name(&data))), false);
    };
    let new_title = match get_strip(obj.get("title"), "") {
        Ok(t) => t,
        Err(message) => return reply(json!(message), false),
    };
    if new_title.is_empty() {
        return reply(json!("Title cannot be empty"), false);
    }
    match op_chat_rename(&app, &ch, json!(new_title), Some(&chat_id)).await {
        Ok(true) => reply(Value::Null, true),
        Ok(false) => reply(json!("Chat not found"), false),
        Err(e) => reply(json!(e.to_string()), false),
    }
}

async fn chat_delete_by_id(State(app): State<Arc<Shared>>, Path(chat_id): Path<String>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    // python: a failure inside op_chat_delete is an unhandled exception, i.e. a plain 500
    if op_chat_delete(&app, &ch, &chat_id).await.is_err() {
        return internal_error();
    }
    reply(Value::Null, true)
}

async fn chat_set_category(State(app): State<Arc<Shared>>, Path(chat_id): Path<String>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let data = match parse_body(&body) {
        Ok(v) => v,
        Err(message) => return reply(json!(message), false),
    };
    let Some(obj) = data.as_object() else {
        return reply(json!(format!("'{}' object has no attribute 'get'", py_type_name(&data))), false);
    };
    let category = match or_strip(obj.get("category"), "") {
        Ok(c) => c,
        Err(message) => return reply(json!(message), false),
    };
    if category.is_empty() {
        return reply(json!("Category cannot be empty"), false);
    }
    let Some(index) = ch.chat.find_index(&chat_id) else {
        return reply(json!("Chat not found"), false);
    };
    let set = ensure_chat_set_target(&ch, index).and_then(|()| ch.chat.set("category", json!(category), Some(index)));
    match set {
        Ok(_) => {
            ch.chat.save_index();
            reply(Value::Null, true)
        }
        Err(e) => reply(json!(e.to_string()), false),
    }
}

// ------------------
// input history
// ------------------

async fn input_history_get(State(app): State<Arc<Shared>>) -> Response {
    let history = app.input_history.lock().data.clone();
    reply(Value::Array(history), true)
}

async fn input_history_add(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    // python: request.json() is json.loads, which takes NaN and Infinity. a NaN message is truthy and
    // fails at .strip() (a plain 500), which the python-only reader's stand-in does too
    let Some(obj) = py_json_loads_python_only_true(&body).ok().and_then(|v| v.as_object().cloned()) else {
        return internal_error();
    };
    let Ok(message) = or_strip(obj.get("message"), "") else { return internal_error() };
    if message.is_empty() {
        return reply(Value::Null, true);
    }
    let mut history = app.input_history.lock();
    if !input_history_reload(&mut history) {
        return internal_error();
    }
    // python returns the list without saving when the message repeats the last entry
    if push_input_history(&mut history.data, &message) {
        let _ = history.save();
    }
    reply(Value::Array(history.data.clone()), true)
}

// ------------------
// settings / models / connection
// ------------------

async fn settings_load(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let (module_info, core_sections) = get_module_info();
    let Ok(structure) = build_settings_structure(&ol_core::config::snapshot(), &module_info, &core_sections) else {
        return internal_error();
    };
    let show_unsafe = truthy(&ch.config.get1("show_unsafe_settings").unwrap_or(Value::Null));
    reply(json!({"categories": structure, "show_unsafe_settings": show_unsafe}), true)
}

async fn check_connection(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    if ch.manager().api().connected() {
        reply(json!(true), true)
    } else {
        reply(json!("not connected"), false)
    }
}

async fn models_get(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let manager = ch.manager();
    let api = manager.api();
    // python list_models() returns the attempt_connect() result when disconnected: True once the connect
    // succeeds (sent as data, with no model list), or the error as the failure string
    if !api.connected() {
        return match api.attempt_connect().await {
            Ok(()) => reply(json!(true), true),
            Err(e) => reply(json!(e.to_string()), false),
        };
    }
    match api.list_models().await {
        Ok(models) => reply(json!(models), true),
        Err(e) => reply(json!(e.to_string()), false),
    }
}

/// python `data = await request.json()` then `(data.get("name") or "").strip()`. None where python raises:
/// a body that is not an object, or a truthy name that is not a string (NaN and Infinity are truthy)
fn model_name_of(body: &[u8]) -> Option<String> {
    let obj = py_json_loads_python_only_true(body).ok()?.as_object().cloned()?;
    or_strip(obj.get("name"), "").ok()
}

async fn model_set(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(name) = model_name_of(&body) else { return internal_error() };
    if name.is_empty() {
        return reply(json!("Model name cannot be empty"), false);
    }
    // python set_model() does config["model"]["name"] = name: a missing or non-dict model section raises
    // there, a plain 500 that leaves the config as it was
    if !matches!(ol_core::config::get(&["model"]), Some(Value::Object(_))) {
        return internal_error();
    }
    ch.manager().api().set_model(&name);
    reply(json!({"name": name}), true)
}

/// python `list(data.get("changed_modules", []))`. list() of a string is its characters and of a dict its
/// keys; list() of a number, bool or null raises (None here), which is a plain 500
fn changed_modules_of(obj: &Map<String, Value>) -> Option<Vec<Value>> {
    match obj.get("changed_modules") {
        None => Some(Vec::new()),
        Some(Value::Array(items)) => Some(items.clone()),
        Some(Value::String(s)) => Some(s.chars().map(|c| json!(c.to_string())).collect()),
        Some(Value::Object(m)) => Some(m.keys().map(|k| json!(k)).collect()),
        Some(_) => None,
    }
}

/// the body of settings/save: `(changed_modules, categories)`. python's `await request.json()` takes
/// NaN and Infinity, which the lenient reader reads as null (a plain 500 where python raises on them)
fn settings_save_request(body: &[u8]) -> Option<(Vec<Value>, Map<String, Value>)> {
    let obj = py_json_loads_lenient(body).ok()?.as_object().cloned()?;
    let changed_modules = changed_modules_of(&obj)?;
    let categories = match obj.get("categories") {
        None => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return None,
    };
    Some((changed_modules, categories))
}

async fn settings_save(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some((changed_modules, categories)) = settings_save_request(&body) else { return internal_error() };
    let Ok(flattened) = flatten_categories(&categories) else { return internal_error() };

    let old_config = ol_core::config::snapshot();
    ol_core::config::replace(flattened.clone());
    // python reads the module sections after the save, so a bad section is a 500 with the config saved
    let Some((requires_restart, requires_reconnect)) = restart_flags(&old_config, &flattened) else {
        return internal_error();
    };

    let manager = ch.manager();
    for item in &changed_modules {
        match item {
            // python: reload_module(name) logs a failed reload itself and carries on with the next module
            Value::String(name) => {
                manager.reload_module(name).await;
            }
            // python's `name not in self.modules` raises for an unhashable list or dict, which settings_save
            // logs as its reload error and carries on past
            Value::Array(_) | Value::Object(_) => ch.log(
                &ch.name,
                &format!("Error reloading module {}: unhashable type: '{}'", py_repr(item), py_type_name(item)),
            ),
            // any other value is not a loaded module name: python logs that and returns False
            other => manager.log("core", &format!("Module {} not loaded, cannot reload", py_str(other))),
        }
    }
    app.setup_ui_extensions();

    reply(json!({"requires_restart": requires_restart, "requires_reconnect": requires_reconnect}), true)
}

async fn reconnect(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    match ch.manager().api().reconnect().await {
        Ok(()) => reply(Value::Null, true),
        Err(e) => reply(json!(e.to_string()), false),
    }
}

// ------------------
// module libraries
// ------------------

/// python `ModuleManager.list_available()`, run off the async runtime (its docstrings come from python)
async fn library_modules(libraries: std::collections::BTreeMap<String, std::path::PathBuf>) -> Vec<Value> {
    tokio::task::spawn_blocking(move || crate::util::scan_library_modules(&libraries)).await.unwrap_or_default()
}

async fn libraries_status(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let manager = ch.manager();
    let mm = manager.module_manager();
    let libraries = mm.list_libraries();
    let names: Vec<String> = libraries.keys().cloned().collect();
    let modules = library_modules(libraries).await;
    reply(json!({"repos": mm.list_repos(), "libraries": names, "modules": modules}), true)
}

async fn libraries_sync(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let manager = ch.manager();
    let mm = manager.module_manager();
    if mm.list_repos().is_empty() {
        return reply(json!("No module libraries configured. Add github repo urls under Core settings first."), false);
    }
    let synced = mm.load().await;
    let modules = library_modules(mm.list_libraries()).await;
    reply(json!({"synced": synced, "modules": modules}), true)
}

async fn libraries_install(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(obj) = library_body(&body) else { return internal_error() };
    let (Ok(library), Ok(name)) = (or_strip(obj.get("library"), ""), or_strip(obj.get("name"), "")) else {
        return internal_error();
    };
    let manager = ch.manager();
    match manager.module_manager().install(&library, &name) {
        Ok(message) => {
            // python: install_module_deps on the package folder picked by the copy's files; a failure is logged, the response still succeeds
            let dir = crate::util::module_deps_folder(std::path::Path::new(&message), &name);
            let dep_name = name.clone();
            let outcome = run_module_deps(manager.clone(), move |logs| {
                crate::util::install_module_deps(&dir, &dep_name, logs)
            })
            .await;
            if let Err(detail) = outcome {
                ch.log("libraries", &format!("dependency install failed for {name}: {detail}"));
            }
            reply(json!({"restart_required": true, "message": message}), true)
        }
        Err(message) => reply(json!(message), false),
    }
}

/// runs a blocking module-dependency step (pip, interpreter calls) off the async runtime. The log lines
/// it collects go through the manager, as python's manager.log does; its error is returned to the caller.
async fn run_module_deps<F>(manager: std::sync::Arc<ol_core::manager::Manager>, job: F) -> Result<bool, String>
where
    F: FnOnce(&mut Vec<(String, String)>) -> Result<bool, String> + Send + 'static,
{
    let (logs, result) = tokio::task::spawn_blocking(move || {
        let mut logs = Vec::new();
        let result = job(&mut logs);
        (logs, result)
    })
    .await
    .unwrap_or_else(|e| (Vec::new(), Err(e.to_string())));
    for (category, message) in logs {
        manager.log(&category, &message);
    }
    result
}

async fn libraries_update(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(obj) = library_body(&body) else { return internal_error() };
    let (Ok(library), Ok(name)) = (or_strip(obj.get("library"), ""), or_strip(obj.get("name"), "")) else {
        return internal_error();
    };
    let manager = ch.manager();
    match manager.module_manager().update(&library, &name).await {
        Ok(message) => {
            // python: install_module_deps on the package folder picked by the copy's files; a failure is logged, the response still succeeds
            let dir = crate::util::module_deps_folder(std::path::Path::new(&message), &name);
            let dep_name = name.clone();
            let outcome = run_module_deps(manager.clone(), move |logs| {
                crate::util::install_module_deps(&dir, &dep_name, logs)
            })
            .await;
            if let Err(detail) = outcome {
                ch.log("libraries", &format!("dependency install failed for {name}: {detail}"));
            }
            reply(json!({"restart_required": true, "message": message}), true)
        }
        Err(message) => reply(json!(message), false),
    }
}

async fn libraries_uninstall(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(obj) = library_body(&body) else { return internal_error() };
    let Ok(name) = or_strip(obj.get("name"), "") else { return internal_error() };
    let manager = ch.manager();
    // python: dependency cleanup for user_modules then user_channels runs before the folder is removed,
    // even when the module is not installed (then it is a no-op)
    for sub in ["user_modules", "user_channels"] {
        let dir = ol_core::paths::project_root().join(sub).join(&name);
        let dep_name = name.clone();
        let outcome = run_module_deps(manager.clone(), move |logs| {
            let still_needed = crate::util::deps_still_needed(logs);
            crate::util::uninstall_module_deps(&dir, &dep_name, &still_needed, logs)
        })
        .await;
        if let Err(detail) = outcome {
            ch.log("libraries", &format!("dependency cleanup failed for {name}: {detail}"));
        }
    }
    match manager.module_manager().uninstall(&name) {
        Ok(message) => reply(json!({"restart_required": true, "message": message}), true),
        Err(message) => reply(json!(message), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_put_general_first_and_map_falsy_to_general() {
        let cats = vec![json!("work"), Value::Null, json!("general"), json!("alpha"), json!("")];
        assert_eq!(category_names(&cats).unwrap(), vec![json!("general"), json!("alpha"), json!("work")]);
        // python sorts a mix of strings and numbers in a set: TypeError
        assert!(category_names(&[json!("work"), json!(3)]).is_err());
    }

    #[test]
    fn category_counts_group_missing_and_empty_under_general() {
        let chats = vec![json!({"category": "work"}), json!({}), json!({"category": ""}), json!({"category": "work"})];
        let counts = category_counts(&chats).unwrap();
        assert_eq!(counts.get("work"), Some(&json!(2)));
        assert_eq!(counts.get("general"), Some(&json!(2)));
        assert!(category_counts(&[json!({"category": ["x"]})]).is_err());
    }

    #[test]
    fn category_counts_keep_python_dict_key_semantics() {
        // json.dumps writes a bool key as "true"; True, 1 and 1.0 are one dict key
        let counts = category_counts(&[json!({"category": true}), json!({"category": 1}), json!({"category": 1.0})]).unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts.get("true"), Some(&json!(3)));
        // a non-dict index entry is python's AttributeError on chat.get
        assert!(category_counts(&[json!({"category": "work"}), json!("work")]).is_err());
        assert!(category_counts(&[json!(null)]).is_err());
    }

    #[test]
    fn delete_category_moves_matching_chats_to_general() {
        let mut chats = vec![json!({"category": "work"}), json!({"category": "home"}), json!({"category": "work"}), json!({})];
        assert_eq!(move_category_chats(&mut chats, "work").unwrap(), 2);
        assert_eq!(chats[0]["category"], json!("general"));
        assert_eq!(chats[1]["category"], json!("home"));
        assert_eq!(chats[2]["category"], json!("general"));
        assert_eq!(move_category_chats(&mut chats, "missing").unwrap(), 0);
    }

    #[test]
    fn delete_category_stops_at_a_non_dict_entry_keeping_earlier_moves() {
        // python: AttributeError on chat.get at the list entry; the route returns the plain 500 and does not save
        let mut chats = vec![json!({"category": "work"}), json!("work"), json!({"category": "work"})];
        assert!(move_category_chats(&mut chats, "work").is_err());
        assert_eq!(chats[0]["category"], json!("general"));
        assert_eq!(chats[2]["category"], json!("work"));
    }

    #[test]
    fn export_filename_replaces_unsafe_characters() {
        assert_eq!(export_filename("a/b:c*?\"<>|d"), "a_b_c______d.txt");
        assert_eq!(export_filename("   "), "chat-export.txt");
    }

    #[test]
    fn input_history_dedupes_last_and_caps_at_100() {
        let mut history: Vec<Value> = Vec::new();
        push_input_history(&mut history, "ls");
        push_input_history(&mut history, "ls");
        assert_eq!(history, vec![json!("ls")]);
        for i in 0..105 {
            push_input_history(&mut history, &format!("cmd {}", i));
        }
        assert_eq!(history.len(), 100);
        assert_eq!(history.first(), Some(&json!("cmd 5")));
        assert_eq!(history.last(), Some(&json!("cmd 104")));
    }

    #[test]
    fn input_history_push_reports_a_save_only_when_it_appends() {
        let mut history: Vec<Value> = vec![json!("ls")];
        // a repeat of the last entry is answered without a save, like python's early return
        assert!(!push_input_history(&mut history, "ls"));
        assert_eq!(history, vec![json!("ls")]);
        assert!(push_input_history(&mut history, "cd /tmp"));
        assert_eq!(history, vec![json!("ls"), json!("cd /tmp")]);
    }

    #[test]
    fn input_history_file_that_json_loads_rejects_is_a_500() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, text: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, text).unwrap();
            p.to_string_lossy().to_string()
        };
        // python json.loads raises on these: the handler answers 500 and leaves the file alone
        assert!(!input_history_file_parses(&file("truncated.json", "[\"ls\", \"cd")));
        assert!(!input_history_file_parses(&file("garbage.json", "not json")));
        // an empty file loads as no entries, and valid json (or python's NaN) does not raise
        assert!(input_history_file_parses(&file("empty.json", "")));
        assert!(input_history_file_parses(&file("list.json", "[\"ls\"]")));
        assert!(input_history_file_parses(&file("nan.json", "[NaN]")));
        // a file that cannot be read is logged by python's _read and does not raise
        assert!(input_history_file_parses(&dir.path().join("missing.json").to_string_lossy()));
    }

    #[test]
    fn input_history_reload_empties_the_list_when_the_file_is_corrupt() {
        // python load() clears the list, then json.loads raises: the POST is a 500 and the list reads []
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("history").to_string_lossy().to_string();
        let mut history = StorageList::with_autoload("webui_input_history_test", ol_core::storage::StorageType::Json, Some(&base), false).unwrap();
        std::fs::write(&history.path, "[\"ls\", \"cd").unwrap();
        history.data = vec![json!("old")];
        assert!(!input_history_reload(&mut history));
        assert!(history.data.is_empty());
    }

    #[test]
    fn input_history_reload_loads_a_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("history").to_string_lossy().to_string();
        let mut history = StorageList::with_autoload("webui_input_history_test", ol_core::storage::StorageType::Json, Some(&base), false).unwrap();
        std::fs::write(&history.path, "[\"ls\", \"cd /tmp\"]").unwrap();
        assert!(input_history_reload(&mut history));
        assert_eq!(history.data, vec![json!("ls"), json!("cd /tmp")]);
    }

    #[test]
    fn category_and_search_bodies_accept_python_nan_and_infinity() {
        // python json.loads takes NaN and Infinity: an unused key holding them is not a 500
        let obj = json_object(br#"{"name":"x","n":NaN,"m":-Infinity}"#).unwrap();
        assert_eq!(obj.get("name"), Some(&json!("x")));
        // a non-object body is still rejected
        assert!(json_object(b"[NaN]").is_none());
    }

    #[test]
    fn search_tz_offset_fails_on_nan_like_python_int() {
        // python int(nan) raises ValueError and int(-inf) raises OverflowError: both are a 500
        assert_eq!(search_tz_offset(br#"{"tz_offset": 300}"#), Some(300));
        assert_eq!(search_tz_offset(br#"{"query": "a"}"#), Some(0));
        assert_eq!(search_tz_offset(br#"{"tz_offset": NaN}"#), None);
        assert_eq!(search_tz_offset(br#"{"tz_offset": -Infinity}"#), None);
    }

    #[test]
    fn restart_flags_follow_module_lists_then_api() {
        let old = json!({"modules": {"enabled": ["a"]}, "api": {"model": "x"}});
        let same = json!({"modules": {"enabled": ["a"]}, "api": {"model": "y"}});
        let moved = json!({"modules": {"enabled": ["a", "b"]}, "api": {"model": "y"}});
        let old = old.as_object().unwrap();
        assert_eq!(restart_flags(old, same.as_object().unwrap()), Some((false, true)));
        assert_eq!(restart_flags(old, moved.as_object().unwrap()), Some((true, false)));
    }

    #[test]
    fn restart_flags_compare_numbers_the_python_way() {
        // python: 1 == 1.0, so an integral float in the api section is no reconnect
        let old = json!({"api": {"timeout": 1, "retries": 0.0}});
        let new = json!({"api": {"timeout": 1.0, "retries": 0}});
        assert_eq!(restart_flags(old.as_object().unwrap(), new.as_object().unwrap()), Some((false, false)));
        let changed = json!({"api": {"timeout": 2.0, "retries": 0}});
        assert_eq!(restart_flags(old.as_object().unwrap(), changed.as_object().unwrap()), Some((false, true)));
    }

    #[test]
    fn restart_flags_fail_on_a_truthy_non_dict_module_section_only_when_read() {
        // python `(section or {}).get(k)` raises on a truthy list or string section
        let bad = json!({"modules": ["x"]});
        let fine = json!({});
        assert_eq!(restart_flags(bad.as_object().unwrap(), fine.as_object().unwrap()), None);
        // falsy non-dicts read as {} and are no error
        let falsy = json!({"modules": 0, "channels": ""});
        assert_eq!(restart_flags(falsy.as_object().unwrap(), fine.as_object().unwrap()), Some((false, false)));
        // python's any() stops at the first differing key, so a bad section after it is never read
        let old = json!({"modules": {"enabled": ["a"]}, "channels": "bad"});
        let new = json!({"modules": {"enabled": ["b"]}});
        assert_eq!(restart_flags(old.as_object().unwrap(), new.as_object().unwrap()), Some((true, false)));
    }

    #[test]
    fn model_name_follows_python_strip_and_nan_truthiness() {
        assert_eq!(model_name_of(br#"{"name": "  qwen-3  "}"#), Some("qwen-3".to_string()));
        // NaN elsewhere in the body is accepted, like python's json.loads
        assert_eq!(model_name_of(br#"{"name": "m", "x": NaN}"#), Some("m".to_string()));
        // a NaN name is truthy and has no .strip(): a plain 500
        assert_eq!(model_name_of(br#"{"name": NaN}"#), None);
        assert_eq!(model_name_of(br#"{"name": 5}"#), None);
        assert_eq!(model_name_of(b"[]"), None);
        assert_eq!(model_name_of(br#"{}"#), Some(String::new()));
    }

    #[test]
    fn changed_modules_coerce_like_python_list() {
        let obj = json!({"changed_modules": "ab"});
        assert_eq!(changed_modules_of(obj.as_object().unwrap()), Some(vec![json!("a"), json!("b")]));
        // non-string entries are kept as they are, not converted to strings
        let obj = json!({"changed_modules": [5, null, "m"]});
        assert_eq!(changed_modules_of(obj.as_object().unwrap()), Some(vec![json!(5), Value::Null, json!("m")]));
        let obj = json!({"changed_modules": {"k": 1}});
        assert_eq!(changed_modules_of(obj.as_object().unwrap()), Some(vec![json!("k")]));
        // list() of a number, bool or null raises
        for bad in [json!(5), json!(true), Value::Null] {
            assert_eq!(changed_modules_of(json!({"changed_modules": bad}).as_object().unwrap()), None);
        }
        assert_eq!(changed_modules_of(&Map::new()), Some(vec![]));
    }

    #[test]
    fn settings_save_body_accepts_nan_like_python_json_loads() {
        let (modules, categories) = settings_save_request(br#"{"categories": {}, "x": NaN}"#).unwrap();
        assert!(modules.is_empty() && categories.is_empty());
        // a NaN changed_modules is a list() error in python: a plain 500
        assert!(settings_save_request(br#"{"changed_modules": NaN, "categories": {}}"#).is_none());
        assert!(settings_save_request(b"[]").is_none());
        assert!(settings_save_request(br#"{"categories": 3}"#).is_none());
    }

    #[test]
    fn query_params_follow_starlette_get_and_getlist() {
        let q = QueryParams::parse(Some("tags=a&tags=b&offset=3&offset=7&offset=x"));
        assert_eq!(q.get_all("tags"), vec!["a", "b"]);
        assert_eq!(q.get("offset"), Some("x"));
        assert_eq!(q.int("limit", 50), Some(50));
        assert_eq!(QueryParams::parse(Some("offset=7")).int("offset", 0), Some(7));
    }

    #[test]
    fn page_of_matches_python_slicing_and_has_more() {
        let chats: Vec<Value> = (0..5).map(|i| json!(i)).collect();
        let (page, more) = page_of(&chats, 1, 2);
        assert_eq!(page, vec![json!(1), json!(2)]);
        assert!(more);
        let (page, more) = page_of(&chats, 4, 10);
        assert_eq!(page, vec![json!(4)]);
        assert!(!more);
    }
}
