//! Port of core/context.py: builds the context window sent to the API, token
//! estimation/accounting, and (reversible) context compression.

use crate::api::{is_cancelled_token, ApiError, SendOpts};
use crate::channel::Channel;
use crate::config;
use crate::pyjson::{self, truthy};
use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

/// arguments of Context.get()
#[derive(Clone, Copy, Debug)]
pub struct GetOpts {
    pub system_prompt: bool,
    pub end_prompt: bool,
    pub history: bool,
    pub prevent_recursion: bool,
    pub trim: bool,
}

impl Default for GetOpts {
    fn default() -> Self {
        Self { system_prompt: true, end_prompt: true, history: true, prevent_recursion: false, trim: true }
    }
}

type CacheKey = (Option<usize>, bool, bool, bool, bool, bool);

struct PromptCacheEntry {
    core: Vec<String>,
    snaps: Vec<String>,
}

pub struct Context {
    ch: Weak<Channel>,
    pub compressing: AtomicBool,
    pub using_api_token_data: AtomicBool,
    prompt_cache_state: Mutex<VecDeque<(CacheKey, PromptCacheEntry)>>,
}

const APPROVED_KEYS: [&str; 7] = ["role", "content", "reasoning_content", "tool_calls", "tool_call_id", "function_call", "tool"];

const COMPRESS_PROMPT: &str = r#"[SYSTEM INSTRUCTION]
Compress the conversation so far into a handoff summary for a brand-new session with zero other context. A fresh instance reading ONLY this summary must continue seamlessly.

Format (markdown, no preamble, no closing remarks):

1. STATE - 1-3 sentences: what's in flight right now, status, open questions.
2. REQUESTS → OUTCOMES - one bullet per user request: "request: outcome". Merge similar exchanges into one bullet. Telegraphic style (no full sentences, no articles where droppable).
3. FACTS - only specifics needed to continue: exact quoted values (paths, names, IDs, commands, dates, error strings), decisions, preferences learned THIS session. Omit anything already inferable from the system prompt.
4. TOOLS - only calls whose results still matter for continuing: "tool(args-key): result". Omit routine/obsolete calls entirely.
5. NEXT - explicit remaining actions as imperative bullets. Write "none" if done.

Hard rules:
- Brevity beats completeness EXCEPT for exact values (paths, IDs, names, quotes) - those must be verbatim.
- Target: smallest summary that loses no actionable information. If a detail wouldn't change what you do next, cut it.
- No filler ("the user asked whether...", "it was decided that..."). Use fragments.
- NEVER restate identity, memories, tools, scheduler, or anything from the system prompt.
- Never invent or guess details not present in the history.
- No meta-commentary about summarizing. Output the summary only."#;

fn signal_of(m: &Value) -> Option<&str> {
    m.get("_metadata").and_then(|md| md.get("signal")).and_then(|s| s.as_str())
}

fn meta_flag(m: &Value, key: &str) -> bool {
    m.get("_metadata").and_then(|md| md.get(key)).map(truthy).unwrap_or(false)
}

fn strip_key(m: &Value, key: &str) -> Value {
    let mut m = m.clone();
    if let Some(o) = m.as_object_mut() {
        o.shift_remove(key);
    }
    m
}

impl Context {
    pub fn new(ch: Weak<Channel>) -> Self {
        Self {
            ch,
            compressing: AtomicBool::new(false),
            using_api_token_data: AtomicBool::new(false),
            prompt_cache_state: Mutex::new(VecDeque::new()),
        }
    }

    fn ch(&self) -> Arc<Channel> {
        self.ch.upgrade().expect("channel dropped")
    }

    /// the SUMMARIZATION_CUTOFF signal message
    pub fn summarization_cutoff() -> Value {
        json!({"_metadata": {"signal": "SUMMARIZATION_CUTOFF"}})
    }

