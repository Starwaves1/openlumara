//! Port of channels/discord_bot/channel.py: talk to your AI over Discord.
//!
//! discord.py is replaced by a hand-rolled gateway client ([`gateway`]) and a small REST
//! client ([`rest`]). The bot talks in one target channel, responds when mentioned (or
//! when a mention keyword appears), optionally tracks the chat history of others, and
//! renders streamed responses by editing a placeholder message on an interval.

pub mod gateway;
pub mod rest;

#[path = "../../../../generated/channels/discord_bot.rs"]
mod meta;

use crate::util::{cfg_string, clen, cmd_prefix, fixed_chunks, lstrip_chars, slice, strip_chars};
use futures::StreamExt;
use ol_core::channel::{Channel, ChannelBehavior, Files, SendArgs};
use ol_core::pyjson::{as_f64, py_str};
use ol_core::registry::{ChannelDescriptor, Registry};
use parking_lot::Mutex;
use rest::{HttpError, Msg, Rest};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// python CHUNK_SIZE
pub const CHUNK_SIZE: usize = 1500;
/// discord.py's typing context re-triggers every 5 seconds
const TYPING_INTERVAL: Duration = Duration::from_secs(5);

pub fn register(r: &mut Registry) {
    r.add_channel(ChannelDescriptor {
        name: "discord_bot",
        class_name: meta::CLASS_NAME,
        doc: meta::DOC,
        settings: meta::settings,
        unsafe_: meta::UNSAFE,
        category: meta::CATEGORY,
        factory: || Arc::new(DiscordBot::new()),
    });
}

/// python _make_progress_bar
pub fn make_progress_bar(percentage: f64, size: usize) -> String {
    let filled = (size as f64 * percentage / 100.0).trunc() as i64;
    let bar = if filled <= 0 {
        "░".repeat(size)
    } else if filled as usize >= size {
        "█".repeat(size)
    } else {
        let f = filled as usize;
        format!("{}▓{}", "█".repeat(f - 1), "░".repeat(size - f))
    };
    format!("`{}` {:.0}%", bar, percentage)
}

