//! Port of channels/matrix/channel.py: talk to your AI over Matrix.
//!
//! matrix-nio is replaced by a thin client-server API client ([`client::MxClient`]) and
//! a /sync loop. Login (config access token, saved credentials in
//! `data/matrix_credentials.json`, or password login), auto-joining invites, the
//! streaming send/edit (m.replace) logic, typing notifications, deduplication and the
//! startup grace period behave like the python channel.
//!
//! **End-to-end encryption is not supported** (matrix-nio[e2e]/libolm has no lightweight
//! Rust equivalent; matrix-sdk-crypto is far too heavy for this port). Encrypted
//! events can't be read: the channel logs them like python logs undecryptable events
//! and, once per room, posts a notice that encrypted rooms are not supported. Use
//! unencrypted rooms with the Rust port. SAS verification, key sharing and device
//! trust handling are therefore not ported either.

pub mod client;
pub mod rooms;

#[path = "../../../../generated/channels/matrix.rs"]
mod meta;

use crate::util::head;
use client::{MxClient, MxError};
use futures::StreamExt;
use ol_core::channel::{Channel, ChannelBehavior, SendArgs};
use ol_core::pyjson::{py_str, truthy};
use ol_core::registry::{ChannelDescriptor, Registry};
use parking_lot::Mutex;
use rooms::{RoomEvent, Rooms};
use serde_json::{json, Map, Value};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// seconds between streaming edits
pub const EDIT_INTERVAL: Duration = Duration::from_secs(2);
const SYNC_TIMEOUT_MS: u64 = 30000;
const MAX_PROCESSED_EVENTS: usize = 1000;

pub const ENCRYPTED_ROOM_NOTICE: &str = "⚠️ This room is end-to-end encrypted. Encrypted rooms are not supported by the Rust port of OpenLumara, so I can't read messages here. Please talk to me in an unencrypted room.";

pub fn register(r: &mut Registry) {
    r.add_channel(ChannelDescriptor {
        name: "matrix",
        class_name: meta::CLASS_NAME,
        doc: meta::DOC,
        settings: meta::settings,
        unsafe_: meta::UNSAFE,
        category: meta::CATEGORY,
        factory: || Arc::new(Matrix::new()),
    });
}

