//! Port of the `/ws` endpoint (channels/webui/channel.py `websocket_endpoint`), the
//! `WebSocketManager` class, the shared chat ops (`op_chat_switch/new/delete/rename`) and the
//! stream forwarding of `Webui.on_stream`.
//!
//! Message shapes are the python ones: server -> client `ready`, `stream_state`, `push`, `log`,
//! `token`, `turn_stream`, `user_message_added`, `user_message_confirmed`, `stream_complete`,
//! `sync`, `chat_switched`, `chat_metadata_updated`, `error`, `shutdown`.

use crate::server::Shared;
use crate::util::{py_int, py_type_name};
use anyhow::anyhow;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use indexmap::IndexMap;
use ol_core::channel::{Channel, Files, SendArgs, TokenStream};
use ol_core::pyjson::{py_str, truthy};
use ol_core::turns;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// the stream task that is currently producing a reply (python `active_stream_task`)
struct ActiveStream {
    /// consumed by the first cancel
    cancel: Option<oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

/// python `WebSocketManager`: the connected clients, the authoritative stream state and the one
/// running stream. broadcasts are queued on each client's outgoing channel, so they never block
pub struct WsManager {
    connections: Mutex<Vec<(u64, mpsc::UnboundedSender<String>)>>,
    next_connection: AtomicU64,
    stream_state: Mutex<&'static str>,
    active_stream: Mutex<Option<ActiveStream>>,
    active_chat_id: Mutex<Option<String>>,
    stream_index: AtomicI64,
    webui_ready: AtomicBool,
}

impl Default for WsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WsManager {
    pub fn new() -> Self {
        Self {
            connections: Mutex::new(Vec::new()),
            next_connection: AtomicU64::new(0),
            // python: the backend is the single source of truth for stream state
            stream_state: Mutex::new("idle"),
            active_stream: Mutex::new(None),
            active_chat_id: Mutex::new(None),
            stream_index: AtomicI64::new(-1),
            webui_ready: AtomicBool::new(false),
        }
    }

    /// python `broadcast`: clients whose socket is gone are dropped
    pub fn broadcast(&self, message: Value) {
        let text = message.to_string();
        self.connections.lock().retain(|(_, tx)| tx.send(text.clone()).is_ok());
    }

    /// python `set_stream_state`: deduped, so high-frequency token paths can call it freely
    pub fn set_stream_state(&self, state: &'static str) {
        let mut current = self.stream_state.lock();
        if *current == state {
            return;
        }
        *current = state;
        self.broadcast(json!({"type": "stream_state", "state": state}));
    }

    pub fn stream_state(&self) -> &'static str {
        *self.stream_state.lock()
    }

    fn add_connection(&self, tx: mpsc::UnboundedSender<String>) -> u64 {
        let id = self.next_connection.fetch_add(1, Ordering::SeqCst);
        self.connections.lock().push((id, tx));
        id
    }

    fn remove_connection(&self, id: u64) {
        self.connections.lock().retain(|(i, _)| *i != id);
    }

    pub fn webui_ready(&self) -> bool {
        self.webui_ready.load(Ordering::SeqCst)
    }

    /// python `send_ready_signal`
    pub fn send_ready_signal(&self) {
        self.webui_ready.store(true, Ordering::SeqCst);
    }

    pub fn set_stream_index(&self, index: i64) {
        self.stream_index.store(index, Ordering::SeqCst);
    }

    pub fn stream_index(&self) -> i64 {
        self.stream_index.load(Ordering::SeqCst)
    }

    pub fn set_active_chat_id(&self, id: String) {
        *self.active_chat_id.lock() = Some(id);
    }

    /// python: `if active_stream_task and not active_stream_task.done(): active_stream_task.cancel()`
    pub fn cancel_active_stream(&self) {
        let mut active = self.active_stream.lock();
        if let Some(stream) = active.as_mut() {
            if !stream.handle.is_finished() {
                if let Some(tx) = stream.cancel.take() {
                    let _ = tx.send(());
                }
            }
        }
    }

    fn set_active_stream(&self, stream: ActiveStream) {
        *self.active_stream.lock() = Some(stream);
    }
}

/// the `/ws` route. python refuses an unauthenticated client before accepting, which uvicorn
/// answers with an HTTP 403 (the handshake never completes)
pub async fn ws_route(State(app): State<Arc<Shared>>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    let Some(ch) = app.ch() else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    if ch.config.get_bool("require_login", false) && !crate::session::ws_authorized(&headers, &app.signer) {
        return StatusCode::FORBIDDEN.into_response();
    }
    upgrade.on_upgrade(move |socket| serve_connection(app, socket)).into_response()
}

