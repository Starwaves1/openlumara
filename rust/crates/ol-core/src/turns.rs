//! Port of core/turns.py: groups raw messages / streamed tokens into display "turns".

use crate::api::TokenStream;
use crate::lenient_json::parse_lenient_json;
use crate::pyjson::{py_str_opt, truthy};
use futures::StreamExt;
use md5::{Digest, Md5};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

fn s_nonblank(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s)) if !s.trim().is_empty())
}

/// python str() of a message field the way turn_hash formats it
fn py_field(v: Option<&Value>) -> String {
    py_str_opt(v)
}

/// stable content hash for a grouped turn
pub fn turn_hash(turn: &Value) -> String {
    let mut parts = Vec::new();
    for message in turn.get("messages").and_then(|m| m.as_array()).cloned().unwrap_or_default() {
        let calls: Vec<String> = message
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|tc| {
                let resp = tc.get("_metadata").and_then(|m| m.get("response"));
                format!("{}:{}", py_field(tc.get("id")), py_field(resp))
            })
            .collect();
        let is_cmd = message.get("_metadata").and_then(|m| m.get("is_cmd"));
        parts.push(
            [
                py_field(message.get("index")),
                py_field(message.get("role")),
                py_field(message.get("content")),
                py_field(message.get("reasoning_content")),
                py_field(is_cmd),
                calls.join(","),
            ]
            .join("|"),
        );
    }
    let mut h = Md5::new();
    h.update(parts.join("\n").as_bytes());
    format!("{:x}", h.finalize())
}

pub const TOOL_ARG_PRIORITY: [&str; 16] = [
    "path", "file_path", "filepath", "file", "filename", "url", "query", "pattern", "regex_pattern", "sub_path", "subfolder",
    "folder", "id", "name", "content", "text",
];

/// a tool call failed when its parsed response is {status: "error"}
pub fn tool_call_failed(tool: &Value) -> bool {
    let Some(resp) = tool.get("_metadata").and_then(|m| m.get("response")) else { return false };
    if resp.is_null() {
        return false;
    }
    let parsed = match resp {
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(v) => v,
            Err(_) => return false,
        },
        other => other.clone(),
    };
    parsed.get("status").and_then(|s| s.as_str()) == Some("error")
}

fn truncate_arg(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 70 {
        return s.to_string();
    }
    if s.contains('/') || s.contains('\\') {
        return format!("..{}", chars[chars.len() - 68..].iter().collect::<String>());
    }
    format!("{}..", chars[..69].iter().collect::<String>().trim_end())
}

fn arg_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(_) | Value::Array(_) => crate::pyjson::dumps(v),
        Value::Null => "null".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Number(n) => {
            if n.is_f64() {
                crate::pyjson::float_repr(n.as_f64().unwrap())
            } else {
                n.to_string()
            }
        }
    }
}

/// one-line arg summary for completed tool call headers
pub fn tool_call_summary(tool: &Value) -> String {
    let raw = tool.get("function").and_then(|f| f.get("arguments"));
    if !matches!(raw, Some(Value::String(_))) {
        return String::new();
    }
    let args = parse_lenient_json(raw);
    if args.contains_key("_raw") || args.is_empty() {
        return String::new();
    }
    let mut chosen: Option<(&String, &Value)> = None;
    for key in TOOL_ARG_PRIORITY {
        if let Some((k, v)) = args.iter().find(|(k, _)| k.as_str() == key) {
            chosen = Some((k, v));
            break;
        }
    }
    let chosen = chosen.unwrap_or_else(|| args.iter().find(|(_, v)| v.is_string()).unwrap_or_else(|| args.iter().next().unwrap()));
    format!("({})", truncate_arg(&arg_to_string(chosen.1)))
}

fn fn_name(tool: &Value) -> Option<String> {
    tool.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string())
}

