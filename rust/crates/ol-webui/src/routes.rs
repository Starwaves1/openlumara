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
    export_chat_text, get_strip, or_strip, parse_body, py_contains, py_int, py_int_str, py_iter, py_json_loads_lenient,
    py_json_loads_python_only_true, py_slice, py_sorted_unique, py_strip, py_type_name,
};
use crate::ws::{ensure_chat_set_target, op_chat_delete, op_chat_new, op_chat_rename, op_chat_switch};
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use indexmap::IndexMap;
use ol_core::channel::Channel;
use ol_core::context::GetOpts;
use ol_core::pyjson::{py_str, truthy};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// registers the `/api` chat, stream, input history, settings, library and system routes
pub fn add_api_routes(router: Router<Arc<Shared>>) -> Router<Arc<Shared>> {
    router
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
        .route("/api/libraries/status", get(libraries_status))
        .route("/api/libraries/sync", post(libraries_sync))
        .route("/api/libraries/install", post(libraries_install))
        .route("/api/libraries/update", post(libraries_update))
        .route("/api/libraries/uninstall", post(libraries_uninstall))
        .route("/api/system/data", get(system_data))
        .route("/api/system/context_size", get(system_context_size))
        .route("/api/system/logs", get(system_logs))
        .route("/api/system/restart", post(system_restart))
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

/// `await request.json()` for a body that must be a dict; None where python raises
fn json_object(body: &[u8]) -> Option<Map<String, Value>> {
    serde_json::from_slice::<Value>(body).ok()?.as_object().cloned()
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
    if !["token_usage", "token_usage_mark"].iter().all(|key| ch.chat.get(key).map_or(true, |v| py_int(&v).is_some())) {
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

/// python `input_history` append: dedupes against the last entry, keeps the newest 100
pub fn push_input_history(history: &mut Vec<Value>, message: &str) {
    if history.last().and_then(Value::as_str) == Some(message) {
        return;
    }
    history.push(json!(message));
    while history.len() > 100 {
        history.remove(0);
    }
}

/// python `_get_module_section(config, key)`: `{"enabled", "disabled"}` of a module section, None where absent
fn module_section(section: Option<&Value>) -> Value {
    let obj = match section {
        Some(v) if truthy(v) => v.clone(),
        _ => json!({}),
    };
    json!({
        "enabled": obj.get("enabled").cloned().unwrap_or(Value::Null),
        "disabled": obj.get("disabled").cloned().unwrap_or(Value::Null),
    })
}

/// python settings-save diff: (requires_restart, requires_reconnect)
pub fn restart_flags(old: &Map<String, Value>, new: &Map<String, Value>) -> (bool, bool) {
    let requires_restart = MODULE_CATEGORY_KEYS
        .iter()
        .any(|key| module_section(old.get(*key)) != module_section(new.get(*key)));
    let requires_reconnect = !requires_restart && old.get("api").unwrap_or(&Value::Null) != new.get("api").unwrap_or(&Value::Null);
    (requires_restart, requires_reconnect)
}

/// python `round(x, 1)`
fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
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
    // raises AttributeError for a truthy non-string category, which reaches starlette as the plain 500
    if let Err(error) = validate_category_format(&name) {
        return reply(json!(error), false);
    }
    let chats = ch.chat.data();
    if chats.iter().any(|c| matches!(c.get("category"), Some(v) if truthy(v) && !v.is_string())) {
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
    let target = Value::String(name.clone());
    let general = json!("general");
    let moved = ch.chat.with_data_mut(|chats| {
        let mut moved = 0usize;
        for chat in chats.iter_mut() {
            if chat.get("category") == Some(&target) {
                chat["category"] = general.clone();
                moved += 1;
            }
        }
        moved
    });
    if moved > 0 {
        ch.chat.save_index();
    }
    reply(json!({"moved": moved}), true)
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
    let Some(tz) = tz_offset_of(obj.get("tz_offset")) else { return internal_error() };
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
        results.sort_by(|a, b| updated_of(b).cmp(&updated_of(a)));
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
    history.load();
    push_input_history(&mut history.data, &message);
    let _ = history.save();
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
    match ch.manager().api().list_models().await {
        Ok(models) => reply(json!(models), true),
        Err(e) => reply(json!(e.to_string()), false),
    }
}

async fn model_set(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(obj) = json_object(&body) else { return internal_error() };
    let Ok(name) = or_strip(obj.get("name"), "") else { return internal_error() };
    if name.is_empty() {
        return reply(json!("Model name cannot be empty"), false);
    }
    ch.manager().api().set_model(&name);
    reply(json!({"name": name}), true)
}

async fn settings_save(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(obj) = json_object(&body) else { return internal_error() };
    // python: list(data.get("changed_modules", [])). list() of a string is its characters and of a dict its keys;
    // list() of a number, bool or null raises, which is a plain 500
    let changed_modules: Vec<String> = match obj.get("changed_modules") {
        None => Vec::new(),
        Some(Value::Array(items)) => items.iter().map(py_str).collect(),
        Some(Value::String(s)) => s.chars().map(String::from).collect(),
        Some(Value::Object(m)) => m.keys().cloned().collect(),
        Some(_) => return internal_error(),
    };
    let categories = match obj.get("categories") {
        None => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return internal_error(),
    };
    let Ok(flattened) = flatten_categories(&categories) else { return internal_error() };

    let old_config = ol_core::config::snapshot();
    ol_core::config::replace(flattened.clone());
    let (requires_restart, requires_reconnect) = restart_flags(&old_config, &flattened);

    let manager = ch.manager();
    for name in &changed_modules {
        // python logs a failed reload and carries on with the next module
        let _ = manager.reload_module(name).await;
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

async fn libraries_status(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let manager = ch.manager();
    let mm = manager.module_manager();
    let libraries: Vec<String> = mm.list_libraries().keys().cloned().collect();
    reply(json!({"repos": mm.list_repos(), "libraries": libraries, "modules": mm.list_available()}), true)
}

async fn libraries_sync(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let manager = ch.manager();
    let mm = manager.module_manager();
    if mm.list_repos().is_empty() {
        return reply(json!("No module libraries configured. Add github repo urls under Core settings first."), false);
    }
    let synced = mm.load().await;
    reply(json!({"synced": synced, "modules": mm.list_available()}), true)
}

async fn libraries_install(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(obj) = json_object(&body) else { return internal_error() };
    let (Ok(library), Ok(name)) = (or_strip(obj.get("library"), ""), or_strip(obj.get("name"), "")) else {
        return internal_error();
    };
    let manager = ch.manager();
    match manager.module_manager().install(&library, &name) {
        Ok(message) => reply(json!({"restart_required": true, "message": message}), true),
        Err(message) => reply(json!(message), false),
    }
}

async fn libraries_update(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(obj) = json_object(&body) else { return internal_error() };
    let (Ok(library), Ok(name)) = (or_strip(obj.get("library"), ""), or_strip(obj.get("name"), "")) else {
        return internal_error();
    };
    let manager = ch.manager();
    match manager.module_manager().update(&library, &name).await {
        Ok(message) => reply(json!({"restart_required": true, "message": message}), true),
        Err(message) => reply(json!(message), false),
    }
}

async fn libraries_uninstall(State(app): State<Arc<Shared>>, body: Bytes) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let Some(obj) = json_object(&body) else { return internal_error() };
    let Ok(name) = or_strip(obj.get("name"), "") else { return internal_error() };
    let manager = ch.manager();
    match manager.module_manager().uninstall(&name) {
        Ok(message) => reply(json!({"restart_required": true, "message": message}), true),
        Err(message) => reply(json!(message), false),
    }
}

// ------------------
// system
// ------------------

async fn system_data(State(app): State<Arc<Shared>>) -> Response {
    let Some(_ch) = app.ch() else { return internal_error() };
    let max_context = ol_core::config::get(&["api", "max_context"]).unwrap_or(Value::Null);
    reply(json!({"max_context": max_context}), true)
}

async fn system_context_size(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let mut size = ch.context.get_size().await;
    let threshold = ol_core::config::get_f64(&["model", "context_compression_threshold"], 0.0);
    size["compression_threshold"] = json!(round1(threshold * 100.0));
    let auto = ol_core::config::get(&["model", "automatically_compress_context"]).unwrap_or(Value::Null);
    size["auto_compress"] = json!(truthy(&auto));
    reply(size, true)
}

async fn system_logs(State(app): State<Arc<Shared>>) -> Response {
    let logs: Vec<Value> = app.logs.lock().iter().map(|(category, message)| json!({"category": category, "message": message})).collect();
    reply(json!(logs), true)
}

async fn system_restart(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    ch.manager().restart();
    Json(Value::Null).into_response()
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
    fn restart_flags_follow_module_lists_then_api() {
        let old = json!({"modules": {"enabled": ["a"]}, "api": {"model": "x"}});
        let same = json!({"modules": {"enabled": ["a"]}, "api": {"model": "y"}});
        let moved = json!({"modules": {"enabled": ["a", "b"]}, "api": {"model": "y"}});
        let old = old.as_object().unwrap();
        assert_eq!(restart_flags(old, same.as_object().unwrap()), (false, true));
        assert_eq!(restart_flags(old, moved.as_object().unwrap()), (true, false));
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