/// python _format_tool_call
pub fn format_tool_call(tool: &Value) -> String {
    let Some(function) = tool.get("function").filter(|f| f.is_object()) else { return "🔧 Calling tool…".into() };
    let func_name = function.get("name").map(py_str).unwrap_or_else(|| "unknown".into());
    let raw_args = function.get("arguments").cloned().unwrap_or(json!("{}"));
    let args: Map<String, Value> = match raw_args {
        Value::String(s) => match ol_core::lenient_json::repair_loads(&s) {
            Value::Object(m) => m,
            _ => return "🔧 Calling tool…".into(),
        },
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let arg_strs: Vec<String> = args.iter().map(|(k, v)| format!("{}=\"{}\"", k, head(&py_str(v), 30))).collect();
    format!("🔧 {}({})", func_name, arg_strs.join(", "))
}

/// python `f"{tools}\n\n{body}" if tools and body else tools or body`
pub fn compose_visual(tools_text: &str, body_text: &str) -> String {
    if !tools_text.is_empty() && !body_text.is_empty() {
        format!("{}\n\n{}", tools_text, body_text)
    } else if !tools_text.is_empty() {
        tools_text.to_string()
    } else {
        body_text.to_string()
    }
}

/// python _announce text
pub fn announce_text(message: &str, kind: &str) -> String {
    let emoji = match kind {
        "error" => "🚨",
        "warning" => "⚠️",
        "status" => "ℹ️",
        "info" => "💬",
        _ => "🔔",
    };
    format!("{} **{}**: {}", emoji, kind.to_uppercase(), message)
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[derive(Default)]
struct MState {
    client: Mutex<Option<Arc<MxClient>>>,
    rooms: Mutex<Rooms>,
    ready: AtomicBool,
    running: AtomicBool,
    shutting_down: AtomicBool,
    startup_ts: Mutex<f64>,
    processed: Mutex<VecDeque<String>>,
    encrypted_noticed: Mutex<HashSet<String>>,
    stop: tokio::sync::Notify,
}

impl MState {
    fn client(&self) -> Option<Arc<MxClient>> {
        self.client.lock().clone()
    }

    fn is_duplicate_event(&self, event_id: &str) -> bool {
        if event_id.is_empty() {
            return false;
        }
        let mut p = self.processed.lock();
        if p.iter().any(|e| e == event_id) {
            return true;
        }
        p.push_back(event_id.to_string());
        while p.len() > MAX_PROCESSED_EVENTS {
            p.pop_front();
        }
        false
    }

    fn room_display_name(&self, room_id: &str, own_user: &str) -> String {
        self.rooms.lock().joined.get(room_id).map(|r| r.display_name(own_user)).unwrap_or_else(|| room_id.to_string())
    }

    fn room_is_encrypted(&self, room_id: &str) -> bool {
        self.rooms.lock().joined.get(room_id).map(|r| r.encrypted).unwrap_or(false)
    }

    fn room_is_dm(&self, room_id: &str) -> bool {
        self.rooms.lock().joined.get(room_id).map(|r| r.users() <= 2).unwrap_or(false)
    }

    /// startup grace period: ignore events older than startup - 5s
    fn is_old(&self, event: &Value) -> bool {
        let ts = event.get("origin_server_ts").and_then(|t| t.as_f64()).unwrap_or(0.0) / 1000.0;
        ts != 0.0 && ts < *self.startup_ts.lock() - 5.0
    }
}

pub struct Matrix {
    st: Arc<MState>,
}

impl Default for Matrix {
    fn default() -> Self {
        Self::new()
    }
}

impl Matrix {
    pub fn new() -> Self {
        Self { st: Arc::new(MState::default()) }
    }
}

fn store_path() -> anyhow::Result<PathBuf> {
    let p = PathBuf::from(ol_core::get_data_path(None)?).join("matrix_store");
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

fn credentials_path() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(ol_core::get_data_path(None)?).join("matrix_credentials.json"))
}

fn load_credentials(ch: &Channel) -> Option<Map<String, Value>> {
    let path = credentials_path().ok()?;
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())) {
        Ok(Value::Object(m)) => Some(m),
        Ok(_) => None,
        Err(e) => {
            ch.log("matrix", &format!("Failed to load credentials: {}", e));
            None
        }
    }
}

fn save_credentials(ch: &Channel, user_id: &str, device_id: &str, access_token: &str) {
    let creds = json!({"user_id": user_id, "device_id": device_id, "access_token": access_token});
    let Ok(path) = credentials_path() else { return };
    match std::fs::write(&path, ol_core::pyjson::dumps(&creds)) {
        Ok(_) => ch.log("matrix", &format!("Credentials saved to {}", path.display())),
        Err(e) => ch.log("matrix", &format!("Failed to save credentials: {}", e)),
    }
}

/// sync token persistence (nio's store_sync_tokens)
fn sync_token_file(store: &Path) -> PathBuf {
    store.join("sync_token.json")
}

fn load_sync_token(store: &Path, user_id: &str, device_id: &str) -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(sync_token_file(store)).ok()?).ok()?;
    if v.get("user_id").and_then(|u| u.as_str()) != Some(user_id) || v.get("device_id").and_then(|u| u.as_str()) != Some(device_id) {
        return None;
    }
    v.get("next_batch").and_then(|n| n.as_str()).map(|s| s.to_string())
}

fn save_sync_token(store: &Path, user_id: &str, device_id: &str, next_batch: &str) {
    let _ = std::fs::write(sync_token_file(store), json!({"user_id": user_id, "device_id": device_id, "next_batch": next_batch}).to_string());
}

struct Settings {
    homeserver: String,
    user_id: String,
    password: String,
    access_token: String,
    device_id: String,
    device_name: String,
}

