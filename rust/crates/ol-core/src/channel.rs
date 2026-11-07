//! Port of core/channel.py: the channel base class.
//!
//! Python subclasses `core.channel.Channel` and overrides hooks. In Rust the shared
//! state and logic live in [`Channel`]; channel-specific behaviour (servers, bots,
//! terminals) implements [`ChannelBehavior`], whose hooks receive the owning
//! `Arc<Channel>`.

use crate::api::{is_cancelled_token, SendOpts};
pub use crate::api::TokenStream;
use crate::chat::Chat;
use crate::config::{self, ConfigView};
use crate::context::{Context, GetOpts};
use crate::manager::Manager;
use crate::module::UserMessageHook;
use crate::pyjson::{self, py_str, truthy};
use crate::tool_loader::ToolLoader;
use async_trait::async_trait;
use base64::Engine;
use futures::StreamExt;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Weak};

/// overridable channel hooks (the methods python channel subclasses override)
#[async_trait]
pub trait ChannelBehavior: Send + Sync + 'static {
    /// called when the entire framework has fully initialized
    async fn on_ready(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let _ = ch;
        Ok(())
    }
    /// the channel's main loop (servers, bots, terminal loops)
    async fn run(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let _ = ch;
        Ok(())
    }
    /// immediately display a pushed message (announcements, scheduler, ...)
    async fn on_push(&self, ch: &Arc<Channel>, message: Value) -> anyhow::Result<()> {
        let _ = (ch, message);
        Ok(())
    }
    /// consume a pushed stream (render its tokens). the base implementation, like
    /// python's, does nothing (the stream is never iterated).
    async fn on_stream(&self, ch: &Arc<Channel>, stream: TokenStream) -> anyhow::Result<()> {
        let _ = (ch, stream);
        Ok(())
    }
    /// display logs broadcast by manager.log()
    fn on_log(&self, ch: &Arc<Channel>, category: &str, message: &str) {
        let _ = (ch, category, message);
    }
    async fn on_shutdown(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let _ = ch;
        Ok(())
    }
    fn as_any(&self) -> &dyn std::any::Any;
}

/// files attached to a user message: filename -> bytes (python dict of file data)
pub type Files = IndexMap<String, Vec<u8>>;

/// options for send()/send_stream()
#[derive(Clone, Default)]
pub struct SendArgs {
    pub files: Option<Files>,
    pub commands_authorized: bool,
    pub add_to_context: Option<bool>,
    pub api: SendOpts,
}

impl SendArgs {
    pub fn authorized() -> Self {
        Self { commands_authorized: true, ..Default::default() }
    }
    fn add_to_context(&self) -> bool {
        self.add_to_context.unwrap_or(true)
    }
}

enum Preprocessed {
    CmdResponse(String),
    Blank,
    ModuleIntercept,
    Error(String),
    Ready { user_message: Value, context: Vec<Value> },
}

/// guard that re-opens the push gate when a send ends (python's `finally: _chain_idle.set()`)
struct ChainIdleGuard(Arc<Channel>);
impl Drop for ChainIdleGuard {
    fn drop(&mut self) {
        let _ = self.0.chain_idle.send(true);
    }
}

pub struct Channel {
    pub name: String,
    pub is_user_channel: bool,
    manager: Weak<Manager>,
    pub config: ConfigView,
    pub chat: Chat,
    pub context: Context,
    pub tool_loader: ToolLoader,
    agentic_loop_start: AtomicI64,
    push_tx: tokio::sync::mpsc::UnboundedSender<(Value, bool)>,
    push_rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<(Value, bool)>>>,
    chain_idle: tokio::sync::watch::Sender<bool>,
    shutting_down: AtomicBool,
    queue_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    tool_state: Mutex<(Option<String>, String, Map<String, Value>)>,
    pub behavior: Arc<dyn ChannelBehavior>,
}