/// python str.splitlines()
pub fn splitlines(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// the "## thinking.." block shown while streaming reasoning: last 5 lines, each
/// truncated to its last 244 characters when longer than 250
pub fn reasoning_snippet(reasoning_full: &str) -> String {
    let lines = splitlines(reasoning_full);
    let start = lines.len().saturating_sub(5);
    let snippet: Vec<String> = lines[start..]
        .iter()
        .map(|line| {
            let n = clen(line);
            if n > 250 {
                format!("> ...{}...", slice(line, n - 244, n).trim_end())
            } else {
                format!("> {}", line)
            }
        })
        .collect();
    format!("## thinking..\n{}", snippet.join("\n"))
}

/// python `"> " + "\n> ".join(content.split("\n"))`
pub fn quote_reply(content: &str) -> String {
    format!("> {}", content.split('\n').collect::<Vec<_>>().join("\n> "))
}

/// render the non-mentioned chat history tracker
pub fn history_string(history: &[(String, String)], cmd_prefix: &str) -> String {
    history.iter().map(|(author, content)| format!("{} said:\n{}\n\n", strip_chars(author, cmd_prefix), content)).collect()
}

/// a snowflake config value (python `int(config.get(...))`)
fn cfg_snowflake(v: Option<Value>) -> Option<String> {
    let s = cfg_string(v)?;
    let t = s.trim();
    t.parse::<u64>().ok().map(|n| n.to_string())
}

#[derive(Default)]
struct DState {
    rest: Mutex<Option<Rest>>,
    me_id: Mutex<String>,
    target_channel: Mutex<Option<String>>,
    history: Mutex<Vec<(String, String)>>,
    last_user_msg: Mutex<String>,
    stop: Arc<tokio::sync::Notify>,
}

impl DState {
    fn rest(&self) -> Option<Rest> {
        self.rest.lock().clone()
    }
}

pub struct DiscordBot {
    api_base: String,
    gateway_base: String,
    st: Arc<DState>,
}

impl Default for DiscordBot {
    fn default() -> Self {
        Self::new()
    }
}

impl DiscordBot {
    pub fn new() -> Self {
        Self::with_endpoints(rest::DEFAULT_API_BASE, gateway::DEFAULT_GATEWAY_URL)
    }

    /// custom REST/gateway endpoints (tests, proxies)
    pub fn with_endpoints(api_base: &str, gateway_base: &str) -> Self {
        Self { api_base: api_base.into(), gateway_base: gateway_base.into(), st: Arc::new(DState::default()) }
    }
}

/// DiscordClient.send_to_main: reply to `message` when use_replies is on, otherwise post
/// to the target channel
async fn send_to_main(ch: &Arc<Channel>, st: &DState, content: &str, message: Option<&Msg>) -> Result<Msg, HttpError> {
    let rest = st.rest().ok_or_else(|| HttpError { status: 0, reason: String::new(), code: 0, message: "the discord client is not running".into() })?;
    if let Some(m) = message {
        if ch.config.get_bool("use_replies", false) {
            return rest.send_message(&m.channel_id, content, Some(&m.id)).await;
        }
    }
    let target = st.target_channel.lock().clone().ok_or_else(|| HttpError { status: 0, reason: String::new(), code: 0, message: "'DiscordClient' object has no attribute 'target_channel'".into() })?;
    rest.send_message(&target, content, None).await
}

#[async_trait::async_trait]
impl ChannelBehavior for DiscordBot {
    async fn run(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let st = self.st.clone();
        let Some(token) = ch.config.get1("token").filter(|v| !v.is_null()) else {
            anyhow::bail!("expected token to be a str, received NoneType instead");
        };
        let token = py_str(&token).trim().to_string();
        let rest = Rest::new(&self.api_base, &token);
        // discord.py login(): validates the token
        if let Err(e) = rest.get_me().await {
            if e.status == 401 {
                anyhow::bail!("Improper token has been passed.");
            }
            anyhow::bail!("{}", e);
        }
        *st.rest.lock() = Some(rest);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let dispatcher = {
            let ch = ch.clone();
            let st = st.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    let ch = ch.clone();
                    let st = st.clone();
                    // discord.py runs every event handler as its own task
                    match ev {
                        gateway::GatewayEvent::Ready(d) => {
                            tokio::spawn(async move { on_ready(&ch, &st, d).await });
                        }
                        gateway::GatewayEvent::MessageCreate(m) => {
                            tokio::spawn(async move { on_message(&ch, &st, m).await });
                        }
                    }
                }
            })
        };
        let intents = gateway::intents::default_intents() | gateway::intents::MESSAGE_CONTENT;
        let res = gateway::run(self.gateway_base.clone(), token, intents, tx, st.stop.clone()).await;
        dispatcher.abort();
        res.map_err(|e| anyhow::anyhow!(e))
    }

    async fn on_shutdown(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let msg = cfg_string(ch.config.get1("shutdown_message")).filter(|s| !s.is_empty());
        let result = match msg {
            Some(m) if self.st.rest().is_some() => send_to_main(ch, &self.st, &m, None).await.map(|_| ()),
            _ => Ok(()),
        };
        self.st.stop.notify_waiters();
        result.map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn on_push(&self, ch: &Arc<Channel>, message: Value) -> anyhow::Result<()> {
        let content = message.get("content").map(py_str).unwrap_or_default();
        if clen(&content) < CHUNK_SIZE {
            send_to_main(ch, &self.st, &content, None).await?;
        } else {
            for chunk in fixed_chunks(&content, CHUNK_SIZE) {
                send_to_main(ch, &self.st, &chunk, None).await?;
            }
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

async fn on_ready(ch: &Arc<Channel>, st: &Arc<DState>, ready: Value) {
    *st.me_id.lock() = ready.get("user").and_then(|u| u.get("id")).and_then(|i| i.as_str()).unwrap_or("").to_string();
    ch.log(&ch.name, "Logged in");

    let Some(rest) = st.rest() else { return };
    match cfg_snowflake(ch.config.get1("target_channel_id")) {
        Some(id) => match rest.get_channel(&id).await {
            Ok(c) => *st.target_channel.lock() = Some(c.get("id").and_then(|i| i.as_str()).unwrap_or(&id).to_string()),
            Err(e) => ch.log(&ch.name, &format!("failed to retrieve target channel: {}", ol_core::detail_error(&e))),
        },
        None => ch.log(&ch.name, "failed to retrieve target channel: target_channel_id is not set to a valid channel ID"),
    }

    if let Some(startup) = cfg_string(ch.config.get1("startup_message")).filter(|s| !s.is_empty()) {
        if let Err(e) = send_to_main(ch, st, &startup, None).await {
            ch.log(&ch.name, &format!("error: {}", ol_core::detail_error(&e)));
        }
    }
}

/// is the bot addressed by this message (mention or mention keyword)?
pub fn is_mentioned(message: &Value, me_id: &str, keywords: &[String]) -> bool {
    let mut mentioned = message
        .get("mentions")
        .and_then(|m| m.as_array())
        .map(|m| m.iter().any(|u| u.get("id").and_then(|i| i.as_str()) == Some(me_id)))
        .unwrap_or(false);
    let content = message.get("content").and_then(|c| c.as_str()).unwrap_or("").to_lowercase();
    for keyword in keywords {
        if content.contains(&keyword.to_lowercase()) {
            mentioned = true;
        }
    }
    mentioned
}

/// remove the bot's own mentions from the text, like python
pub fn strip_own_mentions(content: &str, message: &Value, me_id: &str) -> String {
    let mut content = content.trim().to_string();
    let mentions_me = message
        .get("mentions")
        .and_then(|m| m.as_array())
        .map(|m| m.iter().any(|u| u.get("id").and_then(|i| i.as_str()) == Some(me_id)))
        .unwrap_or(false);
    if mentions_me {
        content = content.replace(&format!("<@{}>", me_id), "");
    }
    content.trim().to_string()
}

fn keep_typing(rest: Rest, channel_id: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let _ = rest.trigger_typing(&channel_id).await;
            tokio::time::sleep(TYPING_INTERVAL).await;
        }
    })
}

