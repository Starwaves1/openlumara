//! Port of core/toolcalls.py: executes tool calls and runs the recursive agentic loop.

use crate::api::{is_cancelled_token, SendOpts, TokenStream};
use crate::channel::Channel;
use crate::config;
use crate::context::GetOpts;
use crate::lenient_json::repair_loads;
use crate::module::{validate_tool_args, ToolArgs};
use crate::pyjson::{self, py_str};
use crate::tool_loader::ToolLoader;
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// format a toolcall into a nice string for display to the user
pub fn display_call(tool_data: &Value) -> String {
    let Some(function) = tool_data.get("function") else { return "🔧 Calling tool...".into() };
    let func_name = function.get("name").map(py_str).unwrap_or_else(|| "unknown".into());
    let raw_args = function.get("arguments").cloned().unwrap_or(json!("{}"));
    let args: Map<String, Value> = match raw_args {
        Value::String(s) => match repair_loads(&s) {
            Value::Object(m) => m,
            _ => Map::new(),
        },
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let parts: Vec<String> = args
        .iter()
        .map(|(k, v)| {
            let mut s = py_str(v);
            if pyjson::char_len(&s) > 30 {
                s = format!("{}..", pyjson::take_chars(&s, 30));
            }
            format!("{}=\"{}\"", k, s.replace('"', "'"))
        })
        .collect();
    format!("🔧 {}({})", func_name, parts.join(", "))
}

/// normalize tool call arguments into json strings of dicts
pub fn repair_tool_calls(ch: &Channel, tool_calls: &[Value]) -> Vec<Value> {
    tool_calls
        .iter()
        .map(|tc| {
            let mut tc = tc.clone();
            let raw = tc.get("function").and_then(|f| f.get("arguments")).cloned().unwrap_or(Value::Null);
            let args = match raw {
                Value::Object(m) => Value::Object(m),
                Value::String(s) => repair_loads(&s),
                other => {
                    ch.log("error", &format!("unexpected arguments type: {}", py_str(&other)));
                    json!({})
                }
            };
            let args = if args.is_object() {
                args
            } else {
                ch.log("error", &format!("Arguments not a dict: {}", py_str(&args)));
                json!({})
            };
            if tc.get("function").map(|f| f.is_object()).unwrap_or(false) {
                tc["function"]["arguments"] = json!(pyjson::dumps(&args));
            }
            tc
        })
        .collect()
}

/// build the assistant message that carries the tool calls (+ accumulated content/reasoning)
pub fn build_recursive_request(ch: &Channel, token: &Value, final_content: &[String], final_reasoning: &[String]) -> Value {
    let tool_calls = token.get("tool_calls").and_then(|t| t.as_array()).cloned().unwrap_or_default();
    let repaired = repair_tool_calls(ch, &tool_calls);
    let mut req = Map::new();
    req.insert("role".into(), json!("assistant"));
    if !final_content.is_empty() {
        req.insert("content".into(), json!(final_content.concat()));
    }
    if !final_reasoning.is_empty() {
        req.insert("reasoning_content".into(), json!(final_reasoning.concat()));
    }
    req.insert("tool_calls".into(), Value::Array(repaired));
    Value::Object(req)
}

fn err_json(msg: &str) -> String {
    pyjson::dumps(&json!({"content": msg, "status": "error"}))
}

/// execute a single tool call; returns the tool response string
async fn execute_one(ch: &Arc<Channel>, tool_call: &Value) -> String {
    let id = tool_call.get("id").cloned().unwrap_or(Value::Null);
    let _ = id;
    let tool_name = tool_call.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("").to_string();
    let args_v = repair_loads(tool_call.get("function").and_then(|f| f.get("arguments")).and_then(|a| a.as_str()).unwrap_or(""));
    let tool_args: Map<String, Value> = args_v.as_object().cloned().unwrap_or_default();
    let dynamic = config::get_bool(&["model", "dynamic_tool_loading"], true);
    let timeout_val = config::get_f64(&["core", "tool_timeout"], 10.0);
    let manager = ch.manager();
    let is_meta = ToolLoader::is_meta_tool(&tool_name);

    enum Target {
        Meta,
        Module(Arc<dyn crate::module::Module>, String),
    }

    let target = if is_meta {
        if !dynamic {
            return pyjson::dumps(&json!({
                "content": "Dynamic tool loading is disabled. The meta tool (tools_load) is not available. You have access to all available tools without needing to load them.",
                "status": "error"
            }));
        }
        Target::Meta
    } else {
        let entry = ch.tool_loader.catalog_entry(&tool_name);
        let module = entry.as_ref().and_then(|e| manager.module(&e.module));
        let (Some(entry), Some(module)) = (entry, module) else {
            ch.log("toolcall", &format!("tried to call tool {} but couldn't find it", tool_name));
            return if !dynamic {
                err_json(&format!("Tool named {} does not exist.", tool_name))
            } else {
                err_json(&format!("No tool named {} exists. Use tools_load with the module name to load that module's tools.", tool_name))
            };
        };
        if module.disabled_tools().contains(&entry.method) {
            return err_json("That tool has been disabled by the user.");
        }
        if !manager.tool_names().contains(&tool_name) {
            return if !dynamic {
                err_json(&format!("Tool {} is not available. This tool may be disabled or unavailable.", tool_name))
            } else {
                let first = tool_name.split('_').next().unwrap_or("");
                err_json(&format!("Tool {} is not loaded. Load it first by calling tools_load with module_names=[\"{}\"], then call it again.", tool_name, first))
            };
        }
        Target::Module(module, entry.method)
    };

    ch.log("toolcall", &display_call(tool_call));

    let fut = async {
        match &target {
            Target::Meta => {
                if tool_args.keys().any(|k| k != "module_names") {
                    let bad = tool_args.keys().find(|k| *k != "module_names").unwrap();
                    return Err(format!("ToolLoader.tools_load() got an unexpected keyword argument '{}'", bad));
                }
                let Some(names) = tool_args.get("module_names") else {
                    return Err("ToolLoader.tools_load() missing 1 required positional argument: 'module_names'".to_string());
                };
                let names: Vec<String> = match names {
                    Value::Array(a) => a.iter().map(py_str).collect(),
                    // python iterates a string character by character
                    Value::String(s) => s.chars().map(|c| c.to_string()).collect(),
                    other => return Err(format!("'{}' object is not iterable", py_type_name(other))),
                };
                Ok(ch.tool_loader.tools_load(&names))
            }
            Target::Module(module, method) => {
                let class_name = crate::registry::module(module.name()).map(|d| d.class_name).unwrap_or("Module");
                let spec = module.tools().into_iter().find(|s| s.method == method.as_str());
                if let Some(spec) = &spec {
                    validate_tool_args(class_name, spec, &tool_args)?;
                }
                module.call_tool(method, ToolArgs::new(tool_args.clone())).await.map_err(|e| crate::detail_error(&e))
            }
        }
    };

    let response = match tokio::time::timeout(Duration::from_secs_f64(timeout_val.max(0.0)), fut).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            let r = json!({"status": "error", "content": format!("Error while executing tool: {}", e)});
            ch.log("toolcall", r["content"].as_str().unwrap_or(""));
            r
        }
        Err(_) => {
            let r = json!({"status": "error", "content": format!("Tool timed out after {}s", pyjson::float_repr(timeout_val))});
            ch.log("toolcall", r["content"].as_str().unwrap_or(""));
            r
        }
    };

    match response {
        Value::String(s) => s,
        other => pyjson::dumps(&other),
    }
}

fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_f64() {
                "float"
            } else {
                "int"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// process tool calls from an assistant message, then continue the agentic loop.
/// yields tool responses, streamed tokens of follow-up requests, and finally
/// `{"type": "final", "content": <assistant message>}`.
pub fn process(ch: Arc<Channel>, assistant_message: Value, push: bool) -> TokenStream {
    Box::pin(async_stream::stream! {
        let Some(tool_calls) = assistant_message.get("tool_calls").and_then(|t| t.as_array()).cloned().filter(|t| !t.is_empty()) else { return };
        let repaired = repair_tool_calls(&ch, &tool_calls);
        let mut assistant_message = assistant_message.clone();
        assistant_message["tool_calls"] = Value::Array(repaired.clone());

        ch.chat.messages_add(&assistant_message, false, false).await;
        if push {
            ch.push(assistant_message.clone(), false).await;
        }

        let api = ch.manager().api();
        for tc in &repaired {
            let id = tc.get("id").cloned().unwrap_or(Value::Null);
            let content = execute_one(&ch, tc).await;
            yield json!({"type": "tool", "tool_call_id": id, "content": content});
            ch.chat.messages_add(&json!({"role": "tool", "tool_call_id": id, "content": content}), false, false).await;
        }

        if api.cancel_requested() {
            ch.push(json!("toolcalling chain cancelled"), false).await;
            return;
        }

        let mut final_content: Vec<String> = Vec::new();
        let mut final_reasoning: Vec<String> = Vec::new();
        let mut had_recursive_call = false;

        if ch.context.is_over_threshold().await {
            ch.context.compress().await;
        }

        let ctx = match ch.context.get(GetOpts { end_prompt: false, ..Default::default() }).await {
            Ok(Some(c)) => c,
            Ok(None) => Vec::new(),
            Err(e) => {
                ch.log_error("Error while handling tool calls", &e.to_string());
                ch.push(json!(format!("Error while handling tool calls: {}", e)), false).await;
                return;
            }
        };
        let tools = ch.manager().tools();
        let mut stream = api.send_stream(ctx, SendOpts { tools: Some(tools), ..Default::default() });
        let mut cancelled = false;
        while let Some(token) = stream.next().await {
            if is_cancelled_token(&token) {
                cancelled = true;
                break;
            }
            let ttype = token.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
            match ttype.as_str() {
                "content" => {
                    final_content.push(token.get("content").map(py_str).unwrap_or_default());
                    yield token.clone();
                }
                "reasoning" => {
                    final_reasoning.push(token.get("content").map(py_str).unwrap_or_default());
                    yield token.clone();
                }
                "tool_call_delta" | "tool" | "tool_calls" | "prompt_progress" | "timings" | "model_load_progress" => {
                    yield token.clone();
                }
                "token_usage" => {
                    let usage = token.get("content").and_then(|v| v.as_i64()).unwrap_or(0);
                    if token.get("source").and_then(|s| s.as_str()) == Some("API") && usage > 0 {
                        ch.context.record_api_usage(usage).await;
                    }
                    yield token.clone();
                }
                "error" => {
                    // python ignores error tokens of the follow-up request (the stream just
                    // ends). we additionally log them so failures aren't fully silent.
                    let msg = token.get("content").map(py_str).unwrap_or_default();
                    ch.log("api", &msg);
                }
                _ => {}
            }
            if ttype == "tool_calls" {
                yield json!({"type": "token_usage", "content": ch.context.get_total_tokens().await, "source": "estimation"});
                had_recursive_call = true;
                let req = build_recursive_request(&ch, &token, &final_content, &final_reasoning);
                let mut sub = process(ch.clone(), req, push);
                while let Some(st) = sub.next().await {
                    if is_cancelled_token(&st) {
                        cancelled = true;
                        break;
                    }
                    yield st;
                }
                if cancelled {
                    break;
                }
            }
        }

        if cancelled {
            if !final_content.is_empty() || !final_reasoning.is_empty() {
                let mut msg = json!({"role": "assistant", "content": final_content.concat()});
                if !final_reasoning.is_empty() {
                    msg["reasoning_content"] = json!(final_reasoning.concat());
                }
                ch.chat.messages_add(&msg, false, false).await;
            }
            return;
        }

        if !had_recursive_call && (!final_content.is_empty() || !final_reasoning.is_empty()) {
            let mut msg = json!({"role": "assistant", "content": final_content.concat()});
            if !final_reasoning.is_empty() {
                msg["reasoning_content"] = json!(final_reasoning.concat());
            }
            yield json!({"type": "final", "content": msg});
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display() {
        let tc = json!({"function": {"name": "memory_create", "arguments": "{\"content\": \"a \\\"quoted\\\" thing that is quite long indeed\", \"n\": 1}"}});
        assert_eq!(display_call(&tc), "🔧 memory_create(content=\"a 'quoted' thing that is quite..\", n=\"1\")");
    }
}