    /// builds the full context window: system prompt + message history + end prompt.
    /// Ok(None) means the system prompt alone exceeds the context size.
    pub async fn get(&self, opts: GetOpts) -> Result<Option<Vec<Value>>, ApiError> {
        let ch = self.ch();
        let manager = ch.manager();
        let api = manager.api();
        if !api.connected() {
            if let Err(e) = api.connect().await {
                ch.log("api", &e.to_string());
                return Err(e);
            }
        }

        let max_messages = config::get_i64(&["api", "max_messages"], 200).max(0) as usize;
        let max_tokens = config::get_i64(&["api", "max_context"], 16768);
        let dev = api.supports_developer_role();
        let system_role = if dev { "developer" } else { "system" };
        let dev_role = if dev { "developer" } else { "user" };

        let mut system_msg: Vec<Value> = Vec::new();
        if opts.system_prompt {
            let content = ch.chat.get_system_prompt().await;
            if !content.is_empty() {
                system_msg.push(json!({"role": system_role, "content": content}));
            }
        }

        let mut messages: Vec<Value> = Vec::new();
        let mut summary_ref: Option<Value> = None;
        if opts.history {
            messages = ch.chat.messages_get();
            if let Some(i) = (0..messages.len()).rev().find(|&i| signal_of(&messages[i]) == Some("SUMMARIZATION_CUTOFF")) {
                let content = messages.get(i + 1).and_then(|m| m.get("content")).cloned().unwrap_or(Value::Null);
                let sref = json!({"role": "user", "content": content});
                let mut rest = vec![sref.clone()];
                if i + 2 <= messages.len() {
                    rest.extend(messages[(i + 2).min(messages.len())..].iter().cloned());
                }
                messages = rest;
                summary_ref = Some(sref);
            }
            messages.retain(|m| !meta_flag(m, "ghost") && signal_of(m).is_none());
            messages.retain(|m| {
                !(m.get("role").and_then(|r| r.as_str()) == Some("assistant")
                    && !m.get("content").map(truthy).unwrap_or(false)
                    && !m.get("tool_calls").map(truthy).unwrap_or(false))
            });
            if !config::get_bool(&["model", "keep_reasoning_in_context"], false) {
                messages = messages.iter().map(|m| strip_key(m, "reasoning_content")).collect();
            }
            if config::get_bool(&["model", "only_preserve_reasoning_for_current_agentic_loop"], false) {
                // python slice messages[:loop_idx] with a possibly negative index
                let loop_idx = ch.agentic_loop_start();
                let n = messages.len() as i64;
                let end = if loop_idx < 0 { (n + loop_idx).max(0) } else { loop_idx.min(n) } as usize;
                for m in messages.iter_mut().take(end) {
                    if m.get("tool_calls").is_some() {
                        *m = strip_key(m, "reasoning_content");
                    }
                }
            }
            if messages.len() > max_messages {
                messages = messages[messages.len() - max_messages..].to_vec();
            }
        }

        let mut end_msg: Vec<Value> = Vec::new();
        if opts.end_prompt {
            let histend = ch.chat.get_end_prompt(opts.prevent_recursion).await;
            if !histend.is_empty() {
                end_msg.push(json!({"role": dev_role, "content": histend}));
            }
        }

        // inject module injections into user messages
        for m in messages.iter_mut() {
            let inj = m.get("_metadata").and_then(|md| md.get("injection")).filter(|v| truthy(v)).cloned();
            if let Some(inj) = inj {
                if m.get("role").and_then(|r| r.as_str()) == Some("user") {
                    if let Some(Value::String(c)) = m.get("content").cloned() {
                        if !c.is_empty() {
                            m["content"] = json!(format!("{}\n\n{}", c, pyjson::py_str(&inj)));
                        }
                    }
                }
            }
        }

        // strip non-standard fields
        messages = messages
            .into_iter()
            .map(|m| {
                let o = m.as_object().cloned().unwrap_or_default();
                Value::Object(o.into_iter().filter(|(k, _)| APPROVED_KEYS.contains(&k.as_str())).collect())
            })
            .collect();
        for m in messages.iter_mut() {
            if m.get("tool_calls").map(truthy).unwrap_or(false) {
                if let Some(Value::Array(tcs)) = m.get("tool_calls").cloned() {
                    m["tool_calls"] = Value::Array(tcs.into_iter().map(|tc| if tc.is_object() { strip_key(&tc, "_metadata") } else { tc }).collect());
                }
            }
        }

        // enforce correct turn order
        if !messages.is_empty() {
            let mut enforced: Vec<Value> = Vec::new();
            for msg in messages {
                if let Some(last) = enforced.last() {
                    let lr = last.get("role").and_then(|r| r.as_str()).unwrap_or("");
                    let cr = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
                    match (lr, cr) {
                        ("assistant", "assistant") => enforced.push(json!({"role": "user", "content": " "})),
                        ("user", "user") | ("tool", "user") | ("user", "tool") => enforced.push(json!({"role": "assistant", "content": " "})),
                        _ => {}
                    }
                }
                enforced.push(msg);
            }
            messages = enforced;
        }

        let tools = manager.tools();
        let tool_tokens = if tools.is_empty() { 0 } else { count_tokens(&Value::Array(tools.clone())) };

        let build = |msgs: &[Value]| -> Vec<Value> { system_msg.iter().chain(msgs.iter()).chain(end_msg.iter()).cloned().collect() };
        let mut full = build(&messages);
        let mut current_tokens = count_messages_tokens(&full) + tool_tokens;
        let effective_max = (max_tokens as f64 * 0.95) as i64;

        if current_tokens > effective_max && !messages.is_empty() && opts.trim {
            let keep: Vec<Value> = summary_ref.iter().cloned().collect();
            let rest: Vec<Value> = if summary_ref.is_some() { messages[1..].to_vec() } else { messages.clone() };
            let (mut lo, mut hi) = (0i64, rest.len() as i64);
            let mut best = rest.len();
            while lo <= hi {
                let mid = (lo + hi) / 2;
                let cand: Vec<Value> = keep.iter().chain(rest[mid as usize..].iter()).cloned().collect();
                let tokens = tool_tokens + count_messages_tokens(&build(&cand));
                if tokens <= effective_max {
                    best = mid as usize;
                    hi = mid - 1;
                } else {
                    lo = mid + 1;
                }
            }
            messages = keep.into_iter().chain(rest[best..].iter().cloned()).collect();
            full = build(&messages);
            current_tokens = tool_tokens + count_messages_tokens(&full);
        }

        if opts.trim && current_tokens > max_tokens {
            ch.push(
                json!(format!("Your system prompt of {} tokens somehow exceeds the maximum context size of {}! Please set a larger context size. Or disable some modules, disable system prompt insertion across modules, do whatever you can to reduce token size.", current_tokens, max_tokens)),
                true,
            )
            .await;
            api.disconnect().await;
            return Ok(None);
        }

        if crate::globals::debug() {
            self.debug_prompt_cache_check(&full, (ch.chat.current(), opts.system_prompt, opts.end_prompt, opts.history, opts.prevent_recursion, opts.trim));
        }

        Ok(Some(full))
    }

