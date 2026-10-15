//! Port of channels/telegram/channel.py: talk to your AI over Telegram.
//!
//! python-telegram-bot is replaced by a thin Bot API client ([`api::TgApi`]) with a
//! getUpdates long-polling loop. Behaviour mirrors the python channel: the first chat
//! that talks to the bot (or sends /start) becomes the authorized chat (persisted in
//! `data/telegram_chat_id`), commands run immediately (so `/stop` can interrupt), normal
//! messages are queued and answered one at a time, responses are streamed by editing a
//! "processing your request..." message every 1.5s, starting a new message every
//! ~1900 characters.

pub mod api;

#[path = "../../../../generated/channels/telegram.rs"]
mod meta;

use crate::util::{clen, head};
use api::{TgApi, TgError};
use futures::StreamExt;
use ol_core::channel::{Channel, ChannelBehavior, SendArgs, TextFormatOpts};
use ol_core::pyjson::py_str;
use ol_core::registry::{ChannelDescriptor, Registry};
use ol_core::storage::StorageText;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// python `max_length` of _send_chunked_message and the `[:4000]` edit truncation
pub const MAX_LENGTH: usize = 4000;
/// chunk size passed to format_stream_for_text
pub const STREAM_CHUNK_SIZE: usize = 1900;
/// seconds between streaming edits
pub const EDIT_INTERVAL: Duration = Duration::from_millis(1500);
/// seconds between typing indicators
pub const TYPING_INTERVAL: Duration = Duration::from_secs(3);
/// getUpdates long-poll timeout (python-telegram-bot default)
const POLL_TIMEOUT: u64 = 10;

pub fn register(r: &mut Registry) {
    r.add_channel(ChannelDescriptor {
        name: "telegram",
        class_name: meta::CLASS_NAME,
        doc: meta::DOC,
        settings: meta::settings,
        unsafe_: meta::UNSAFE,
        category: meta::CATEGORY,
        factory: || Arc::new(Telegram::new()),
    });
}

/// split a long message like python's _send_chunked_message: at the last newline
/// (or space) before `max_length`, stripping whitespace around the cut
pub fn split_message(text: &str, max_length: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![];
    }
    if clen(text) <= max_length {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut rest: Vec<char> = text.chars().collect();
    while !rest.is_empty() {
        if rest.len() <= max_length {
            chunks.push(rest.iter().collect());
            break;
        }
        let window = &rest[..max_length];
        let split_idx = window
            .iter()
            .rposition(|c| *c == '\n')
            .or_else(|| window.iter().rposition(|c| *c == ' '))
            .unwrap_or(max_length);
        let chunk: String = rest[..split_idx].iter().collect();
        chunks.push(chunk.trim().to_string());
        let remaining: String = rest[split_idx..].iter().collect();
        rest = remaining.trim().chars().collect();
    }
    chunks.into_iter().filter(|c| !c.is_empty()).collect()
}

/// does this message trigger python-telegram-bot's CommandHandler("start")?
pub fn is_start_command(message: &Value, bot_username: &str) -> bool {
    let text = message.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let command: String = match message.get("entities").and_then(|e| e.as_array()) {
        Some(entities) => {
            let Some(first) = entities.first() else { return false };
            if first.get("type").and_then(|t| t.as_str()) != Some("bot_command") || first.get("offset").and_then(|o| o.as_i64()) != Some(0) {
                return false;
            }
            // entity lengths are utf-16 units; commands are ascii
            let len = first.get("length").and_then(|l| l.as_u64()).unwrap_or(0) as usize;
            text.encode_utf16().take(len).collect::<Vec<u16>>().iter().map(|u| char::from_u32(*u as u32).unwrap_or(' ')).collect()
        }
        // no entity info (should not happen with real telegram): parse the text
        None => text.split_whitespace().next().unwrap_or("").to_string(),
    };
    let Some(cmd) = command.strip_prefix('/') else { return false };
    let mut parts = cmd.splitn(2, '@');
    let name = parts.next().unwrap_or("");
    let target = parts.next();
    name.to_lowercase() == "start" && target.map(|t| t.to_lowercase() == bot_username.to_lowercase()).unwrap_or(true)
}