/// aborts the typing loop when dropped (python's `async with channel.typing()`)
struct TypingGuard(tokio::task::JoinHandle<()>);
impl Drop for TypingGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn on_message(ch: &Arc<Channel>, st: &Arc<DState>, message: Value) {
    let Some(rest) = st.rest() else { return };
    let prefix = cmd_prefix();
    let me_id = st.me_id.lock().clone();
    let author_id = message.get("author").and_then(|a| a.get("id")).and_then(|i| i.as_str()).unwrap_or("").to_string();
    let author_name = message.get("author").and_then(|a| a.get("username")).map(py_str).unwrap_or_default();
    let channel_id = message.get("channel_id").and_then(|c| c.as_str()).unwrap_or("").to_string();
    let raw_content = message.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
    let msg_ref = Msg { channel_id: channel_id.clone(), id: message.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string(), content: raw_content.clone() };

    // dont reply to its own messages
    if author_id == me_id {
        return;
    }
    // and dont spam channels that arent the target channel
    match cfg_snowflake(ch.config.get1("target_channel_id")) {
        Some(t) if t == channel_id => {}
        _ => return,
    }

    let require_mentions = ch.config.get_bool("require_mentions", true);
    // python leaves `mentioned` unbound (NameError) when require_mentions is off; the
    // evident intent is to answer every message in the target channel
    let mut mentioned = true;
    if require_mentions {
        let keywords: Vec<String> = ch.config.get1("mention_keywords").and_then(|v| v.as_array().cloned()).unwrap_or_default().iter().map(py_str).collect();
        mentioned = is_mentioned(&message, &me_id, &keywords);
    }