    /// context.get() that maps errors/None to an empty context (for estimations)
    pub async fn get_or_empty(&self, opts: GetOpts) -> Vec<Value> {
        Box::pin(self.get(opts)).await.ok().flatten().unwrap_or_default()
    }

    pub async fn is_over_threshold(&self) -> bool {
        if !config::get_bool(&["model", "automatically_compress_context"], false) {
            return false;
        }
        let max_tokens = config::get_f64(&["api", "max_context"], 0.0);
        if max_tokens == 0.0 {
            return false;
        }
        let threshold = config::get_f64(&["model", "context_compression_threshold"], 0.0);
        let used = self.get_total_tokens().await;
        if used == 0 {
            return false;
        }
        (used as f64 / max_tokens) >= threshold
    }

    /// returns the newest handoff summary in this chat, or None if never compacted
    pub fn get_latest_summary(&self) -> Option<Value> {
        let messages = self.ch().chat.messages_get();
        for i in (0..messages.len()).rev() {
            if signal_of(&messages[i]) == Some("SUMMARIZATION_CUTOFF") {
                return messages.get(i + 1).and_then(|m| m.get("content").cloned());
            }
        }
        None
    }

    /// compress context using the summarization cutoff signal (reversible). returns the summary message
    pub async fn compress(&self) -> Option<Value> {
        if self.compressing.swap(true, Ordering::SeqCst) {
            return None;
        }
        let ch = self.ch();
        let result = async {
            ch.chat.invalidate_system_prompt();
            let prev_summary = self.get_latest_summary().filter(|v| truthy(v));
            let mut prompt = COMPRESS_PROMPT.to_string();
            if let Some(prev) = prev_summary {
                prompt.push_str(&format!(
                    "\n\nPREVIOUS HANDOFF SUMMARY\nThis conversation was compacted before. Your previous handoff summary is quoted below. Merge it into your output:\n- Move finished NEXT items into REQUESTS -> OUTCOMES.\n- Replace STATE with the current status based on the newest messages.\n- Carry over all FACTS and exact values (paths, IDs, names, quotes) unless a newer message contradicts them.\n- Delete entries invalidated by newer messages.\nOutput ONE unified summary in the same format - never a diff, never mention this section.\n\n\"\"\"\n{}\n\"\"\"",
                    pyjson::py_str(&prev)
                ));
            }
            let mut active_request = None;
            for msg in ch.chat.messages_get().iter().rev() {
                if msg.get("role").and_then(|r| r.as_str()) == Some("user") && !meta_flag(msg, "is_cmd") && signal_of(msg).is_none() && !meta_flag(msg, "ghost") {
                    active_request = Some(crate::chat::extract_content(msg));
                    break;
                }
            }
            if let Some(req) = active_request.filter(|r| !r.is_empty()) {
                prompt.push_str(&format!("\n\nThe user's active request is the message below. Before the numbered sections, output a line \"ACTIVE USER REQUEST:\" followed by this message quoted verbatim. Do NOT paraphrase it.\n\"\"\"\n{}\n\"\"\"", req));
            }

            let mut ctx = self.get_or_empty(GetOpts { end_prompt: false, ..Default::default() }).await;
            ctx.push(json!({"role": "user", "content": prompt}));
            let stream = ch.manager().api().send_stream(ctx, SendOpts::default());
            let response = ch.push_stream(stream).await;
            if !truthy(&response) {
                return None;
            }
            ch.chat.messages_add(&Context::summarization_cutoff(), false, false).await;
            ch.chat.messages_add(&response, false, false).await;
            if ch.chat.current().is_some() {
                ch.chat.set_current_field("token_usage", json!(0));
                ch.chat.set_current_field("token_usage_mark", json!(0));
                self.using_api_token_data.store(false, Ordering::SeqCst);
            }
            Some(response)
        }
        .await;
        self.compressing.store(false, Ordering::SeqCst);
        result
    }