fn upper_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// deduped per-function counts grouped by module (first token of the tool name)
pub fn modules_for(tool_calls: &[Value]) -> Value {
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for tool in tool_calls {
        let Some(f) = fn_name(tool) else { continue };
        if !counts.contains_key(&f) {
            counts.insert(f.clone(), 0);
            order.push(f.clone());
        }
        *counts.get_mut(&f).unwrap() += 1;
    }
    let mut groups: Vec<(String, Vec<(String, i64)>)> = Vec::new();
    for f in order {
        let parts: Vec<&str> = f.split('_').collect();
        let module = upper_first(parts[0]);
        let action = parts[1..].join("_");
        let idx = match groups.iter().position(|(m, _)| *m == module) {
            Some(i) => i,
            None => {
                groups.push((module.clone(), Vec::new()));
                groups.len() - 1
            }
        };
        let count = counts[&f];
        if let Some(existing) = groups[idx].1.iter_mut().find(|(n, _)| *n == action) {
            existing.1 += count;
        } else {
            groups[idx].1.push((action, count));
        }
    }
    Value::Array(
        groups
            .into_iter()
            .map(|(m, actions)| {
                json!({"module": m, "actions": actions.into_iter().map(|(n, c)| json!({"name": n, "count": c})).collect::<Vec<_>>()})
            })
            .collect(),
    )
}

/// running: some call still lacks a response. failed: every call failed. thoughts: tool-less step.
pub fn step_status(tool_calls: &[Value]) -> &'static str {
    if tool_calls.is_empty() {
        return "thoughts";
    }
    if tool_calls.iter().any(|t| t.get("_metadata").and_then(|m| m.get("response")).map(|r| r.is_null()).unwrap_or(true)) {
        return "running";
    }
    if tool_calls.iter().all(tool_call_failed) {
        return "failed";
    }
    "done"
}

fn is_final_content(msg: &Value) -> bool {
    msg.get("role").and_then(|r| r.as_str()) == Some("assistant") && !truthy_opt(msg.get("tool_calls")) && s_nonblank(msg.get("content"))
}

fn truthy_opt(v: Option<&Value>) -> bool {
    v.map(truthy).unwrap_or(false)
}

fn segment_label(segment: &Value) -> String {
    if let Some(Value::Array(tcs)) = segment.get("tool_calls") {
        if !tcs.is_empty() {
            if let Some(f) = fn_name(tcs.last().unwrap()) {
                let parts: Vec<&str> = f.split('_').collect();
                return format!("{}: {}", upper_first(parts[0]), parts[1..].join(" "));
            }
        }
    }
    if s_nonblank(segment.get("reasoning_content")) {
        return "Thinking..".into();
    }
    if s_nonblank(segment.get("content")) {
        return "writing".into();
    }
    String::new()
}

fn has_visible_chain_content(msg: &Value) -> bool {
    if truthy_opt(msg.get("tool_calls")) {
        return msg["tool_calls"].as_array().map(|a| !a.is_empty()).unwrap_or(false);
    }
    if s_nonblank(msg.get("reasoning_content")) {
        return true;
    }
    if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") && s_nonblank(msg.get("content")) {
        return true;
    }
    false
}

fn tool_calls_of(m: &Value) -> Vec<Value> {
    m.get("tool_calls").and_then(|t| t.as_array()).cloned().unwrap_or_default()
}

fn str_or_empty(v: Option<&Value>) -> Value {
    match v {
        Some(x) if truthy(x) => x.clone(),
        _ => json!(""),
    }
}