/// how the read loop ends after a command: python's `return` / `break` close the endpoint
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Close,
}

async fn serve_connection(app: Arc<Shared>, socket: WebSocket) {
    let Some(ch) = app.ch() else { return };
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // python `connect`: register, announce ready when a chat is loaded, send the current stream state
    let id = app.ws.add_connection(tx.clone());
    if ch.chat.get("id").map(|v| truthy(&v)).unwrap_or(false) {
        let _ = tx.send(json!({"type": "ready"}).to_string());
    }
    let _ = tx.send(json!({"type": "stream_state", "state": app.ws.stream_state()}).to_string());
    drop(tx);
    let ready_app = app.clone();
    tokio::spawn(async move {
        // python `queue_ready_signal`: polls until the ready flag is set, then announces ready
        while !ready_app.ws.webui_ready() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        ready_app.ws.broadcast(json!({"type": "ready"}));
    });

    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    while let Some(frame) = stream.next().await {
        match frame {
            Ok(Message::Text(text)) => {
                if handle_command(&app, &ch, text.as_str()).await == Flow::Close {
                    break;
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Binary(_)) => {
                // python receive_text() on a binary frame fails and ends the connection
                ch.log(&ch.name, "WebSocket error: 'text'");
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    app.ws.remove_connection(id);
    writer.abort();
}

/// python `websocket_endpoint` read loop body: parse one client message and run its command
async fn handle_command(app: &Arc<Shared>, ch: &Arc<Channel>, text: &str) -> Flow {
    // python: json.JSONDecodeError is silently ignored
    let Ok(data) = serde_json::from_str::<Value>(text) else { return Flow::Continue };
    let Some(obj) = data.as_object() else {
        ch.log(&ch.name, &format!("WebSocket command error: '{}' object has no attribute 'get'", py_type_name(&data)));
        return Flow::Continue;
    };
    let msg_type = obj.get("type").cloned().unwrap_or(Value::Null);

    let outcome: anyhow::Result<Flow> = match msg_type.as_str() {
        Some("stop") => {
            let api = ch.manager().api();
            api.cancel().await;
            Ok(Flow::Continue)
        }
        Some("reload_messages") => {
            app.ws.broadcast(json!({"type": "sync"}));
            Ok(Flow::Continue)
        }
        Some("rename") => {
            let title = obj.get("title").cloned().unwrap_or(Value::Null);
            if truthy(&title) {
                op_chat_rename(app, ch, title, None).await.map(|_| Flow::Continue)
            } else {
                Ok(Flow::Continue)
            }
        }
        Some("switch_chat") => {
            let chat_id = obj.get("chat_id").cloned().unwrap_or(Value::Null);
            if truthy(&chat_id) && !op_chat_switch(app, ch, &py_str(&chat_id)).await {
                app.ws.broadcast(json!({"type": "error", "content": "Failed to load chat"}));
            }
            Ok(Flow::Continue)
        }
        Some("new_chat") => {
            op_chat_new(app, ch, "general").await;
            Ok(Flow::Continue)
        }
        Some("chat_delete") => {
            let chat_id = obj.get("chat_id").cloned().unwrap_or(Value::Null);
            if !truthy(&chat_id) {
                Ok(Flow::Close)
            } else {
                op_chat_delete(app, ch, &py_str(&chat_id)).await;
                Ok(Flow::Continue)
            }
        }
        Some("user_message") => cmd_user_message(app, ch, obj).await,
        Some("message_edit") => cmd_message_edit(app, ch, obj).await,
        Some("message_delete") => cmd_message_delete(app, ch, obj).await,
        Some("message_regenerate") => cmd_message_regenerate(app, ch, obj).await,
        _ => {
            ch.log(&ch.name, &format!("Unknown websocket command received: {}", py_str(&msg_type)));
            Ok(Flow::Continue)
        }
    };

    match outcome {
        Ok(flow) => flow,
        Err(e) => {
            ch.log(&ch.name, &format!("WebSocket command error: {}", ol_core::detail_error(&e)));
            Flow::Continue
        }
    }
}

/// python `index < 0` / `messages[index]` operand: an int (bool counts as int); anything else is
/// the python TypeError raised by the comparison
fn py_index(value: Option<&Value>) -> anyhow::Result<i64> {
    let v = value.cloned().unwrap_or(Value::Null);
    let int_like = matches!(v, Value::Number(_) | Value::Bool(_));
    match py_int(&v).filter(|_| int_like) {
        Some(i) => Ok(i),
        None => anyhow::bail!("'<' not supported between instances of '{}' and 'int'", py_type_name(&v)),
    }
}

/// python `messages.get(index)`
fn message_at(ch: &Channel, index: i64) -> anyhow::Result<Value> {
    let i = usize::try_from(index).map_err(|_| anyhow!("list index out of range"))?;
    ch.chat.messages_get_at(i).map_err(|_| anyhow!("list index out of range"))
}

/// python `_stream_task` file decoding: `{f["name"]: base64.b64decode(f["data"])}`
fn decode_files(files_data: &Value) -> anyhow::Result<Files> {
    let Value::Array(items) = files_data else {
        anyhow::bail!("'{}' object is not iterable", py_type_name(files_data));
    };
    let mut files: Files = IndexMap::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            anyhow::bail!("'{}' object is not subscriptable", py_type_name(item));
        };
        let name = obj.get("name").ok_or_else(|| anyhow!("'name'"))?;
        let data = obj.get("data").ok_or_else(|| anyhow!("'data'"))?;
        let encoded = data.as_str().ok_or_else(|| {
            anyhow!("argument should be a bytes-like object or ASCII string, not '{}'", py_type_name(data))
        })?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| anyhow!("Incorrect padding"))?;
        files.insert(py_str(name), bytes);
    }
    Ok(files)
}

async fn cmd_user_message(app: &Arc<Shared>, ch: &Arc<Channel>, obj: &serde_json::Map<String, Value>) -> anyhow::Result<Flow> {
    let text = obj.get("content").cloned().unwrap_or(Value::Null);
    let files_data = obj.get("files").cloned().unwrap_or(Value::Null);
    if !truthy(&text) && !truthy(&files_data) {
        return Ok(Flow::Close);
    }

    let mut files: Option<Files> = None;
    if truthy(&files_data) {
        // enforce enable_file_upload server-side too: hiding the button is only cosmetic otherwise
        if !ch.config.get_bool("enable_file_upload", true) {
            app.ws.broadcast(json!({"type": "error", "error": "File uploads are disabled"}));
            return Ok(Flow::Continue);
        }
        files = Some(decode_files(&files_data)?);
    }

    start_stream(app, ch, text, files).await;
    Ok(Flow::Continue)
}

async fn cmd_message_edit(app: &Arc<Shared>, ch: &Arc<Channel>, obj: &serde_json::Map<String, Value>) -> anyhow::Result<Flow> {
    let index = py_index(obj.get("index"))?;
    if index < 0 {
        return Ok(Flow::Close);
    }
    let mut message = message_at(ch, index)?;
    if !message.is_object() {
        anyhow::bail!("'{}' object does not support item assignment", py_type_name(&message));
    }
    message["content"] = obj.get("content").cloned().unwrap_or(Value::Null);
    ch.chat.messages_edit(index as usize, message).await;
    app.ws.broadcast(json!({"type": "sync"}));
    Ok(Flow::Continue)
}

async fn cmd_message_delete(app: &Arc<Shared>, ch: &Arc<Channel>, obj: &serde_json::Map<String, Value>) -> anyhow::Result<Flow> {
    let index = py_index(obj.get("index"))?;
    if index < 0 {
        return Ok(Flow::Close);
    }
    ch.chat.messages_delete_from(index as usize).await?;
    app.ws.broadcast(json!({"type": "sync"}));
    Ok(Flow::Continue)
}

async fn cmd_message_regenerate(app: &Arc<Shared>, ch: &Arc<Channel>, obj: &serde_json::Map<String, Value>) -> anyhow::Result<Flow> {
    let index = obj.get("index").cloned().unwrap_or(Value::Null);
    if index.is_null() {
        return Ok(Flow::Continue);
    }
    let cutoff = py_index(Some(&index))?;
    let last_user = ch.chat.get_last_message_with_role("user", Some(cutoff));
    if last_user == -1 {
        app.ws.broadcast(json!({
            "type": "error",
            "error": "Could not regenerate message (no preceding user message found)"
        }));
        return Ok(Flow::Close);
    }

    let user_message = message_at(ch, last_user)?;
    // delete_from removes everything AFTER the target, so the clamp keeps index 0 valid
    ch.chat.messages_delete_from(last_user.max(0) as usize).await?;
    app.ws.broadcast(json!({"type": "sync"}));
    let content = user_message.get("content").cloned().unwrap_or(Value::Null);
    start_stream(app, ch, content, None).await;
    Ok(Flow::Continue)
}

/// python `op_chat_switch`: loads a chat, cancels any stream, notifies all clients.
/// returns False only when the load itself failed
pub async fn op_chat_switch(app: &Arc<Shared>, ch: &Arc<Channel>, chat_id: &str) -> bool {
    app.ws.cancel_active_stream();
    let loaded = match ch.chat.load(chat_id).await {
        Ok(v) => v,
        Err(_) => return false,
    };
    if loaded {
        app.ws.set_active_chat_id(chat_id.to_string());
        app.ws.broadcast(json!({"type": "chat_switched", "id": chat_id}));
    }
    true
}

/// python `op_chat_new`: creates a chat, switches to it, notifies all clients
pub async fn op_chat_new(app: &Arc<Shared>, ch: &Arc<Channel>, category: &str) -> String {
    app.ws.cancel_active_stream();
    let new_id = ch.chat.new_chat(category, "New chat", None).await;
    app.ws.set_active_chat_id(new_id.clone());
    app.ws.broadcast(json!({"type": "chat_switched", "id": new_id}));
    new_id
}

/// python `op_chat_delete`: deletes a chat and points all clients at whatever is loaded now
pub async fn op_chat_delete(app: &Arc<Shared>, ch: &Arc<Channel>, chat_id: &str) {
    ch.chat.delete(chat_id).await;
    app.ws.broadcast(json!({"type": "chat_switched", "id": ch.chat.get("id").unwrap_or(Value::Null)}));
}

/// python `op_chat_rename`: renames a chat (the loaded one when no id is given), saves, notifies
pub async fn op_chat_rename(app: &Arc<Shared>, ch: &Arc<Channel>, title: Value, chat_id: Option<&str>) -> anyhow::Result<bool> {
    let tags = match chat_id {
        Some(id) => {
            let Some(index) = ch.chat.find_index(id) else { return Ok(false) };
            ch.chat.set("title", title.clone(), Some(index))?;
            ch.chat.data().get(index).and_then(|c| c.get("tags")).cloned()
        }
        None => {
            ch.chat.set("title", title.clone(), None)?;
            ch.chat.get("tags")
        }
    };
    // python `... or []`
    let tags = tags.filter(truthy).unwrap_or_else(|| json!([]));
    ch.chat.save_index();
    app.ws.broadcast(json!({"type": "chat_metadata_updated", "title": title, "tags": tags}));
    Ok(true)
}

/// python `WebSocketManager.start_stream`: cancels the running stream, announces the new one, and
/// runs `send_stream` as a background task. the task always ends with stream_complete + idle, even
/// when cancelled (python's `finally`)
pub async fn start_stream(app: &Arc<Shared>, ch: &Arc<Channel>, message: Value, files: Option<Files>) {
    app.ws.cancel_active_stream();
    app.ws.set_stream_state("message_sending");

    let next_index = ch.chat.messages_get().len() as i64;
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let (app2, ch2) = (app.clone(), ch.clone());
    let handle = tokio::spawn(async move {
        app2.ws.set_stream_index(next_index);
        let args = SendArgs {
            files,
            // python: admin commands are gated by enable_admin_settings
            commands_authorized: ch2.config.get_bool("enable_admin_settings", true),
            ..Default::default()
        };
        let stream = ch2.send_stream(message, args);
        tokio::select! {
            _ = ch2.push_stream(stream) => {}
            _ = cancel_rx => {}
        }
        // broadcast first, state second: stream_complete does UI handoff work that still reads the state
        app2.ws.broadcast(json!({"type": "stream_complete"}));
        app2.ws.set_stream_state("idle");
    });
    app.ws.set_active_stream(ActiveStream { cancel: Some(cancel_tx), handle });
}

/// python `Webui.on_stream`: turns the grouped stream into the client messages (token/turn/state)
pub async fn forward_stream(app: &Arc<Shared>, stream: TokenStream) {
    let index = app.ws.stream_index();
    let mut partials = turns::group_stream(stream);
    let mut user_message_confirmed = false;

    while let Some(partial) = partials.next().await {
        let content = partial.get("content").cloned().unwrap_or(Value::Null);
        match partial.get("type").and_then(Value::as_str) {
            Some("token") => {
                let token_type = content.get("type").and_then(Value::as_str).map(str::to_string);
                match token_type.as_deref() {
                    Some("user_message") => {
                        app.ws.set_stream_state("message_sending");
                        let mut payload = content.clone();
                        payload["index"] = json!(index);
                        app.ws.broadcast(json!({"type": "user_message_added", "message": payload}));
                    }
                    // an error ends the turn: straight back to idle, then a forced chat reload
                    Some("error") => {
                        app.ws.set_stream_state("idle");
                        app.ws.broadcast(json!({"type": "user_message_confirmed", "index": index}));
                        app.ws.broadcast(json!({"type": "token", "content": content}));
                        app.ws.broadcast(json!({"type": "sync"}));
                        return;
                    }
                    other => {
                        if !user_message_confirmed {
                            user_message_confirmed = true;
                            app.ws.broadcast(json!({"type": "user_message_confirmed", "index": index}));
                            app.ws.set_stream_state("message_received");
                        }
                        match other {
                            // prompt eval between tool rounds keeps the processing_tools state
                            Some("prompt_progress") => {
                                if app.ws.stream_state() != "processing_tools" {
                                    app.ws.set_stream_state("processing");
                                }
                            }
                            Some("model_load_progress") => app.ws.set_stream_state("processing"),
                            Some("tool") => app.ws.set_stream_state("processing_tools"),
                            Some("tool_call_delta") | Some("tool_calls") => app.ws.set_stream_state("calling_tools"),
                            _ => {}
                        }
                        app.ws.broadcast(json!({"type": "token", "content": content}));
                    }
                }
            }
            Some("turn") => {
                match content.get("type").and_then(Value::as_str) {
                    Some("reasoning") => app.ws.set_stream_state("thinking"),
                    Some("content") => app.ws.set_stream_state("streaming"),
                    _ => {}
                }
                app.ws.broadcast(json!({"type": "turn_stream", "turn": content}));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(text) = rx.try_recv() {
            out.push(serde_json::from_str(&text).unwrap());
        }
        out
    }

    #[test]
    fn stream_state_is_deduped_and_broadcast() {
        let ws = WsManager::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        ws.add_connection(tx);
        ws.set_stream_state("idle");
        assert!(drain(&mut rx).is_empty());
        ws.set_stream_state("thinking");
        ws.set_stream_state("thinking");
        ws.set_stream_state("streaming");
        assert_eq!(
            drain(&mut rx),
            vec![json!({"type": "stream_state", "state": "thinking"}), json!({"type": "stream_state", "state": "streaming"})]
        );
        assert_eq!(ws.stream_state(), "streaming");
    }

    #[test]
    fn broadcast_drops_closed_clients() {
        let ws = WsManager::new();
        let (tx_a, rx_a) = mpsc::unbounded_channel::<String>();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel::<String>();
        ws.add_connection(tx_a);
        ws.add_connection(tx_b);
        drop(rx_a);
        ws.broadcast(json!({"type": "sync"}));
        assert_eq!(drain(&mut rx_b), vec![json!({"type": "sync"})]);
        assert_eq!(ws.connections.lock().len(), 1);
    }

    #[test]
    fn python_index_semantics() {
        assert_eq!(py_index(Some(&json!(2))).unwrap(), 2);
        assert_eq!(py_index(Some(&json!(true))).unwrap(), 1);
        assert_eq!(py_index(Some(&json!(-1))).unwrap(), -1);
        assert_eq!(
            py_index(None).unwrap_err().to_string(),
            "'<' not supported between instances of 'NoneType' and 'int'"
        );
        assert_eq!(
            py_index(Some(&json!("3"))).unwrap_err().to_string(),
            "'<' not supported between instances of 'str' and 'int'"
        );
    }

    #[test]
    fn file_upload_decoding() {
        let files = decode_files(&json!([{"name": "a.txt", "data": "aGk="}, {"name": "b", "data": "AQI="}])).unwrap();
        assert_eq!(files.get("a.txt").unwrap(), b"hi");
        assert_eq!(files.get("b").unwrap(), &vec![1u8, 2u8]);
        assert_eq!(decode_files(&json!([{"data": "aGk="}])).unwrap_err().to_string(), "'name'");
    }

}