impl Channel {
    pub fn new(manager: &Arc<Manager>, name: &str, is_user_channel: bool, behavior: Arc<dyn ChannelBehavior>) -> anyhow::Result<Arc<Self>> {
        let chat = Chat::new(name)?;
        let (push_tx, push_rx) = tokio::sync::mpsc::unbounded_channel();
        let (chain_idle, _) = tokio::sync::watch::channel(true);
        let section = if is_user_channel { "user_channels" } else { "channels" };
        let ch = Arc::new_cyclic(|w: &Weak<Channel>| {
            chat.set_channel(w.clone());
            Channel {
                name: name.to_string(),
                is_user_channel,
                manager: Arc::downgrade(manager),
                config: ConfigView::new(&[section, "settings", name]),
                chat,
                context: Context::new(w.clone()),
                tool_loader: ToolLoader::new(w.clone()),
                agentic_loop_start: AtomicI64::new(-1),
                push_tx,
                push_rx: Mutex::new(Some(push_rx)),
                chain_idle,
                shutting_down: AtomicBool::new(false),
                queue_task: Mutex::new(None),
                tool_state: Mutex::new((None, String::new(), Map::new())),
                behavior,
            }
        });
        ch.tool_loader.register_meta_tools();
        Ok(ch)
    }

    pub fn manager(&self) -> Arc<Manager> {
        self.manager.upgrade().expect("manager dropped")
    }

    pub fn agentic_loop_start(&self) -> i64 {
        self.agentic_loop_start.load(Ordering::SeqCst)
    }