    /// detailed breakdown of current token use (used by /status and the webui context popup)
    pub async fn get_size(&self) -> Value {
        let ch = self.ch();
        let max_context = config::get_i64(&["api", "max_context"], 16768);
        let full = self.get_or_empty(GetOpts::default()).await;
        let system_present = !ch.chat.get_system_prompt().await.is_empty();
        let histend = ch.chat.get_end_prompt(true).await;
        let sys_msgs: Vec<Value> = if system_present && !full.is_empty() { full[..1].to_vec() } else { vec![] };
        let end_msgs: Vec<Value> = if !histend.is_empty() && full.len() > sys_msgs.len() { full[full.len() - 1..].to_vec() } else { vec![] };
        let hist_msgs: Vec<Value> = full[sys_msgs.len()..full.len() - end_msgs.len()].to_vec();

        let sys_tokens = count_messages_tokens(&sys_msgs);
        let sys_words = pyjson::word_count(&pyjson::py_repr(&Value::Array(sys_msgs.clone())));
        let hist_words = pyjson::word_count(&pyjson::py_repr(&Value::Array(hist_msgs)));
        let end_tokens = count_messages_tokens(&end_msgs);
        let end_words = if end_msgs.is_empty() { 0 } else { pyjson::word_count(&pyjson::py_repr(&Value::Array(end_msgs.clone()))) };
        let tools = ch.manager().tools();
        let tools_v = Value::Array(tools.clone());
        let tool_tokens = count_tokens(&tools_v);
        let tool_words = pyjson::word_count(&pyjson::py_repr(&tools_v));
        let combined = tool_words + sys_words + hist_words + end_words;
        let token_usage = self.get_total_tokens().await;
        let pct = if max_context != 0 { ((token_usage as f64 / max_context as f64) * 100.0).round() as i64 } else { 0 };
        let compaction_count = ch.chat.messages_get().iter().filter(|m| signal_of(m) == Some("SUMMARIZATION_CUTOFF")).count();
        let hist_tokens = (token_usage - sys_tokens - end_tokens - tool_tokens).max(0);
        json!({
            "max_context": max_context,
            "total_tokens": token_usage,
            "percent_full": pct,
            "compaction_count": compaction_count,
            "total_words": combined,
            "system_prompt": {"tokens": sys_tokens, "words": sys_words},
            "tools": {"active": tools.len(), "tokens": tool_tokens, "words": tool_words},
            "message_history": {"tokens": hist_tokens, "words": hist_words},
            "end_prompt": {"tokens": end_tokens, "words": end_words},
        })
    }