#[derive(Default)]
struct State {
    api: Mutex<Option<TgApi>>,
    authorized_chat_id: Mutex<Option<i64>>,
    auth_storage: Mutex<Option<StorageText>>,
    bot_username: Mutex<String>,
    running: AtomicBool,
    shutting_down: AtomicBool,
    stop: tokio::sync::Notify,
}

impl State {
    fn api(&self) -> Option<TgApi> {
        self.api.lock().clone()
    }

    fn authorize(&self, chat_id: i64) {
        *self.authorized_chat_id.lock() = Some(chat_id);
        if let Some(s) = self.auth_storage.lock().as_mut() {
            s.set(&chat_id.to_string());
        }
    }
}

pub struct Telegram {
    api_base: String,
    st: Arc<State>,
}

impl Default for Telegram {
    fn default() -> Self {
        Self::new()
    }
}

impl Telegram {
    pub fn new() -> Self {
        Self::with_api_base(api::DEFAULT_API_BASE)
    }

    /// point the bot at a different Bot API server (local bot api server, tests)
    pub fn with_api_base(base: &str) -> Self {
        Self { api_base: base.to_string(), st: Arc::new(State::default()) }
    }

    pub fn authorized_chat_id(&self) -> Option<i64> {
        *self.st.authorized_chat_id.lock()
    }
}

/// a queued (non-command) message
struct Incoming {
    chat_id: i64,
    text: String,
}