    /// downcast the behaviour (e.g. to reach webui state)
    pub fn behavior_as<T: 'static>(&self) -> Option<&T> {
        self.behavior.as_any().downcast_ref::<T>()
    }

    /// async constructor: resume or create a chat
    pub async fn init(&self) {
        self.chat.autoload().await;
    }

    // ------------------
    // Events
    // ------------------

    pub async fn shutdown_internal(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Some(t) = self.queue_task.lock().take() {
            t.abort();
        }
    }

    /// log a message through the manager (propagates to all channels)
    pub fn log(&self, category: &str, message: &str) {
        match self.manager.upgrade() {
            Some(m) => m.log(category, message),
            None => println!("[FATAL ERROR] failed to send log to channels: [{}] {}", category.to_uppercase(), message),
        }
    }

    /// console log with extra spice for errors
    pub fn log_error(&self, msg: &str, e: &str) {
        self.log("error", &format!("{}: {}", msg, e));
    }

    /// start the push queue consumer
    pub fn start_push_queue(self: &Arc<Self>) {
        let Some(mut rx) = self.push_rx.lock().take() else { return };
        let ch = self.clone();
        let task = tokio::spawn(async move {
            while !ch.shutting_down.load(Ordering::SeqCst) {
                let Some((message, defer)) = rx.recv().await else { break };
                if defer {
                    let mut idle = ch.chain_idle.subscribe();
                    while !*idle.borrow_and_update() {
                        if idle.changed().await.is_err() {
                            break;
                        }
                    }
                    ch.chat.messages_add(&message, false, false).await;
                }
                let formatted = ch.format_message(&message);
                if let Err(e) = ch.behavior.on_push(&ch, formatted).await {
                    ch.log(&ch.name, &format!("error in message consumer: {}", e));
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        });
        *self.queue_task.lock() = Some(task);
    }

    /// push a message to the push queue, which will instantly display it in the channel.
    /// strings become assistant messages.
    pub async fn push(&self, message: Value, defer: bool) -> bool {
        let message = if message.is_object() { message } else { json!({"role": "assistant", "content": py_str(&message)}) };
        if !defer {
            self.chat.messages_add(&message, false, false).await;
        }
        self.push_tx.send((message, defer)).is_ok()
    }

    /// send a stream into the channel without user input. the channel's on_stream()
    /// consumes it. returns the final assistant message.
    pub async fn push_stream(self: &Arc<Self>, stream: TokenStream) -> Value {
        let acc = Arc::new(Mutex::new((Vec::<String>::new(), Vec::<String>::new())));
        let acc2 = acc.clone();
        let wrapped: TokenStream = Box::pin(stream.inspect(move |token| {
            match token.get("type").and_then(|t| t.as_str()) {
                Some("content") => acc2.lock().0.push(token.get("content").map(py_str).unwrap_or_default()),
                Some("reasoning") => acc2.lock().1.push(token.get("content").map(py_str).unwrap_or_default()),
                _ => {}
            }
        }));
        if let Err(e) = self.behavior.on_stream(self, wrapped).await {
            self.log(&self.name, &format!("error in on_stream: {}", e));
        }
        let (content, reasoning) = acc.lock().clone();
        let mut msg = json!({"role": "assistant"});
        if !reasoning.is_empty() {
            msg["reasoning_content"] = json!(reasoning.concat());
        }
        if !content.is_empty() {
            msg["content"] = json!(content.concat());
        }
        msg
    }

    // --------------------
    // Helper methods
    // --------------------

    pub async fn set_as_active_channel(self: &Arc<Self>) {
        let manager = self.manager();
        if let Some(cur) = manager.current_channel() {
            if Arc::ptr_eq(&cur, self) {
                return;
            }
        }
        manager.set_current_channel(self.clone());
    }

    /// text content of a message as a string, even if it's multimodal
    pub fn extract_content(message: &Value) -> String {
        crate::chat::extract_content(message)
    }

    // ---------------------
    // Content Processors
    // ---------------------

    /// converts files into an openAI API multimodal message
    pub fn process_multimodal(message: Option<&Value>, files: Option<&Files>) -> Option<Value> {
        if let Some(Value::Array(list)) = message {
            return Some(json!({"role": "user", "content": list}));
        }
        let text = message.map(py_str).unwrap_or_default();
        let has_files = files.map(|f| !f.is_empty()).unwrap_or(false);
        if text.is_empty() && !has_files {
            return None;
        }
        if !has_files {
            return Some(json!({"role": "user", "content": message.cloned().unwrap_or(json!(""))}));
        }
        let mut blocks = Vec::new();
        let mut filenames = Vec::new();
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
            filenames.push(json!(""));
        }
        let format_map = |mime: &str| match mime {
            "audio/wav" | "audio/x-wav" => "wav",
            "audio/mp3" | "audio/mpeg" => "mp3",
            "audio/ogg" => "ogg",
            "audio/flac" | "audio/x-flac" => "flac",
            "audio/webm" => "webm",
            "audio/mp4" | "audio/aac" | "audio/m4a" | "audio/x-m4a" => "mp4",
            _ => "wav",
        };
        let b64 = base64::engine::general_purpose::STANDARD;
        for (filename, data) in files.unwrap() {
            if data.is_empty() {
                continue;
            }
            let mime = infer::get(data).map(|k| k.mime_type().to_string()).unwrap_or_else(|| "application/octet-stream".into());
            if mime.starts_with("image/") {
                blocks.push(json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{}", mime, b64.encode(data))}}));
            } else if mime.starts_with("audio/") {
                blocks.push(json!({"type": "input_audio", "input_audio": {"data": b64.encode(data), "format": format_map(&mime)}}));
            } else if mime == "application/pdf" {
                match pdf_extract::extract_text_from_mem(data) {
                    Ok(t) => blocks.push(json!({"type": "text", "text": format!("File: {}\n\n```pdf\n{}\n```", filename, t.trim())})),
                    Err(e) => blocks.push(json!({"type": "text", "text": format!("[Error extracting PDF '{}': {}]", filename, e)})),
                }
            } else {
                match std::str::from_utf8(data) {
                    Ok(t) => blocks.push(json!({"type": "text", "text": format!("File: {}\n\n```{}```", filename, t)})),
                    Err(_) => blocks.push(json!({"type": "text", "text": format!("[Binary file: {}]", filename)})),
                }
            }
            filenames.push(json!(filename));
        }
        if !blocks.is_empty() {
            return Some(json!({"role": "user", "content": blocks, "_metadata": {"filenames": filenames}}));
        }
        Some(json!({"role": "user", "content": message.cloned().unwrap_or(Value::Null)}))
    }

    /// render a message as a display string (reasoning/content/tool calls)
    pub fn format_message(&self, orig: &Value) -> Value {
        let mut message = orig.clone();
        let mut formatted = String::new();
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("").to_string();
        let show_reasoning = self.config.get_bool("show_reasoning", false);
        let mut reasoning_present = false;
        if role == "user" || role == "assistant" {
            if show_reasoning {
                if let Some(r) = message.get("reasoning_content").filter(|v| truthy(v)) {
                    formatted.push_str(&format!("**Reasoning:**\n{}\n\n", py_str(r)));
                    reasoning_present = true;
                }
            }
            if let Some(c) = message.get("content").filter(|v| truthy(v)) {
                if reasoning_present && show_reasoning {
                    formatted.push_str("**Conclusion**:\n");
                }
                formatted.push_str(&format!("{}\n\n", py_str(c)));
            }
        }
        if role == "assistant" {
            if let Some(Value::Array(tcs)) = message.get("tool_calls").filter(|v| truthy(v)) {
                for tc in tcs {
                    formatted.push_str(&crate::toolcalls::display_call(tc));
                    formatted.push('\n');
                }
                formatted.push_str("\n\n");
            }
        }
        if role == "tool" {
            formatted = "processing results..".into();
        }
        if message.is_object() {
            message["content"] = json!(formatted.trim());
        }
        message
    }

    /// incremental display of a streaming tool call (python _render_tool_token)
    pub fn render_tool_token(&self, name: &str, args_str: &str) -> String {
        let mut st = self.tool_state.lock();
        if st.0.as_deref() != Some(name) {
            st.0 = Some(name.to_string());
            st.1 = String::new();
            st.2 = Map::new();
            return format!("\n**Calling tool: {}**", name);
        }
        let data = match crate::lenient_json::partial_loads(args_str) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let mut delta = String::new();
        for (key, value) in data {
            let val_str = if value.is_object() || value.is_array() { pyjson::dumps(&value) } else { py_str(&value) };
            match st.2.get(&key).and_then(|v| v.as_str()).map(|s| s.to_string()) {
                None => {
                    delta.push_str(&format!("\n**{}**: ", key));
                    delta.push_str(&val_str);
                    st.2.insert(key, json!(val_str));
                }
                Some(prev) if prev != val_str => {
                    if let Some(rest) = val_str.strip_prefix(prev.as_str()) {
                        delta.push_str(rest);
                    } else {
                        delta.push_str(&val_str);
                    }
                    st.2.insert(key, json!(val_str));
                }
                _ => {}
            }
        }
        st.1 = args_str.to_string();
        delta
    }

    // -------------------------
    // The actual sending logic
    // -------------------------

    async fn send_preprocess(self: &Arc<Self>, message: &Value, files: Option<&Files>, commands_authorized: bool) -> Preprocessed {
        self.set_as_active_channel().await;
        let manager = self.manager();
        let mut user_message = match message {
            Value::Object(o) => o.get("content").cloned().unwrap_or(json!("")),
            other => other.clone(),
        };

        if let Value::String(text) = &user_message {
            let prefix = config::get_str(&["core", "cmd_prefix"]).unwrap_or_else(|| "/".into());
            let is_cmd = text.trim().to_lowercase().starts_with(&prefix.trim().to_lowercase());
            if is_cmd {
                match crate::commands::process_input(self, text, commands_authorized).await {
                    Err(e) => {
                        self.log(&self.name, &format!("Error while executing command: {}", e));
                        return Preprocessed::Error(e.to_string());
                    }
                    Ok(Some(resp)) if !resp.is_empty() => return Preprocessed::CmdResponse(resp),
                    Ok(_) => return Preprocessed::Blank,
                }
            }

            let mut current = text.clone();
            for (module_name, module) in manager.modules().iter() {
                match module.on_user_message(&current).await {
                    Ok(UserMessageHook::Intercept) => {
                        self.chat.messages_add(&json!({"role": "user", "content": current}), false, false).await;
                        return Preprocessed::ModuleIntercept;
                    }
                    Ok(UserMessageHook::Replace(s)) => current = s,
                    Ok(UserMessageHook::Unchanged) => {}
                    Err(e) => self.log("module error", &format!("{}: in on_user_message(): {}", module_name, crate::detail_error(&e))),
                }
            }
            user_message = json!(current);
        }

        let Some(processed) = Self::process_multimodal(Some(&user_message), files) else {
            return Preprocessed::Error("Unknown error while adding user message to context".into());
        };
        if !self.chat.messages_add(&processed, false, false).await {
            return Preprocessed::Error("Unknown error while adding user message to context".into());
        }
        self.agentic_loop_start.store(self.chat.messages_len() as i64 - 1, Ordering::SeqCst);

        if let Err(e) = manager.api().attempt_connect().await {
            return Preprocessed::Error(e.to_string());
        }
        let context = match self.context.get(GetOpts::default()).await {
            Ok(Some(c)) => c,
            Ok(None) => Vec::new(),
            Err(e) => return Preprocessed::Error(e.to_string()),
        };
        Preprocessed::Ready { user_message: processed.get("content").cloned().unwrap_or(Value::Null), context }
    }

    async fn send_postprocess(&self, assistant_message: &Value, add_to_context: bool) {
        if add_to_context {
            self.chat.messages_add(assistant_message, false, false).await;
        }
        let content = assistant_message.get("content").map(py_str).unwrap_or_default();
        for (name, module) in self.manager().modules().iter() {
            if let Err(e) = module.on_assistant_message(&content).await {
                self.log("module error", &format!("{}: in on_assistant_message(): {}", name, crate::detail_error(&e)));
            }
        }
    }

    fn build_final_assistant_message(content: &[String], reasoning: &[String]) -> Value {
        let mut m = json!({"role": "assistant", "content": content.concat()});
        if !reasoning.is_empty() {
            m["reasoning_content"] = json!(reasoning.concat());
        }
        m
    }

    /// add an error to context and return the error token to yield
    pub async fn throw_stream_error(&self, error: &str) -> Value {
        self.chat.messages_add(&json!({"role": "assistant", "content": format!("Error: {}", error)}), false, false).await;
        json!({"type": "error", "content": error})
    }

    /// sends a message to the AI from within this channel (non-streaming).
    /// returns the formatted assistant message, or None.
    pub async fn send(self: &Arc<Self>, message: Value, args: SendArgs) -> Option<Value> {
        let _ = self.chain_idle.send(false);
        let _guard = ChainIdleGuard(self.clone());
        self.send_inner(message, args).await
    }

    async fn send_inner(self: &Arc<Self>, message: Value, args: SendArgs) -> Option<Value> {
        let processed = self.send_preprocess(&message, args.files.as_ref(), args.commands_authorized).await;
        let context = match processed {
            Preprocessed::CmdResponse(c) => return Some(self.format_message(&json!({"role": "assistant", "content": c}))),
            Preprocessed::Blank | Preprocessed::ModuleIntercept => return None,
            Preprocessed::Error(e) => return Some(json!({"role": "assistant", "content": e})),
            Preprocessed::Ready { context, .. } => context,
        };
        let api = self.manager().api();
        let response = match api.send(&context, args.api.clone()).await {
            Ok(r) => r,
            Err(e) => {
                self.log("api", &e.to_string());
                return Some(json!({"role": "assistant", "content": e.to_string()}));
            }
        };
        let mut assistant_message = response;
        assistant_message["role"] = json!("assistant");
        if assistant_message.get("tool_calls").map(truthy).unwrap_or(false) {
            let mut final_message = None;
            let mut s = crate::toolcalls::process(self.clone(), assistant_message, true);
            while let Some(t) = s.next().await {
                if t.get("type").and_then(|x| x.as_str()) == Some("final") {
                    final_message = t.get("content").cloned();
                }
            }
            if let Some(fm) = final_message {
                self.send_postprocess(&fm, true).await;
                return Some(self.format_message(&fm));
            }
            return None;
        }
        self.send_postprocess(&assistant_message, args.add_to_context()).await;
        Some(self.format_message(&assistant_message))
    }

    /// sends a message to the AI from within this channel, streaming version
    pub fn send_stream(self: &Arc<Self>, message: Value, args: SendArgs) -> TokenStream {
        let ch = self.clone();
        Box::pin(async_stream::stream! {
            let _ = ch.chain_idle.send(false);
            let _guard = ChainIdleGuard(ch.clone());
            let mut inner = ch.send_stream_inner(message, args);
            while let Some(t) = inner.next().await {
                yield t;
            }
        })
    }

    fn send_stream_inner(self: &Arc<Self>, message: Value, args: SendArgs) -> TokenStream {
        let ch = self.clone();
        Box::pin(async_stream::stream! {
            let processed = ch.send_preprocess(&message, args.files.as_ref(), args.commands_authorized).await;
            let (user_message, context) = match processed {
                Preprocessed::CmdResponse(c) => {
                    yield json!({"type": "user_message", "content": message, "is_cmd": true});
                    yield json!({"type": "content", "content": c, "is_cmd": true});
                    return;
                }
                Preprocessed::Blank => {
                    yield json!({"type": "content", "content": "BLANK"});
                    return;
                }
                Preprocessed::ModuleIntercept => return,
                Preprocessed::Error(e) => {
                    yield json!({"type": "user_message", "content": message, "is_cmd": true});
                    yield ch.throw_stream_error(&e).await;
                    return;
                }
                Preprocessed::Ready { user_message, context } => (user_message, context),
            };

            yield json!({"type": "user_message", "content": user_message});
            let estimation = ch.context.get_total_tokens().await;
            yield json!({"type": "token_usage", "content": estimation, "source": "estimation"});

            let mut final_content: Vec<String> = Vec::new();
            let mut final_reasoning: Vec<String> = Vec::new();
            let mut tool_calls_occurred = false;
            let mut fetched_token_usage = false;
            let mut cancelled = false;

            let api = ch.manager().api();
            let mut stream = api.send_stream(context, args.api.clone());
            'outer: while let Some(token) = stream.next().await {
                if is_cancelled_token(&token) {
                    cancelled = true;
                    break;
                }
                let ttype = token.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
                if ttype == "error" {
                    let err = token.get("content").map(py_str).unwrap_or_default();
                    ch.log(&ch.name, &format!("Error: {}", err));
                    let partial = Self::build_final_assistant_message(&final_content, &final_reasoning);
                    ch.chat.messages_add(&partial, false, false).await;
                    yield ch.throw_stream_error(&err).await;
                    return;
                }
                yield token.clone();
                match ttype.as_str() {
                    "content" => final_content.push(token.get("content").map(py_str).unwrap_or_default()),
                    "reasoning" => final_reasoning.push(token.get("content").map(py_str).unwrap_or_default()),
                    "tool_calls" => {
                        tool_calls_occurred = true;
                        let req = crate::toolcalls::build_recursive_request(&ch, &token, &final_content, &final_reasoning);
                        let mut sub = crate::toolcalls::process(ch.clone(), req, false);
                        while let Some(st) = sub.next().await {
                            if is_cancelled_token(&st) {
                                cancelled = true;
                                break 'outer;
                            }
                            if st.get("type").and_then(|t| t.as_str()) == Some("final") {
                                if let Some(c) = st.get("content") {
                                    ch.chat.messages_add(c, false, false).await;
                                }
                            }
                            yield st;
                        }
                    }
                    "token_usage" => {
                        if let Some(u) = token.get("content").and_then(|v| v.as_i64()) {
                            if u > 0 {
                                ch.context.record_api_usage(u).await;
                                fetched_token_usage = true;
                            }
                        }
                    }
                    _ => {}
                }
            }

            if cancelled && tool_calls_occurred {
                return;
            }

            let source = if fetched_token_usage { "API" } else { "estimation" };
            if tool_calls_occurred {
                yield json!({"type": "token_usage", "content": ch.context.get_total_tokens().await, "source": source});
                return;
            }
            let assistant_message = Self::build_final_assistant_message(&final_content, &final_reasoning);
            ch.send_postprocess(&assistant_message, args.add_to_context()).await;
            yield json!({"type": "token_usage", "content": ch.context.get_total_tokens().await, "source": source});
        })
    }

    /// groups incoming tokens into turns (TurnCollector)
    pub fn group_stream(&self, stream: TokenStream) -> TokenStream {
        crate::turns::group_stream(stream)
    }

    /// message history grouped into turns
    pub fn group_history(&self) -> Vec<Value> {
        crate::turns::group_history(&self.chat.messages_get())
    }

    /// formats a stream of turn segments into text deltas for text-based channels.
    /// yields raw tokens, `{"type":"formatted"}` text and `{"type":"new_chunk"}` markers.
    pub fn format_stream_for_text(&self, stream: TokenStream, opts: TextFormatOpts) -> TokenStream {
        let show_reasoning = self.config.get_bool("show_reasoning", false);
        let grouped = self.group_stream(stream);
        Box::pin(async_stream::stream! {
            let strings = opts.strings.clone().unwrap_or_else(|| TextStrings::defaults(opts.use_markdown));
            let chunk_size = opts.chunk_size;
            let mut grouped = grouped;
            let mut first_turn = true;
            let mut last_turn_type: Option<String> = None;
            let mut currently_reasoning = false;
            let mut shown_reasoning_indicator = false;
            let mut last_tool_state: IndexMap<String, (String, Map<String, Value>)> = IndexMap::new();
            let mut last_content: Map<String, Value> = Map::new();
            let mut char_counter: usize = 0;

            macro_rules! emit {
                ($text:expr) => {{
                    let text: String = $text;
                    if let Some(cs) = chunk_size {
                        if char_counter >= cs {
                            yield json!({"type": "new_chunk", "content": ""});
                            char_counter = 0;
                        }
                    }
                    char_counter += text.chars().count();
                    yield json!({"type": "formatted", "content": text});
                }};
            }

            while let Some(token) = grouped.next().await {
                if token.get("type").and_then(|t| t.as_str()) == Some("token") {
                    yield token.get("content").cloned().unwrap_or(Value::Null);
                }
                if token.get("type").and_then(|t| t.as_str()) != Some("turn") {
                    continue;
                }
                let Some(segment) = token.get("content").filter(|s| truthy(s)).cloned() else { continue };
                let seg_type = segment.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
                if segment.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                    continue;
                }
                if Some(&seg_type) != last_turn_type.as_ref() {
                    if first_turn {
                        first_turn = false;
                    } else if !strings.separator.is_empty() {
                        emit!(format!("\n{}\n", strings.separator));
                    }
                    last_content = Map::new();
                    if seg_type == "reasoning" && show_reasoning {
                        currently_reasoning = true;
                        emit!(strings.thinking_header.clone());
                        emit!(strings.thinking_newline.clone());
                    } else if seg_type == "content" && currently_reasoning {
                        currently_reasoning = false;
                        emit!(strings.conclusion_header.clone());
                        emit!("\n".to_string());
                    }
                }

                if seg_type == "reasoning" {
                    if show_reasoning {
                        let current = segment.get("reasoning_content").map(py_str).unwrap_or_default();
                        let prev_len = last_content.get("reasoning").and_then(|v| v.as_str()).map(|s| s.chars().count()).unwrap_or(0);
                        let delta: String = current.chars().skip(prev_len).collect();
                        if !delta.is_empty() {
                            emit!(delta.replace('\n', &strings.thinking_newline));
                        }
                        last_content.insert("reasoning".into(), json!(current));
                    } else if !shown_reasoning_indicator {
                        if opts.show_indicators {
                            emit!("thinking..\n".to_string());
                        }
                        shown_reasoning_indicator = true;
                    }
                } else if seg_type == "content" {
                    shown_reasoning_indicator = false;
                    let current = segment.get("content").filter(|v| truthy(v)).map(py_str).unwrap_or_default();
                    let idx = last_content.get("content").and_then(|v| v.as_str()).map(|s| s.chars().count()).unwrap_or(0);
                    let delta: String = current.chars().skip(idx).collect();
                    if !delta.is_empty() {
                        emit!(delta);
                    }
                    last_content.insert("content".into(), json!(current));
                } else if seg_type == "tool_calls" {
                    for tc in segment.get("tool_calls").and_then(|t| t.as_array()).cloned().unwrap_or_default() {
                        let tc_id = tc.get("id").filter(|v| truthy(v)).map(py_str).unwrap_or_default();
                        if tc_id.is_empty() {
                            continue;
                        }
                        let tool_name = tc.get("function").and_then(|f| f.get("name")).map(py_str).unwrap_or_else(|| "unknown".into());
                        let args_str = tc.get("function").and_then(|f| f.get("arguments")).map(py_str).unwrap_or_default();
                        let (prev_name, prev_args) = last_tool_state.get(&tc_id).cloned().unwrap_or_default();
                        if prev_name != tool_name {
                            let header = strings.tool_call_header.replace("{tool_name}", &tool_name);
                            emit!("\n".to_string());
                            emit!(header);
                        }
                        let current_args = match crate::lenient_json::partial_loads(&args_str) {
                            Some(Value::Object(m)) => m,
                            _ => Map::new(),
                        };
                        let mut keys: Vec<String> = prev_args.keys().cloned().collect();
                        for k in current_args.keys() {
                            if !keys.contains(k) {
                                keys.push(k.clone());
                            }
                        }
                        for key in keys {
                            let prev_val = prev_args.get(&key).filter(|v| !v.is_null());
                            let cur_val = current_args.get(&key).filter(|v| !v.is_null());
                            let fmt = |v: &Value| if v.is_object() || v.is_array() { pyjson::dumps(v) } else { py_str(v) };
                            match (prev_val, cur_val) {
                                (None, cur) => {
                                    let val_str = cur.map(fmt).unwrap_or_else(|| "None".into());
                                    emit!("\n".to_string());
                                    emit!(format!("**{}**: ", key));
                                    emit!(val_str);
                                    emit!("\n".to_string());
                                }
                                (Some(_), None) => {}
                                (Some(p), Some(c)) => {
                                    let ps = fmt(p);
                                    let cs = fmt(c);
                                    if ps != cs {
                                        let delta = cs.strip_prefix(ps.as_str()).map(|s| s.to_string()).unwrap_or(cs.clone());
                                        if !delta.is_empty() {
                                            emit!(delta);
                                        }
                                    }
                                }
                            }
                        }
                        last_tool_state.insert(tc_id, (tool_name, current_args));
                    }
                }
                last_turn_type = Some(seg_type);
            }
        })
    }
}