    /// caches the API's real prompt token count as the single source of truth
    pub async fn record_api_usage(&self, num_tokens: i64) {
        let ch = self.ch();
        if ch.chat.current().is_none() || num_tokens <= 0 {
            return;
        }
        ch.chat.set_current_field("token_usage", json!(num_tokens));
        ch.chat.set_current_field("token_usage_mark", json!(ch.chat.messages_len()));
        self.using_api_token_data.store(true, Ordering::SeqCst);
        ch.chat.save().await;
    }

    /// total tokens taken up by the prompt + the tools array
    pub async fn get_total_tokens(&self) -> i64 {
        let ch = self.ch();
        if ch.chat.current().is_some() {
            let api_base = ch.chat.get("token_usage").and_then(|v| pyjson::as_i64(&v)).unwrap_or(0);
            let mark = ch.chat.get("token_usage_mark").and_then(|v| pyjson::as_i64(&v)).unwrap_or(0);
            if api_base > 0 && mark > 0 {
                let messages = ch.chat.messages_get();
                if (mark as usize) <= messages.len() {
                    let mut delta = 0;
                    if messages.len() > mark as usize {
                        let stripped: Vec<Value> = messages[mark as usize..].iter().map(|m| strip_key(m, "_metadata")).collect();
                        delta = count_messages_tokens(&stripped);
                    }
                    return api_base + delta;
                }
            }
        }
        let context = self.get_or_empty(GetOpts::default()).await;
        if context.is_empty() {
            return 0;
        }
        let mut n = count_messages_tokens(&context);
        let tools = ch.manager().tools();
        if !tools.is_empty() {
            n += count_tokens(&Value::Array(tools));
        }
        n
    }