#[async_trait::async_trait]
impl ChannelBehavior for Telegram {
    async fn run(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let st = self.st.clone();
        let token = std::env::var("TELEGRAM_BOT_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .or_else(|| ch.config.get1("token").filter(|v| !v.is_null()).map(|v| py_str(&v)))
            .unwrap_or_default();

        // the authorized chat id is stored on disk
        match StorageText::new("telegram_chat_id", None) {
            Ok(mut storage) => {
                let stored = storage.get();
                if !stored.trim().is_empty() {
                    match stored.trim().parse::<i64>() {
                        Ok(id) => {
                            *st.authorized_chat_id.lock() = Some(id);
                            ch.log("telegram", &format!("Restored authorized chat ID: {}", id));
                        }
                        Err(_) => ch.log("telegram", "Failed to parse stored chat ID."),
                    }
                }
                *st.auth_storage.lock() = Some(storage);
            }
            Err(e) => ch.log("telegram", &format!("Critical Error: {}", e)),
        }

        if token.is_empty() {
            ch.push(json!("Telegram channel failed: No API token provided."), true).await;
            return Ok(());
        }

        let api = TgApi::new(&self.api_base, &token);
        // Application.initialize() + updater.start_polling(drop_pending_updates=True)
        let init = async {
            let me = api.get_me().await.map_err(|e| match e {
                TgError::InvalidToken => format!("The token `{}` was rejected by the server.", token),
                other => other.to_string(),
            })?;
            *st.bot_username.lock() = me.get("username").map(py_str).unwrap_or_default();
            api.delete_webhook(true).await.map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        };
        if let Err(e) = init.await {
            ch.log("telegram", &format!("Critical Error: {}", e));
            return Ok(());
        }
        *st.api.lock() = Some(api.clone());
        st.running.store(true, Ordering::SeqCst);

        // sequential worker for normal messages
        let (queue_tx, queue_rx) = tokio::sync::mpsc::unbounded_channel::<Incoming>();
        let worker = tokio::spawn(process_queue_worker(ch.clone(), st.clone(), queue_rx));

        if ch.config.get_bool("announce_startup", false) {
            ch.push(json!("Telegram channel connected."), true).await;
        }

        let mut offset: Option<i64> = None;
        let mut backoff = 1u64;
        while st.running.load(Ordering::SeqCst) && !st.shutting_down.load(Ordering::SeqCst) {
            let updates = tokio::select! {
                r = api.get_updates(offset, POLL_TIMEOUT) => r,
                _ = st.stop.notified() => break,
            };
            match updates {
                Ok(updates) => {
                    backoff = 1;
                    for update in updates {
                        if let Some(id) = update.get("update_id").and_then(|v| v.as_i64()) {
                            offset = Some(id + 1);
                        }
                        handle_update(ch, &st, &api, &queue_tx, &update).await;
                    }
                }
                Err(TgError::InvalidToken) => {
                    ch.log("telegram", &format!("Critical Error: The token `{}` was rejected by the server.", token));
                    break;
                }
                Err(e) => {
                    let wait = if let TgError::RetryAfter(s) = e { s } else { backoff };
                    ch.log("telegram", &format!("Error while polling for updates: {}", e));
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    backoff = (backoff * 2).min(30);
                }
            }
        }
        worker.abort();
        st.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn on_shutdown(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        // python called a nonexistent self.announce() here; the intent (tell the
        // authorized chat that the bot is going down) is implemented directly
        if ch.config.get_bool("announce_shutdown", false) {
            let chat = *self.st.authorized_chat_id.lock();
            if let (Some(chat_id), Some(api)) = (chat, self.st.api()) {
                let _ = tokio::time::timeout(Duration::from_secs(5), send_chunked_message(ch, &api, chat_id, "Shutting down Telegram channel...")).await;
            }
        }
        self.st.running.store(false, Ordering::SeqCst);
        self.st.shutting_down.store(true, Ordering::SeqCst);
        self.st.stop.notify_waiters();
        Ok(())
    }

    async fn on_push(&self, ch: &Arc<Channel>, message: Value) -> anyhow::Result<()> {
        let content = message.get("content").map(py_str).unwrap_or_else(|| "None".into());
        ch.log("telegram", &content);
        let chat = *self.st.authorized_chat_id.lock();
        if let (Some(chat_id), Some(api)) = (chat, self.st.api()) {
            let ch = ch.clone();
            tokio::spawn(async move { send_chunked_message(&ch, &api, chat_id, &content).await });
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// route one getUpdates update (python's handler dispatch: CommandHandler("start"),
/// then MessageHandler(filters.TEXT))
async fn handle_update(ch: &Arc<Channel>, st: &Arc<State>, api: &TgApi, queue: &tokio::sync::mpsc::UnboundedSender<Incoming>, update: &Value) {
    // only new messages are handled (python's handlers read update.message)
    let Some(message) = update.get("message") else { return };
    let Some(text) = message.get("text").and_then(|t| t.as_str()).filter(|t| !t.is_empty()) else { return };
    let Some(chat_id) = message.get("chat").and_then(|c| c.get("id")).and_then(|i| i.as_i64()) else { return };

    let username = st.bot_username.lock().clone();
    if is_start_command(message, &username) {
        tg_start(ch, st, api, message, chat_id).await;
        return;
    }

    // _tg_message
    let authorized = *st.authorized_chat_id.lock();
    if let Some(a) = authorized {
        if a != chat_id {
            return;
        }
    } else {
        st.authorize(chat_id);
    }

    let text = text.trim().to_string();
    let prefix = crate::util::cmd_prefix();
    if text.starts_with(&prefix) {
        // commands run immediately so that /stop can interrupt the queue worker
        let ch = ch.clone();
        let st = st.clone();
        tokio::spawn(async move {
            if let Err(e) = process_stream(&ch, &st, chat_id, &text).await {
                ch.log("telegram", &format!("Error processing stream: {}", e));
            }
        });
    } else {
        let _ = queue.send(Incoming { chat_id, text });
    }
}

async fn tg_start(ch: &Arc<Channel>, st: &Arc<State>, api: &TgApi, message: &Value, chat_id: i64) {
    // reply_text quotes the message in group chats only
    let reply_to = if message.get("chat").and_then(|c| c.get("type")).and_then(|t| t.as_str()) != Some("private") {
        message.get("message_id").and_then(|m| m.as_i64())
    } else {
        None
    };
    let authorized = *st.authorized_chat_id.lock();
    match authorized {
        None => {
            st.authorize(chat_id);
            if let Err(e) = api.send_message(chat_id, "✅ Session started.\n", None, reply_to).await {
                ch.log("telegram", &format!("Failed to send message: {}", e));
            }
            ch.log("telegram", &format!("Authorized chat ID: {}", chat_id));
        }
        Some(a) if a != chat_id => {
            let _ = api.send_message(chat_id, "⚠️ This bot is already in use.", None, reply_to).await;
        }
        _ => {}
    }
}

async fn process_queue_worker(ch: Arc<Channel>, st: Arc<State>, mut rx: tokio::sync::mpsc::UnboundedReceiver<Incoming>) {
    while st.running.load(Ordering::SeqCst) && !st.shutting_down.load(Ordering::SeqCst) {
        let Some(msg) = rx.recv().await else { break };
        let Some(api) = st.api() else { continue };
        let typing = tokio::spawn(keep_typing(ch.clone(), api.clone(), msg.chat_id));
        let result: anyhow::Result<()> = async {
            if ch.config.get_bool("use_message_streaming", true) {
                process_stream(&ch, &st, msg.chat_id, &msg.text).await
            } else {
                if let Some(response) = ch.send(json!(msg.text), SendArgs::authorized()).await {
                    let content = response.get("content").map(py_str).unwrap_or_default();
                    if !content.is_empty() {
                        send_chunked_message(&ch, &api, msg.chat_id, &content).await;
                    }
                }
                Ok(())
            }
        }
        .await;
        if let Err(e) = result {
            ch.log("telegram", &format!("Error in queue worker processing: {}", e));
        }
        typing.abort();
    }
}

struct StreamState {
    message_id: Option<i64>,
    full_content: String,
    /// last text successfully sent (skips edits telegram would reject as "not modified")
    shown: String,
}

async fn edit_current(api: &TgApi, chat_id: i64, state: &mut StreamState) {
    let Some(mid) = state.message_id else { return };
    let text = head(&state.full_content, MAX_LENGTH);
    if text == state.shown {
        return;
    }
    if api.edit_message_text(chat_id, mid, &text).await.is_ok() {
        state.shown = text;
    }
}

/// stream the AI response into telegram by editing messages
async fn process_stream(ch: &Arc<Channel>, st: &Arc<State>, chat_id: i64, user_msg: &str) -> anyhow::Result<()> {
    let Some(api) = st.api() else { return Ok(()) };
    let typing = tokio::spawn(keep_typing(ch.clone(), api.clone(), chat_id));
    let initial = match api.send_message(chat_id, "processing your request...", None, None).await {
        Ok(id) => id,
        Err(e) => {
            typing.abort();
            return Err(e.into());
        }
    };
    let state = Arc::new(tokio::sync::Mutex::new(StreamState { message_id: Some(initial), full_content: String::new(), shown: "processing your request...".into() }));

    let editor = {
        let state = state.clone();
        let api = api.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(EDIT_INTERVAL).await;
                let mut s = state.lock().await;
                if s.message_id.is_some() && !s.full_content.is_empty() {
                    edit_current(&api, chat_id, &mut s).await;
                }
            }
        })
    };

    let result: Result<(), TgError> = async {
        let opts = TextFormatOpts { chunk_size: Some(STREAM_CHUNK_SIZE), use_markdown: false, strings: None, show_indicators: true };
        let mut stream = ch.format_stream_for_text(ch.send_stream(json!(user_msg), SendArgs::authorized()), opts);
        while let Some(token) = stream.next().await {
            match token.get("type").and_then(|t| t.as_str()) {
                Some("new_chunk") => {
                    let mut s = state.lock().await;
                    edit_current(&api, chat_id, &mut s).await;
                    let id = api.send_message(chat_id, "...", None, None).await?;
                    s.message_id = Some(id);
                    s.full_content.clear();
                    s.shown = "...".into();
                }
                Some("formatted") => {
                    if let Some(c) = token.get("content").and_then(|c| c.as_str()).filter(|c| !c.is_empty()) {
                        state.lock().await.full_content.push_str(c);
                    }
                }
                _ => {}
            }
        }
        let mut s = state.lock().await;
        if s.message_id.is_some() {
            edit_current(&api, chat_id, &mut s).await;
        } else if !s.full_content.is_empty() {
            let _ = api.send_message(chat_id, &head(&s.full_content, MAX_LENGTH), None, None).await;
        }
        Ok(())
    }
    .await;

    if let Err(e) = result {
        ch.log("telegram", &format!("Error processing stream: {}", e));
        let _ = api.send_message(chat_id, &format!("❌ Error: {}", e), None, None).await;
    }
    editor.abort();
    typing.abort();
    Ok(())
}

async fn keep_typing(ch: Arc<Channel>, api: TgApi, chat_id: i64) {
    loop {
        if let Err(e) = api.send_chat_action(chat_id, "typing").await {
            ch.log("telegram", &format!("Typing indicator error: {}", e));
            return;
        }
        tokio::time::sleep(TYPING_INTERVAL).await;
    }
}

/// send a message, split into chunks if it's too long. tries Markdown first and falls
/// back to plain text
async fn send_chunked_message(ch: &Arc<Channel>, api: &TgApi, chat_id: i64, text: &str) {
    if text.is_empty() {
        return;
    }
    let single = clen(text) <= MAX_LENGTH;
    for chunk in split_message(text, MAX_LENGTH) {
        if api.send_message(chat_id, &chunk, Some("Markdown"), None).await.is_err() {
            if let Err(e) = api.send_message(chat_id, &chunk, None, None).await {
                if single {
                    ch.log("telegram", &format!("Failed to send message: {}", e));
                } else {
                    ch.log("telegram", &format!("Failed to send chunk: {}", e));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_short() {
        assert_eq!(split_message("hello", 10), vec!["hello"]);
        assert!(split_message("", 10).is_empty());
    }

    #[test]
    fn split_at_newline_then_space_then_hard() {
        assert_eq!(split_message("aaaa\nbbbb cccc", 10), vec!["aaaa", "bbbb cccc"]);
        assert_eq!(split_message("aaaa bbbb cccc", 10), vec!["aaaa bbbb", "cccc"]);
        assert_eq!(split_message("abcdefghijklmnop", 10), vec!["abcdefghij", "klmnop"]);
        // python: rfind searches text[0:max_length], so a newline exactly at max_length is not used
        assert_eq!(split_message("abcdefghij\nk", 10), vec!["abcdefghij", "k"]);
    }

    #[test]
    fn split_unicode_counts_code_points() {
        let s = "é".repeat(15);
        let parts = split_message(&s, 10);
        assert_eq!(parts, vec!["é".repeat(10), "é".repeat(5)]);
    }

    #[test]
    fn split_long_text_round_trip() {
        let text: String = (0..2000).map(|i| format!("word{} ", i)).collect();
        let parts = split_message(&text, MAX_LENGTH);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| clen(p) <= MAX_LENGTH));
        assert_eq!(parts.join(" "), text.trim());
    }

    #[test]
    fn start_command() {
        let m = json!({"text": "/start", "entities": [{"type": "bot_command", "offset": 0, "length": 6}]});
        assert!(is_start_command(&m, "lumabot"));
        let m = json!({"text": "/start@LumaBot hi", "entities": [{"type": "bot_command", "offset": 0, "length": 14}]});
        assert!(is_start_command(&m, "lumabot"));
        let m = json!({"text": "/start@other", "entities": [{"type": "bot_command", "offset": 0, "length": 12}]});
        assert!(!is_start_command(&m, "lumabot"));
        let m = json!({"text": "/help", "entities": [{"type": "bot_command", "offset": 0, "length": 5}]});
        assert!(!is_start_command(&m, "lumabot"));
        let m = json!({"text": "/startx", "entities": [{"type": "bot_command", "offset": 0, "length": 7}]});
        assert!(!is_start_command(&m, "lumabot"));
        let m = json!({"text": "say /start", "entities": [{"type": "bot_command", "offset": 4, "length": 6}]});
        assert!(!is_start_command(&m, "lumabot"));
        assert!(is_start_command(&json!({"text": "/start"}), "x"));
    }
}