fn read_settings(ch: &Channel) -> Settings {
    let get = |k: &str| ch.config.get_str(k).unwrap_or_default();
    Settings {
        homeserver: get("homeserver"),
        user_id: get("user_id"),
        password: get("password"),
        access_token: get("access_token"),
        device_id: get("device_id"),
        device_name: ch.config.get_str("device_name").unwrap_or_else(|| "Core Bot".into()),
    }
}

/// python _login. returns the logged-in client and whether a store was restored
async fn login(ch: &Channel, s: &Settings, store: &Path) -> Result<(MxClient, bool), String> {
    let mut client = MxClient::new(&s.homeserver, &s.user_id, &s.device_id);
    // Priority 1: config-provided access token
    if !s.access_token.is_empty() {
        ch.log("matrix", "Using config-provided access token…");
        client.restore_login(&s.user_id, &s.device_id, &s.access_token);
        return Ok((client, true));
    }
    // Priority 2: previously saved credentials
    if let Some(saved) = load_credentials(ch).filter(|c| c.get("access_token").map(truthy).unwrap_or(false)) {
        let get = |k: &str| saved.get(k).map(py_str).unwrap_or_default();
        ch.log("matrix", &format!("Restoring saved credentials (device: {})…", saved.get("device_id").map(py_str).unwrap_or_else(|| "None".into())));
        client.restore_login(&get("user_id"), &get("device_id"), &get("access_token"));
        return Ok((client, true));
    }
    // Priority 3: fresh password login (creates a new device)
    if store.exists() {
        ch.log("matrix", "Wiping old crypto store for clean key registration…");
        let _ = std::fs::remove_dir_all(store);
        let _ = std::fs::create_dir_all(store);
    }
    match client.login(&s.password, &s.device_name).await {
        Ok((user_id, device_id, token)) => {
            ch.log("matrix", &format!("Logged in, device_id={}", device_id));
            save_credentials(ch, &user_id, &device_id, &token);
            Ok((client, false))
        }
        Err(e) => Err(format!("Login failed: {}", e)),
    }
}