/// split a finalized assistant turn's messages into (steps, final)
pub fn build_steps(messages: &[Value]) -> (Vec<Value>, Vec<Value>) {
    let final_index = (0..messages.len()).rev().find(|&i| is_final_content(&messages[i]));
    let mut chain: Vec<Value> = Vec::new();
    let mut fin: Vec<Value> = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        if Some(i) == final_index {
            if s_nonblank(message.get("reasoning_content")) {
                let mut chain_copy = message.clone();
                chain_copy["content"] = json!("");
                chain.push(chain_copy);
                let mut final_copy = message.clone();
                if let Some(o) = final_copy.as_object_mut() {
                    o.shift_remove("reasoning_content");
                }
                fin.push(final_copy);
            } else {
                fin.push(message.clone());
            }
        } else if has_visible_chain_content(message) {
            chain.push(message.clone());
        }
    }

    let is_thought = |msg: &Value| !truthy_opt(msg.get("tool_calls")) && (s_nonblank(msg.get("reasoning_content")) || s_nonblank(msg.get("content")));

    let mut steps = Vec::new();
    let mut step_num = 0;
    let mut i = 0;
    while i < chain.len() {
        let m = &chain[i];
        if truthy_opt(m.get("tool_calls")) {
            step_num += 1;
            let tcs = tool_calls_of(m);
            steps.push(json!({
                "step": step_num,
                "status": step_status(&tcs),
                "reasoning_content": str_or_empty(m.get("reasoning_content")),
                "content": str_or_empty(m.get("content")),
                "tool_calls": tcs,
                "modules": modules_for(&tcs),
            }));
            i += 1;
            continue;
        }
        let mut j = i;
        while j < chain.len() && is_thought(&chain[j]) {
            j += 1;
        }
        let tool_seg = if j > i && j < chain.len() && truthy_opt(chain[j].get("tool_calls")) { Some(&chain[j]) } else { None };
        let mut reasoning = String::new();
        let mut content = String::new();
        for c in &chain[i..j] {
            if let Some(r) = c.get("reasoning_content").filter(|v| truthy(v)).and_then(|v| v.as_str()) {
                reasoning = if reasoning.is_empty() { r.to_string() } else { format!("{}\n\n{}", reasoning, r) };
            }
            if let Some(r) = c.get("content").filter(|v| truthy(v)).and_then(|v| v.as_str()) {
                content = if content.is_empty() { r.to_string() } else { format!("{}\n\n{}", content, r) };
            }
        }
        if let Some(ts) = tool_seg {
            step_num += 1;
            let tcs = tool_calls_of(ts);
            steps.push(json!({
                "step": step_num,
                "status": step_status(&tcs),
                "reasoning_content": reasoning,
                "content": content,
                "tool_calls": tcs,
                "modules": modules_for(&tcs),
            }));
            i = j + 1;
        } else if j == chain.len() {
            step_num += 1;
            steps.push(json!({
                "step": step_num, "status": "thoughts", "reasoning_content": reasoning, "content": content,
                "tool_calls": [], "modules": [],
            }));
            i = j;
        } else {
            step_num += 1;
            steps.push(json!({
                "step": step_num, "status": "thoughts",
                "reasoning_content": str_or_empty(m.get("reasoning_content")),
                "content": str_or_empty(m.get("content")),
                "tool_calls": [], "modules": [],
            }));
            i += 1;
        }
    }
    (steps, fin)
}