    // clear the history tracker if mentioned
    if mentioned {
        st.history.lock().clear();
    }

    // track messages said in chat that weren't mentions
    if require_mentions && ch.config.get_bool("track_chat_history", false) && !mentioned {
        let history_str = {
            let mut h = st.history.lock();
            h.push((author_name.clone(), raw_content.clone()));
            history_string(&h, &prefix)
        };
        let last = ch.chat.messages_get().last().cloned().unwrap_or(json!({}));
        if last.get("role").and_then(|r| r.as_str()) != Some("user") {
            ch.chat.messages_add(&json!({"role": "user", "content": history_str}), false, false).await;
        } else {
            let last_user = st.last_user_msg.lock().clone();
            let idx = ch.chat.messages_len().saturating_sub(1);
            ch.chat.messages_edit(idx, json!({"role": "user", "content": format!("{}\n\n{}", last_user, history_str)})).await;
        }
    }

    if !mentioned {
        return;
    }

    // determine whether non-public commands may be ran by the user
    let authorized = cfg_snowflake(ch.config.get1("authorized_user_id")).map(|id| id == author_id).unwrap_or(false);

    let mut content = strip_own_mentions(&raw_content, &message, &me_id);

    let mut is_cmd = false;
    if let Some((_, cmd, _)) = ol_core::commands::extract_cmd(&content) {
        if !cmd.is_empty() {
            is_cmd = content.to_lowercase().trim().starts_with(&prefix.to_lowercase());
        }
    }

    let mut files = Files::new();
    if !is_cmd {
        // process attachments
        for att in message.get("attachments").and_then(|a| a.as_array()).cloned().unwrap_or_default() {
            let filename = att.get("filename").map(py_str).unwrap_or_default();
            let url = att.get("url").and_then(|u| u.as_str()).unwrap_or("");
            match rest.download(url).await {
                Ok(data) => {
                    files.insert(filename, data);
                }
                Err(e) => ch.log(&ch.name, &format!("error: {}", ol_core::detail_error(&e))),
            }
        }

        let orig_content = content.clone();
        content = String::new();

        // check if the message is a reply
        if let Some(ref_id) = message.get("message_reference").and_then(|r| r.get("message_id")).and_then(|i| i.as_str()) {
            let replied = match rest.get_message(&channel_id, ref_id).await {
                Ok(m) => Some(m),
                Err(_) => message.get("referenced_message").filter(|m| !m.is_null()).cloned(),
            };
            if let Some(replied) = replied {
                let replied_content = replied.get("content").and_then(|c| c.as_str()).unwrap_or("");
                content.push_str(&format!("in reply to:\n{}\n\n", quote_reply(replied_content)));
            }
        }

        // if group chat is enabled, make the AI aware of who is speaking
        if ch.config.get_bool("use_group_chat", true) {
            content.push_str(&format!("{} said: {}", lstrip_chars(&author_name, &prefix), orig_content));
        } else {
            content.push_str(&orig_content);
        }
    }

    *st.last_user_msg.lock() = content.clone();

    let files = if files.is_empty() { None } else { Some(files) };
    let args = SendArgs { files, commands_authorized: authorized, ..Default::default() };
    let target = st.target_channel.lock().clone().unwrap_or_else(|| channel_id.clone());

    if ch.config.get_bool("use_streaming", true) {
        let _typing = TypingGuard(keep_typing(rest.clone(), target));
        if let Err(e) = stream_response(ch, st, &rest, content, args, &msg_ref).await {
            ch.log(&ch.name, &format!("error: {}", ol_core::detail_error(&e)));
        }
    } else {
        let response = {
            let _typing = TypingGuard(keep_typing(rest.clone(), target));
            ch.send(json!(content), args).await
        };
        let response = response.and_then(|r| r.get("content").map(py_str)).unwrap_or_default();
        if response.trim().is_empty() {
            return;
        }
        let chunks = if clen(&response) < CHUNK_SIZE { vec![response] } else { fixed_chunks(&response, CHUNK_SIZE) };
        for chunk in chunks {
            if let Err(e) = send_to_main(ch, st, &chunk, Some(&msg_ref)).await {
                ch.log(&ch.name, &format!("error: {}", ol_core::detail_error(&e)));
                return;
            }
        }
    }
}