    fn debug_prompt_cache_check(&self, context: &[Value], flags: CacheKey) {
        let ch = self.ch();
        let snaps: Vec<String> = context
            .iter()
            .map(|m| pyjson::dumps_opts(m, pyjson::DumpOpts { indent: None, ensure_ascii: false, sort_keys: true }))
            .collect();
        let hashes: Vec<String> = snaps.iter().map(|s| format!("{:x}", Sha256::digest(s.as_bytes()))).collect();
        let (core_hashes, core_snaps) = if flags.2 && !hashes.is_empty() {
            (hashes[..hashes.len() - 1].to_vec(), snaps[..snaps.len() - 1].to_vec())
        } else {
            (hashes.clone(), snaps.clone())
        };
        let mut state = self.prompt_cache_state.lock();
        let prev_pos = state.iter().position(|(k, _)| *k == flags);
        let prev = prev_pos.map(|p| state.remove(p).unwrap().1);
        if prev_pos.is_none() && state.len() >= 8 {
            state.pop_front();
        }
        state.push_back((flags, PromptCacheEntry { core: core_hashes.clone(), snaps: core_snaps.clone() }));
        drop(state);
        let Some(prev) = prev else { return };

        let first_diff = (0..prev.core.len().min(core_hashes.len())).find(|&i| prev.core[i] != core_hashes[i]);
        if first_diff.is_none() && core_hashes.len() >= prev.core.len() {
            return;
        }
        let mut reason: Option<String> = None;
        let mut expected = false;
        if core_hashes.len() < prev.core.len() && core_hashes[..] == prev.core[prev.core.len() - core_hashes.len()..] {
            reason = Some(format!("history trimmed from the front ({} messages dropped)", prev.core.len() - core_hashes.len()));
        } else if core_hashes.len() < prev.core.len() {
            reason = Some("context compressed (history rewritten into a summary)".into());
            expected = true;
        }
        if reason.is_none() {
            if let Some(0) = first_diff {
                let p_role = serde_json::from_str::<Value>(&prev.snaps[0]).ok().and_then(|v| v.get("role").and_then(|r| r.as_str()).map(|s| s.to_string()));
                if matches!(p_role.as_deref(), Some("system") | Some("developer")) {
                    reason = Some("system prompt changed".into());
                    expected = true;
                }
            }
        }
        let tag = if expected { "expected" } else { "UNEXPECTED" };
        let fd = first_diff.map(|d| d.to_string()).unwrap_or_else(|| "None".into());
        if let Some(r) = reason {
            ch.log("prompt-cache", &format!("[{}] prompt cache invalidated :: {}", tag, r));
            ch.log("prompt-cache", &format!("  first divergence at message index {} (core messages: {} -> {})", fd, prev.core.len(), core_hashes.len()));
            return;
        }
        let mut changed = Vec::new();
        let mut reason = "unknown".to_string();
        if let Some(d) = first_diff {
            if d < prev.snaps.len() && d < core_snaps.len() {
                let p: Value = serde_json::from_str(&prev.snaps[d]).unwrap_or(json!({}));
                let c: Value = serde_json::from_str(&core_snaps[d]).unwrap_or(json!({}));
                let mut keys: Vec<String> = p.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
                for k in c.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
                keys.sort();
                for k in keys {
                    if p.get(&k) != c.get(&k) {
                        ch.log("prompt-cache", &format!("    field '{}' changed", k));
                        changed.push(k);
                    }
                }
                reason = if changed == ["reasoning_content"] {
                    "reasoning_content text changed on a past message".into()
                } else if !changed.is_empty() && changed.iter().all(|k| k == "tool_calls") {
                    "tool_calls mutated in-place on a past message (shared nested reference!)".into()
                } else if changed == ["content"] {
                    "content text changed on a past message".into()
                } else {
                    "message content mutated".into()
                };
            }
        }
        ch.log("prompt-cache", &format!("[UNEXPECTED] prompt cache invalidated :: {}", reason));
        ch.log("prompt-cache", &format!("  first divergence at message index {} (core messages: {} -> {})", fd, prev.core.len(), core_hashes.len()));
        if !changed.is_empty() {
            ch.log("prompt-cache", &format!("  changed fields: {}", changed.join(", ")));
        }
    }
}

/// 1 token is roughly 4 characters
pub fn count_text_tokens(text: &str) -> i64 {
    (text.chars().count() / 4) as i64
}

/// json.dumps the data and estimate its tokens
pub fn count_tokens(data: &Value) -> i64 {
    count_text_tokens(&pyjson::dumps(data))
}

/// count tokens of a message array, counting only text parts of multimodal content
pub fn count_messages_tokens(messages: &[Value]) -> i64 {
    let cleaned: Vec<Value> = messages
        .iter()
        .map(|m| match m.get("content") {
            Some(Value::Array(items)) => {
                let mut m = m.clone();
                m["content"] = Value::Array(items.iter().filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("text")).cloned().collect());
                m
            }
            _ => m.clone(),
        })
        .collect();
    count_tokens(&Value::Array(cleaned))
}

/// drain a stream fully (helper for callers that don't care about tokens)
pub async fn drain(mut s: crate::api::TokenStream) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(t) = s.next().await {
        if is_cancelled_token(&t) {
            break;
        }
        out.push(t);
    }
    out
}