/// Groups all finalized messages from history into turns.
pub fn group_history(history: &[Value]) -> Vec<Value> {
    let mut turns: Vec<Value> = Vec::new();
    let mut current: Option<Value> = None;
    for (index, msg) in history.iter().enumerate() {
        let mut msg = msg.clone();
        if let Some(o) = msg.as_object_mut() {
            o.insert("index".into(), json!(index));
        }
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("").to_string();
        if role == "user" {
            if let Some(t) = current.take() {
                turns.push(t);
            }
            turns.push(json!({"role": "user", "messages": [msg], "first_message_index": index}));
        } else {
            if current.is_none() {
                current = Some(json!({"role": "assistant", "first_message_index": index, "messages": []}));
            }
            let t = current.as_mut().unwrap();
            t["last_message_index"] = json!(index);
            t["messages"].as_array_mut().unwrap().push(msg);
        }
    }
    if let Some(t) = current {
        turns.push(t);
    }

    for turn in turns.iter_mut() {
        if turn["role"] != "assistant" {
            continue;
        }
        let mut response_map: HashMap<String, Value> = HashMap::new();
        for msg in turn["messages"].as_array().unwrap() {
            if msg.get("role").and_then(|r| r.as_str()) == Some("tool") {
                let id = crate::pyjson::py_str_opt(msg.get("tool_call_id"));
                response_map.insert(id, msg.get("content").cloned().unwrap_or(Value::Null));
            }
        }
        for msg in turn["messages"].as_array_mut().unwrap() {
            if !truthy_opt(msg.get("tool_calls")) {
                continue;
            }
            let mut annotated = Vec::new();
            for tool in msg["tool_calls"].as_array().cloned().unwrap_or_default() {
                let mut tool = tool;
                if !tool.is_object() {
                    annotated.push(tool);
                    continue;
                }
                let mut meta = Map::new();
                let id = crate::pyjson::py_str_opt(tool.get("id"));
                if tool.get("id").is_some() {
                    if let Some(r) = response_map.get(&id) {
                        meta.insert("response".into(), r.clone());
                    }
                }
                tool["_metadata"] = Value::Object(meta.clone());
                let failed = tool_call_failed(&tool);
                let summary = tool_call_summary(&tool);
                let args = parse_lenient_json(tool.get("function").and_then(|f| f.get("arguments")));
                meta.insert("failed".into(), json!(failed));
                meta.insert("summary".into(), json!(summary));
                meta.insert("args_parsed".into(), Value::Object(args));
                tool["_metadata"] = Value::Object(meta);
                annotated.push(tool);
            }
            msg["tool_calls"] = Value::Array(annotated);
        }
    }

    for turn in turns.iter_mut() {
        let h = turn_hash(turn);
        turn["hash"] = json!(h);
    }

    for turn in turns.iter_mut() {
        if turn["role"] != "assistant" {
            continue;
        }
        let msgs = turn["messages"].as_array().cloned().unwrap_or_default();
        let (steps, fin) = build_steps(&msgs);
        turn["steps"] = Value::Array(steps);
        turn["final_messages"] = Value::Array(fin);
    }
    turns
}

fn annotate_stream_tools(segment: &mut Value, response_map: &HashMap<String, Value>, args_cache: &mut HashMap<String, Map<String, Value>>) {
    let Some(tcs) = segment.get_mut("tool_calls").and_then(|t| t.as_array_mut()) else { return };
    for tool in tcs.iter_mut() {
        if !tool.is_object() {
            continue;
        }
        if !tool.get("_metadata").map(|m| m.is_object()).unwrap_or(false) {
            tool["_metadata"] = json!({});
        }
        let id = crate::pyjson::py_str_opt(tool.get("id"));
        if tool.get("id").is_some() {
            if let Some(r) = response_map.get(&id) {
                tool["_metadata"]["response"] = r.clone();
                let failed = tool_call_failed(tool);
                tool["_metadata"]["failed"] = json!(failed);
            }
        }
        let summary = tool_call_summary(tool);
        tool["_metadata"]["summary"] = json!(summary);
        // monotonic args_parsed per tool id
        let parsed = parse_lenient_json(tool.get("function").and_then(|f| f.get("arguments")));
        let parsed = match args_cache.get(&id) {
            Some(prev) if prev.len() > parsed.len() => prev.clone(),
            _ => {
                args_cache.insert(id.clone(), parsed.clone());
                parsed
            }
        };
        tool["_metadata"]["args_parsed"] = Value::Object(parsed);
    }
}