#[async_trait::async_trait]
impl ChannelBehavior for Matrix {
    async fn run(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let st = self.st.clone();
        let settings = read_settings(ch);
        let store = match store_path() {
            Ok(p) => p,
            Err(e) => {
                ch.log("matrix", &format!("Config error: {}", e));
                return Ok(());
            }
        };

        if settings.homeserver.is_empty() || settings.user_id.is_empty() {
            ch.log("matrix", "Missing homeserver or user_id – aborting.");
            return Ok(());
        }
        if settings.password.is_empty() && settings.access_token.is_empty() {
            let saved = load_credentials(ch);
            if !saved.map(|s| s.get("access_token").map(truthy).unwrap_or(false)).unwrap_or(false) {
                ch.log("matrix", "No password or access_token – aborting.");
                return Ok(());
            }
        }

        ch.log("matrix", "Initializing client…");
        ch.log("matrix", "Logging in…");
        let (client, restored) = match login(ch, &settings, &store).await {
            Ok(c) => c,
            Err(e) => {
                ch.log("matrix", &format!("Critical error: {}", e));
                return Ok(());
            }
        };
        let client = Arc::new(client);
        *st.client.lock() = Some(client.clone());

        let result = async {
            ch.log("matrix", "Running initial sync…");
            let mut since = if restored { load_sync_token(&store, &client.user_id, &client.device_id) } else { None };
            let resp = client.sync(since.as_deref(), SYNC_TIMEOUT_MS, true).await.map_err(|e| format!("Initial sync failed: {}", e.message))?;
            since = handle_sync(ch, &st, &client, &store, &resp).await.or(since);

            *st.startup_ts.lock() = now_secs();
            st.ready.store(true, Ordering::SeqCst);
            st.running.store(true, Ordering::SeqCst);

            announce(ch, &st, &format!("Matrix connected as {} (end-to-end encryption is not supported by the Rust port).", client.user_id), "status").await;

            main_loop(ch, &st, &client, &store, since).await;
            Ok::<(), String>(())
        }
        .await;
        if let Err(e) = result {
            ch.log("matrix", &format!("Critical error: {}", e));
        }
        st.ready.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn on_shutdown(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        ch.log("matrix", "Shutting down…");
        self.st.running.store(false, Ordering::SeqCst);
        self.st.shutting_down.store(true, Ordering::SeqCst);
        self.st.stop.notify_waiters();
        Ok(())
    }

    // python's Matrix channel defines no on_push: pushed messages are not shown in matrix

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

async fn main_loop(ch: &Arc<Channel>, st: &Arc<MState>, client: &Arc<MxClient>, store: &Path, mut since: Option<String>) {
    let mut backoff = 1u64;
    while st.running.load(Ordering::SeqCst) && !st.shutting_down.load(Ordering::SeqCst) {
        let resp = tokio::select! {
            r = client.sync(since.as_deref(), SYNC_TIMEOUT_MS, false) => r,
            _ = st.stop.notified() => break,
        };
        match resp {
            Ok(resp) => {
                if let Some(nb) = handle_sync(ch, st, client, store, &resp).await {
                    since = Some(nb);
                }
                backoff = 1;
            }
            Err(e) => {
                if e.status == 0 {
                    ch.log("matrix", &format!("Sync exception: {}", e.message));
                } else {
                    ch.log("matrix", &format!("Sync error: {}", e.message));
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

/// update room state from a sync response and run the event callbacks in order.
/// returns the new sync token
async fn handle_sync(ch: &Arc<Channel>, st: &Arc<MState>, client: &Arc<MxClient>, store: &Path, resp: &Value) -> Option<String> {
    let events = st.rooms.lock().apply_sync(resp, &client.user_id);
    let next_batch = resp.get("next_batch").and_then(|n| n.as_str()).map(|s| s.to_string());
    if let Some(nb) = &next_batch {
        save_sync_token(store, &client.user_id, &client.device_id, nb);
    }
    for ev in events {
        match ev {
            RoomEvent::Invite { room_id, sender } => on_invite(ch, client, &room_id, &sender).await,
            RoomEvent::Text { room_id, event } => on_room_message(ch, st, client, &room_id, &event).await,
            RoomEvent::Media { room_id, event } => on_room_message_media(ch, st, client, &room_id, &event),
            RoomEvent::Encrypted { room_id, event } => on_encrypted_event(ch, st, client, &room_id, &event).await,
        }
    }
    next_batch
}

async fn on_invite(ch: &Arc<Channel>, client: &Arc<MxClient>, room_id: &str, sender: &str) {
    let auto_join = std::env::var("MATRIX_AUTO_JOIN").unwrap_or_else(|_| "true".into()).to_lowercase() == "true";
    if !auto_join {
        ch.log("matrix", &format!("Ignoring invite to {} (auto-join off).", room_id));
        return;
    }
    ch.log("matrix", &format!("Invited to {} by {}, joining…", room_id, sender));
    match client.join(room_id).await {
        Ok(r) if r.get("room_id").is_some() => ch.log("matrix", &format!("Joined {}", room_id)),
        Ok(_) => {}
        // nio returns (doesn't raise) error responses; only transport errors were logged
        Err(e) if e.status == 0 => ch.log("matrix", &format!("Failed to join {}: {}", room_id, e.message)),
        Err(_) => {}
    }
}

/// common checks of the message callbacks: ready, not ours, not seen, not old
fn should_handle(st: &MState, client: &MxClient, event: &Value) -> bool {
    if !st.ready.load(Ordering::SeqCst) {
        return false;
    }
    if event.get("sender").and_then(|s| s.as_str()) == Some(client.user_id.as_str()) {
        return false;
    }
    if st.is_duplicate_event(event.get("event_id").and_then(|e| e.as_str()).unwrap_or("")) {
        return false;
    }
    !st.is_old(event)
}

async fn on_room_message(ch: &Arc<Channel>, st: &Arc<MState>, client: &Arc<MxClient>, room_id: &str, event: &Value) {
    if !should_handle(st, client, event) {
        return;
    }
    let body = event.get("content").and_then(|c| c.get("body")).and_then(|b| b.as_str()).unwrap_or("");
    if body.trim().is_empty() {
        return;
    }
    let enc_tag = if st.room_is_encrypted(room_id) { "enc" } else { "plain" };
    let dm_tag = if st.room_is_dm(room_id) { "DM" } else { "room" };
    let name = st.room_display_name(room_id, &client.user_id);
    let sender = event.get("sender").and_then(|s| s.as_str()).unwrap_or("");
    ch.log("matrix", &format!("[{}][{}] {} | {}: {}", dm_tag, enc_tag, name, sender, head(body, 80)));
    handle_message(ch, client, room_id, body.trim()).await;
}

fn on_room_message_media(ch: &Arc<Channel>, st: &Arc<MState>, client: &Arc<MxClient>, room_id: &str, event: &Value) {
    if !should_handle(st, client, event) {
        return;
    }
    let content = event.get("content").cloned().unwrap_or(Value::Null);
    let body = content.get("body").and_then(|b| b.as_str()).unwrap_or("");
    let url = content.get("url").and_then(|b| b.as_str()).unwrap_or("");
    ch.log("matrix", &format!("Media received in {}: {} ({})", st.room_display_name(room_id, &client.user_id), body, url));
}

async fn on_encrypted_event(ch: &Arc<Channel>, st: &Arc<MState>, client: &Arc<MxClient>, room_id: &str, event: &Value) {
    if !should_handle(st, client, event) {
        return;
    }
    let session_id = event.get("content").and_then(|c| c.get("session_id")).and_then(|s| s.as_str()).unwrap_or("");
    let tail: String = {
        let chars: Vec<char> = session_id.chars().collect();
        chars[chars.len().saturating_sub(8)..].iter().collect()
    };
    let name = st.room_display_name(room_id, &client.user_id);
    let sender = event.get("sender").and_then(|s| s.as_str()).unwrap_or("");
    ch.log("matrix", &format!("Undecryptable event in {} from {} (session …{})", name, sender, tail));
    if st.encrypted_noticed.lock().insert(room_id.to_string()) {
        ch.log("matrix", &format!("{}: encrypted rooms are not supported by the Rust port", name));
        send_room_message(ch, client, room_id, ENCRYPTED_ROOM_NOTICE).await;
    }
}

/// python _handle_message: stream the response, sending one message and editing it at
/// most every 2 seconds, with the typing indicator on until finished
async fn handle_message(ch: &Arc<Channel>, client: &Arc<MxClient>, room_id: &str, message: &str) {
    let _ = client.room_typing(room_id, true, 30000).await;

    let mut last_event_id: Option<String> = None;
    let mut last_edit_time: Option<Instant> = None;
    let mut tool_calls_display: Vec<String> = Vec::new();
    let mut response_parts = String::new();
    let mut shown_reasoning_text = false;

    let mut stream = ch.send_stream(json!(message), SendArgs::default());
    while let Some(token) = stream.next().await {
        let mut visual: Option<String> = None;
        let t_type = token.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let content = token.get("content").cloned().unwrap_or(json!(""));

        if t_type == "tool_calls" && truthy(&content) {
            let tools = match content {
                Value::Array(a) => a,
                other => vec![other],
            };
            for tool in &tools {
                tool_calls_display.push(format_tool_call(tool));
            }
        } else if t_type == "reasoning" {
            if !shown_reasoning_text {
                visual = Some("thinking..".into());
                shown_reasoning_text = true;
            } else {
                continue;
            }
        } else if t_type == "content" || t_type == "error" {
            response_parts.push_str(&py_str(&content));
        }

        let visual = visual.unwrap_or_else(|| compose_visual(&tool_calls_display.join("\n"), &response_parts));
        if visual.is_empty() {
            continue;
        }

        let now = Instant::now();
        match &last_event_id {
            None => {
                if let Some(id) = send_room_message(ch, client, room_id, &visual).await {
                    last_event_id = Some(id);
                }
                last_edit_time = Some(now);
            }
            Some(id) => {
                if last_edit_time.map(|t| now.duration_since(t) >= EDIT_INTERVAL).unwrap_or(true) {
                    edit_room_message(ch, client, room_id, id, &visual).await;
                    last_edit_time = Some(now);
                }
            }
        }
    }

    let final_text = compose_visual(&tool_calls_display.join("\n"), &response_parts);
    if !final_text.is_empty() {
        match &last_event_id {
            Some(id) => {
                edit_room_message(ch, client, room_id, id, &final_text).await;
            }
            None => {
                send_room_message(ch, client, room_id, &final_text).await;
            }
        }
    }

    let _ = client.room_typing(room_id, false, 30000).await;
}

/// python _announce: log, then post to every joined room
async fn announce(ch: &Arc<Channel>, st: &Arc<MState>, message: &str, kind: &str) {
    let text = announce_text(message, kind);
    ch.log("matrix", &format!("[{}] {}", kind, message));
    let Some(client) = st.client() else { return };
    let rooms: Vec<String> = st.rooms.lock().joined.keys().cloned().collect();
    for room_id in rooms {
        if let Err(e) = client.room_send(&room_id, "m.room.message", client::text_content(&text)).await {
            ch.log("matrix", &format!("Announce to {} failed: {}", room_id, e));
        }
    }
}

fn log_send_error(ch: &Channel, what: &str, room_id: &str, e: &MxError) {
    // nio returns error responses instead of raising; python only logged exceptions
    if e.status == 0 {
        ch.log("matrix", &format!("{} failed ({}): {}", what, room_id, e.message));
    }
}

async fn send_room_message(ch: &Arc<Channel>, client: &Arc<MxClient>, room_id: &str, text: &str) -> Option<String> {
    match client.room_send(room_id, "m.room.message", client::text_content(text)).await {
        Ok(id) if !id.is_empty() => Some(id),
        Ok(_) => None,
        Err(e) => {
            log_send_error(ch, "Send", room_id, &e);
            None
        }
    }
}

async fn edit_room_message(ch: &Arc<Channel>, client: &Arc<MxClient>, room_id: &str, event_id: &str, new_text: &str) {
    if let Err(e) = client.room_send(room_id, "m.room.message", client::edit_content(event_id, new_text)).await {
        log_send_error(ch, "Edit", room_id, &e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_format() {
        let tc = json!({"function": {"name": "notes_add", "arguments": "{\"title\": \"groceries\", \"text\": \"a very long text that goes over thirty characters\"}"}});
        assert_eq!(format_tool_call(&tc), "🔧 notes_add(title=\"groceries\", text=\"a very long text that goes ove\")");
        assert_eq!(format_tool_call(&json!("x")), "🔧 Calling tool…");
        assert_eq!(format_tool_call(&json!({"function": {"name": "t", "arguments": {"a": 1}}})), "🔧 t(a=\"1\")");
    }

    #[test]
    fn visuals() {
        assert_eq!(compose_visual("", ""), "");
        assert_eq!(compose_visual("t", ""), "t");
        assert_eq!(compose_visual("", "b"), "b");
        assert_eq!(compose_visual("t", "b"), "t\n\nb");
        assert_eq!(announce_text("hi", "status"), "ℹ️ **STATUS**: hi");
        assert_eq!(announce_text("hi", "weird"), "🔔 **WEIRD**: hi");
    }

    #[test]
    fn dedup_and_grace() {
        let st = MState::default();
        assert!(!st.is_duplicate_event("$a"));
        assert!(st.is_duplicate_event("$a"));
        assert!(!st.is_duplicate_event(""));
        assert!(!st.is_duplicate_event(""));
        for i in 0..1001 {
            st.is_duplicate_event(&format!("$e{}", i));
        }
        assert!(!st.is_duplicate_event("$a"));
        *st.startup_ts.lock() = 1000.0;
        assert!(st.is_old(&json!({"origin_server_ts": 990_000})));
        assert!(!st.is_old(&json!({"origin_server_ts": 996_000})));
        assert!(!st.is_old(&json!({})));
    }

    #[test]
    fn sync_token_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        save_sync_token(d.path(), "@a:x", "DEV", "s42");
        assert_eq!(load_sync_token(d.path(), "@a:x", "DEV"), Some("s42".into()));
        assert_eq!(load_sync_token(d.path(), "@a:x", "OTHER"), None);
    }
}