/// the `use_streaming` branch of python's on_message
async fn stream_response(ch: &Arc<Channel>, st: &Arc<DState>, rest: &Rest, content: String, args: SendArgs, user_msg: &Msg) -> Result<(), HttpError> {
    let edit_interval = ch.config.get1("edit_interval").and_then(|v| as_f64(&v).or_else(|| py_str(&v).trim().parse().ok())).unwrap_or(1.0);
    let should_stream_text = ch.config.get_bool("stream_text", false);
    let show_reasoning = ch.config.get_bool("show_reasoning", false);

    // send a message that can be edited
    let mut msg = send_to_main(ch, st, "processing your request..", None).await?;

    let mut response_content = String::new();
    let mut chunk_content = String::new();
    let mut reasoning_content_full = String::new();
    let mut reasoning_content = String::new();
    let mut accumulated_toolcalls: Vec<String> = Vec::new();
    let mut timer = Instant::now();

    let mut stream = ch.send_stream(json!(content), args);
    while let Some(token) = stream.next().await {
        let step: Result<bool, HttpError> = async {
            let token_type = token.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let token_content = token.get("content").cloned().unwrap_or(Value::Null);
            let token_text = py_str(&token_content);

            let toolcalls_str = if accumulated_toolcalls.is_empty() {
                String::new()
            } else {
                let start = accumulated_toolcalls.len().saturating_sub(5);
                format!("{}\n\n", accumulated_toolcalls[start..].join("\n"))
            };

            if token_type == "error" {
                send_to_main(ch, st, &format!("✖ ERROR: {}", token_text), None).await?;
                return Ok(true);
            }
            if token_type == "user_message" || token_type == "token_usage" {
                return Ok(false);
            }
            if token_type == "prompt_progress" {
                let total = token_content.get("total").and_then(as_f64).unwrap_or(0.0);
                let processed = token_content.get("processed").and_then(as_f64).unwrap_or(0.0);
                let percentage = if total != 0.0 { processed / total * 100.0 } else { 0.0 };
                msg = rest.edit_message(&msg, &format!("{}{}", toolcalls_str, make_progress_bar(percentage, 12))).await?;
                return Ok(false);
            }

            if !should_stream_text {
                // just accumulate
                if token_type == "reasoning" {
                    reasoning_content.push_str(&token_text);
                    if msg.content != "thinking.." {
                        msg = rest.edit_message(&msg, "thinking..").await?;
                    }
                }
                if token_type == "content" {
                    if msg.content != "writing response.." {
                        msg = rest.edit_message(&msg, "writing response..").await?;
                    }
                    response_content.push_str(&token_text);
                }
                return Ok(false);
            }

            // edit-streaming logic
            if token_type == "tool_calls" {
                for tc in token.get("tool_calls").and_then(|t| t.as_array()).cloned().unwrap_or_default() {
                    accumulated_toolcalls.push(ol_core::toolcalls::display_call(&tc));
                }
            }
            if token_type == "reasoning" {
                reasoning_content_full.push_str(&token_text);
                reasoning_content = reasoning_snippet(&reasoning_content_full);
            } else if token_type == "content" {
                // erase reasoning from display
                reasoning_content.clear();
                chunk_content.push_str(&token_text);
                response_content.push_str(&token_text);
                if clen(&chunk_content) >= CHUNK_SIZE {
                    // finalize current message, start a new chunk message
                    msg = rest.edit_message(&msg, &chunk_content).await?;
                    msg = send_to_main(ch, st, "...", None).await?;
                    chunk_content.clear();
                }
            }

            if timer.elapsed().as_secs_f64() >= edit_interval {
                if !reasoning_content.is_empty() {
                    if show_reasoning {
                        msg = rest.edit_message(&msg, &format!("{}{}", toolcalls_str, reasoning_content)).await?;
                    } else if msg.content != format!("{}thinking..", toolcalls_str) {
                        msg = rest.edit_message(&msg, &format!("{}thinking..", toolcalls_str)).await?;
                    }
                } else if !chunk_content.trim().is_empty() {
                    msg = rest.edit_message(&msg, &chunk_content).await?;
                }
                timer = Instant::now();
            }
            Ok(false)
        }
        .await;
        match step {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => {
                ch.log(&ch.name, &format!("error: {}", ol_core::detail_error(&e)));
                let _ = send_to_main(ch, st, &format!("✖ ERROR: {}", ol_core::detail_error(&e)), None).await;
            }
        }
    }

    let response = response_content;
    if should_stream_text {
        if !chunk_content.trim().is_empty() {
            rest.edit_message(&msg, &chunk_content).await?;
        } else {
            // discord won't let us edit a message to be empty, so remove the placeholder instead
            rest.delete_message(&msg).await?;
        }
    } else if response.trim().is_empty() {
        rest.delete_message(&msg).await?;
    } else if clen(&response) < CHUNK_SIZE {
        rest.edit_message(&msg, &response).await?;
    } else {
        for (i, chunk) in fixed_chunks(&response, CHUNK_SIZE).into_iter().enumerate() {
            if i == 0 {
                msg = rest.edit_message(&msg, &chunk).await?;
            } else {
                send_to_main(ch, st, &chunk, Some(user_msg)).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_bar() {
        assert_eq!(make_progress_bar(0.0, 12), "`░░░░░░░░░░░░` 0%");
        assert_eq!(make_progress_bar(100.0, 12), "`████████████` 100%");
        assert_eq!(make_progress_bar(50.0, 12), "`█████▓░░░░░░` 50%");
        assert_eq!(make_progress_bar(9.0, 12), "`░░░░░░░░░░░░` 9%");
    }

    #[test]
    fn snippet() {
        let s = reasoning_snippet("a\nb\nc\nd\ne\nf");
        assert_eq!(s, "## thinking..\n> b\n> c\n> d\n> e\n> f");
        let long = "x".repeat(300);
        let s = reasoning_snippet(&long);
        assert_eq!(s, format!("## thinking..\n> ...{}...", "x".repeat(244)));
        assert_eq!(splitlines("a\r\nb\rc\n"), vec!["a", "b", "c"]);
    }

    #[test]
    fn reply_quote_and_history() {
        assert_eq!(quote_reply("a\nb"), "> a\n> b");
        assert_eq!(quote_reply(""), "> ");
        let h = vec![("/bob".to_string(), "hi".to_string()), ("amy".to_string(), "yo".to_string())];
        assert_eq!(history_string(&h, "/"), "bob said:\nhi\n\namy said:\nyo\n\n");
    }

    #[test]
    fn mentions() {
        let m = json!({"content": "<@42> hello there", "mentions": [{"id": "42"}]});
        assert!(is_mentioned(&m, "42", &[]));
        assert!(!is_mentioned(&m, "43", &[]));
        assert!(is_mentioned(&json!({"content": "Hey Lumara, hi", "mentions": []}), "42", &["hey lumara".into()]));
        assert_eq!(strip_own_mentions("<@42> hello there", &m, "42"), "hello there");
        assert_eq!(strip_own_mentions("<@43> hi", &m, "42"), "<@43> hi");
    }

    #[test]
    fn snowflakes() {
        assert_eq!(cfg_snowflake(Some(json!(1234567890123456789u64))), Some("1234567890123456789".into()));
        assert_eq!(cfg_snowflake(Some(json!("123"))), Some("123".into()));
        assert_eq!(cfg_snowflake(Some(Value::Null)), None);
        assert_eq!(cfg_snowflake(None), None);
    }
}