/// takes the raw stream and yields `{"type":"token"}` passthrough tokens and
/// `{"type":"turn"}` accumulated segments (TurnCollector.group_stream)
pub fn group_stream(stream: TokenStream) -> TokenStream {
    Box::pin(async_stream::stream! {
        let mut stream = stream;
        let mut current: Option<Value> = None;
        let mut last_segment_type: Option<String> = None;
        let mut response_map: HashMap<String, Value> = HashMap::new();
        let mut last_tool_call_id: Option<Value> = None;
        let mut last_tool_calls_segment: Option<Value> = None;
        let mut current_is_tool_calls_segment = false;
        let mut step_num: i64 = 0;
        let mut step_has_tools = false;
        let mut args_cache: HashMap<String, Map<String, Value>> = HashMap::new();

        while let Some(raw) = stream.next().await {
            let mut token = raw.clone();
            let ttype = token.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();

            if ttype == "prompt_progress" {
                if let Some(Value::Object(progress)) = token.get("content").cloned() {
                    let mut progress = progress;
                    let num = |k: &str| progress.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let cache = num("cache");
                    let processed = num("processed") - cache;
                    let total = num("total") - cache;
                    let elapsed = num("time_ms");
                    let percent = if total > 0.0 { ((processed / total) * 100.0).round() } else { 0.0 };
                    let eta = if processed > 0.0 && total > processed { (elapsed / 1000.0 / processed) * (total - processed) } else { 0.0 };
                    progress.insert("percent".into(), json!(percent as i64));
                    progress.insert("eta_seconds".into(), json!((eta * 10.0).round() / 10.0));
                    token["content"] = Value::Object(progress);
                }
            }

            yield json!({"type": "token", "content": token.clone()});

            if matches!(ttype.as_str(), "prompt_progress" | "token_usage" | "timings" | "user_message" | "model_load_progress") {
                continue;
            }
            if let Some(o) = token.as_object_mut() {
                if o.get("timings").map(truthy).unwrap_or(false) {
                    o.shift_remove("timings");
                }
            }

            let mut segment_type = ttype.clone();
            if segment_type == "tool_call_delta" || segment_type == "tool_calls" {
                segment_type = "tool_calls".into();
            }
            let is_new_tool_response = segment_type == "tool" && last_tool_call_id.as_ref() != token.get("tool_call_id");

            if Some(&segment_type) != last_segment_type.as_ref() || is_new_tool_response {
                // finalize the previous tool_calls segment reference
                if current_is_tool_calls_segment {
                    if let Some(c) = current.clone() {
                        last_tool_calls_segment = Some(c);
                    }
                }
                let mut seg = token.clone();
                seg["role"] = json!(if segment_type != "tool" { "assistant" } else { "tool" });
                seg["type"] = json!(segment_type.clone());
                if segment_type != "tool" {
                    if step_num == 0 || step_has_tools {
                        step_num += 1;
                        step_has_tools = false;
                    }
                    if segment_type == "tool_calls" {
                        step_has_tools = true;
                    }
                    seg["step"] = json!(step_num);
                }
                current_is_tool_calls_segment = false;
                match segment_type.as_str() {
                    "reasoning" => {
                        let c = token.get("content").cloned().unwrap_or(json!(""));
                        if let Some(o) = seg.as_object_mut() {
                            o.shift_remove("content");
                            o.entry("reasoning_content").or_insert(c);
                        }
                    }
                    "content" => {
                        if let Some(o) = seg.as_object_mut() {
                            o.entry("content").or_insert(json!(""));
                        }
                    }
                    "tool_calls" => {
                        let tcs = token.get("tool_calls").cloned().unwrap_or(json!([]));
                        if let Some(o) = seg.as_object_mut() {
                            o.insert("tool_calls".into(), tcs);
                        }
                        current_is_tool_calls_segment = true;
                    }
                    "tool" => {
                        seg["type"] = json!("tool_response");
                        if let Some(o) = seg.as_object_mut() {
                            o.entry("content").or_insert(json!(""));
                        }
                    }
                    _ => {}
                }
                current = Some(seg);
                last_segment_type = Some(segment_type.clone());
                last_tool_call_id = if segment_type == "tool" { token.get("tool_call_id").cloned() } else { None };
            } else if let Some(seg) = current.as_mut() {
                match segment_type.as_str() {
                    "tool_calls" => {
                        if let Some(Value::Array(incoming)) = token.get("tool_calls") {
                            if !seg.get("tool_calls").map(|t| t.is_array()).unwrap_or(false) {
                                seg["tool_calls"] = json!([]);
                            }
                            let existing = seg["tool_calls"].as_array_mut().unwrap();
                            for call in incoming {
                                let call_id = call.get("id").filter(|v| truthy(v)).cloned();
                                if let Some(cid) = call_id {
                                    if let Some(pos) = existing.iter().position(|e| e.get("id") == Some(&cid)) {
                                        // keep previously stamped metadata keys (python replaced the dict
                                        // and re-stamped right after; the result is identical)
                                        existing[pos] = call.clone();
                                    } else {
                                        existing.push(call.clone());
                                    }
                                } else if existing.last().map(|l| !l.get("id").map(truthy).unwrap_or(false)).unwrap_or(false) {
                                    let n = existing.len();
                                    existing[n - 1] = call.clone();
                                } else {
                                    existing.push(call.clone());
                                }
                            }
                        }
                    }
                    "tool" => {
                        let prev = seg.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                        let add = token.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        seg["content"] = json!(prev + add);
                    }
                    _ => {
                        let key = if segment_type == "reasoning" { "reasoning_content" } else { "content" };
                        let prev = seg.get(key).and_then(|c| c.as_str()).unwrap_or("").to_string();
                        let add = token.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        seg[key] = json!(prev + add);
                    }
                }
            }

            if ttype == "tool" {
                let id = crate::pyjson::py_str_opt(token.get("tool_call_id"));
                response_map.insert(id, token.get("content").cloned().unwrap_or(json!("")));
            }

            let seg_type = current.as_ref().and_then(|c| c.get("type")).and_then(|t| t.as_str()).unwrap_or("").to_string();
            if seg_type != "tool_response" {
                let seg = current.as_mut().unwrap();
                if truthy_opt(seg.get("tool_calls")) {
                    annotate_stream_tools(seg, &response_map, &mut args_cache);
                    let tcs = tool_calls_of(seg);
                    seg["step_status"] = json!(step_status(&tcs));
                    seg["modules"] = modules_for(&tcs);
                } else {
                    seg["step_status"] = json!("thinking");
                }
                let label = segment_label(seg);
                seg["label"] = json!(label);
                if has_visible_chain_content(seg) {
                    yield json!({"type": "turn", "content": seg.clone()});
                }
                if current_is_tool_calls_segment {
                    last_tool_calls_segment = Some(seg.clone());
                }
            } else if let Some(ltc) = last_tool_calls_segment.as_mut() {
                annotate_stream_tools(ltc, &response_map, &mut args_cache);
                let tcs = tool_calls_of(ltc);
                ltc["step_status"] = json!(step_status(&tcs));
                let label = segment_label(ltc);
                ltc["label"] = json!(label);
                yield json!({"type": "turn", "content": ltc.clone()});
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_grouping() {
        let history = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "", "reasoning_content": "think", "tool_calls": [{"id": "c1", "function": {"name": "memory_create", "arguments": "{\"content\": \"x\"}"}, "type": "function"}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "{\"status\": \"success\", \"content\": \"ok\"}"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let turns = group_history(&history);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1]["first_message_index"], json!(1));
        assert_eq!(turns[1]["last_message_index"], json!(3));
        let tc = &turns[1]["messages"][0]["tool_calls"][0];
        assert_eq!(tc["_metadata"]["failed"], json!(false));
        assert_eq!(tc["_metadata"]["summary"], json!("(x)"));
        assert_eq!(turns[1]["steps"][0]["status"], json!("done"));
        assert_eq!(turns[1]["steps"][0]["modules"], json!([{"module": "Memory", "actions": [{"name": "create", "count": 1}]}]));
        assert_eq!(turns[1]["final_messages"][0]["content"], json!("done"));
        assert_eq!(turns[0]["hash"].as_str().unwrap().len(), 32);
    }

    #[tokio::test]
    async fn stream_grouping() {
        let tokens = vec![
            json!({"type": "user_message", "content": "hi"}),
            json!({"type": "reasoning", "content": "a"}),
            json!({"type": "reasoning", "content": "b"}),
            json!({"type": "content", "content": "x"}),
        ];
        let s: TokenStream = Box::pin(futures::stream::iter(tokens));
        let out: Vec<Value> = group_stream(s).collect().await;
        let turns: Vec<&Value> = out.iter().filter(|t| t["type"] == "turn").collect();
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[1]["content"]["reasoning_content"], json!("ab"));
        assert_eq!(turns[2]["content"]["content"], json!("x"));
        assert_eq!(turns[2]["content"]["step"], json!(1));
    }
}