/// strings used by format_stream_for_text
#[derive(Clone, Debug)]
pub struct TextStrings {
    pub thinking_header: String,
    pub thinking_newline: String,
    pub conclusion_header: String,
    pub separator: String,
    /// `{tool_name}` is substituted
    pub tool_call_header: String,
}

impl TextStrings {
    pub fn defaults(use_markdown: bool) -> Self {
        if use_markdown {
            Self {
                thinking_header: "**Thinking**".into(),
                thinking_newline: "\n> ".into(),
                conclusion_header: "".into(),
                separator: "".into(),
                tool_call_header: "🔧 calling tool *{tool_name}*".into(),
            }
        } else {
            Self {
                thinking_header: "Thinking:".into(),
                thinking_newline: "\n-> ".into(),
                conclusion_header: "".into(),
                separator: "-".repeat(8),
                tool_call_header: "🔧 calling tool {tool_name}".into(),
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct TextFormatOpts {
    pub chunk_size: Option<usize>,
    pub use_markdown: bool,
    pub strings: Option<TextStrings>,
    pub show_indicators: bool,
}

impl Default for TextFormatOpts {
    fn default() -> Self {
        Self { chunk_size: None, use_markdown: true, strings: None, show_indicators: true }
    }
}

/// names of all available channels (core.channel.get_available_channels)
pub fn get_available_channels() -> Vec<String> {
    config::get_module_structure()
        .into_iter()
        .filter(|(_, d)| matches!(d["metadata"]["type"].as_str(), Some("channel") | Some("user_channel")))
        .map(|(n, _)| n)
        .collect()
}
