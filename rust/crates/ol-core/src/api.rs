//! Port of core/api.py: the OpenAI-compatible chat-completions client.
//!
//! Python used the `openai` SDK; this talks to the same HTTP API directly with reqwest
//! and reproduces the SDK's request shape (`extra_body` fields are merged into the top
//! level of the JSON body, exactly like the SDK does) and the token dicts the python
//! code yields while streaming (`content`, `reasoning`, `tool_call_delta`, `tool_calls`,
//! `token_usage`, `timings`, `prompt_progress`, `model_load_progress`, `error`).
//!
//! Cancellation: python raised `asyncio.CancelledError` out of the stream when
//! `cancel_request` was set. Here the stream yields a single [`CANCELLED`] sentinel
//! token instead, which callers handle the same way the python callers handled the
//! exception.

use crate::config;
use crate::pyjson;
use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type TokenStream = Pin<Box<dyn Stream<Item = Value> + Send>>;

/// sentinel token type yielded when a request is cancelled (python: CancelledError)
pub const CANCELLED: &str = "__cancelled__";

pub fn is_cancelled_token(v: &Value) -> bool {
    v.get("type").and_then(|t| t.as_str()) == Some(CANCELLED)
}

/// Simple class that holds an error message, used for passing on to channels
#[derive(Debug, Clone)]
pub struct ApiError {
    pub message: Option<String>,
    pub exc: Option<String>,
}

impl ApiError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: Some(message.into()), exc: None }
    }
    pub fn with_exc(message: Option<&str>, exc: impl Into<String>) -> Self {
        Self { message: message.map(|s| s.to_string()), exc: Some(exc.into()) }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = String::new();
        if let Some(m) = &self.message {
            s.push_str(m);
        }
        if let Some(e) = &self.exc {
            if self.message.is_some() {
                s.push_str(": ");
            }
            s.push_str(e);
        }
        f.write_str(&s)
    }
}
impl std::error::Error for ApiError {}

/// options for send()/send_stream() (python keyword arguments)
#[derive(Clone, Debug)]
pub struct SendOpts {
    pub use_tools: bool,
    /// explicit tools; None/empty means "use the active channel's tools"
    pub tools: Option<Vec<Value>>,
    pub use_thinking: bool,
    /// extra request fields (python **kwargs, e.g. max_completion_tokens)
    pub extra: Map<String, Value>,
}

impl Default for SendOpts {
    fn default() -> Self {
        Self { use_tools: true, tools: None, use_thinking: true, extra: Map::new() }
    }
}

/// classified HTTP failure, mirroring the openai SDK exception classes
#[derive(Debug)]
#[allow(dead_code)]
enum HttpFail {
    Connection(String),
    Status { code: u16, text: String },
    Other(String),
}

impl HttpFail {
    fn sdk_str(&self) -> String {
        match self {
            HttpFail::Connection(_) => "Connection error.".into(),
            HttpFail::Status { code, text } => format!("Error code: {} - {}", code, text),
            HttpFail::Other(s) => s.clone(),
        }
    }
}

fn status_body_text(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(v) => pyjson::py_str(&v),
        Err(_) => body.to_string(),
    }
}

/// find the end of the first complete SSE event in the buffer: (position, separator length)
fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2));
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| (p, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

pub struct ApiClient {
    manager: Mutex<Weak<crate::manager::Manager>>,
    connected: AtomicBool,
    client: Mutex<Option<reqwest::Client>>,
    is_streaming: AtomicBool,
    cancel_request: AtomicBool,
    supports_developer_role: AtomicBool,
    router_origin: Mutex<Option<String>>,
}

impl Default for ApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiClient {
    pub fn new() -> Self {
        Self {
            manager: Mutex::new(Weak::new()),
            connected: AtomicBool::new(false),
            client: Mutex::new(None),
            is_streaming: AtomicBool::new(false),
            cancel_request: AtomicBool::new(false),
            supports_developer_role: AtomicBool::new(false),
            router_origin: Mutex::new(None),
        }
    }

    pub fn set_manager(&self, m: Weak<crate::manager::Manager>) {
        *self.manager.lock() = m;
    }

    fn manager(&self) -> Option<Arc<crate::manager::Manager>> {
        self.manager.lock().upgrade()
    }

    fn log(&self, cat: &str, msg: &str) {
        match self.manager() {
            Some(m) => m.log(cat, msg),
            None => crate::log(cat, msg),
        }
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }
    pub fn supports_developer_role(&self) -> bool {
        self.supports_developer_role.load(Ordering::SeqCst)
    }
    pub fn is_streaming(&self) -> bool {
        self.is_streaming.load(Ordering::SeqCst)
    }
    pub fn cancel_requested(&self) -> bool {
        self.cancel_request.load(Ordering::SeqCst)
    }
    pub fn clear_cancel(&self) {
        self.cancel_request.store(false, Ordering::SeqCst)
    }
    pub fn request_cancel(&self) {
        self.cancel_request.store(true, Ordering::SeqCst)
    }

    fn base_url() -> String {
        config::get_str(&["api", "url"]).unwrap_or_default().trim_end_matches('/').to_string()
    }

    fn auth_header() -> Option<String> {
        let key = config::get_str(&["api", "key"]).unwrap_or_default();
        if key.is_empty() {
            None
        } else {
            Some(format!("Bearer {}", key))
        }
    }

    fn http(&self) -> Option<reqwest::Client> {
        self.client.lock().clone()
    }

    fn build_client() -> reqwest::Result<reqwest::Client> {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .danger_accept_invalid_certs(crate::globals::insecure_tls())
            .build()
    }

    async fn get_json(client: &reqwest::Client, url: &str, timeout: Option<Duration>) -> Result<Value, HttpFail> {
        let mut req = client.get(url);
        if let Some(a) = Self::auth_header() {
            req = req.header("Authorization", a);
        }
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                HttpFail::Connection(e.to_string())
            } else {
                HttpFail::Other(e.to_string())
            }
        })?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| HttpFail::Other(e.to_string()))?;
        if status >= 400 {
            return Err(HttpFail::Status { code: status, text: status_body_text(&text) });
        }
        serde_json::from_str(&text).map_err(|e| HttpFail::Other(e.to_string()))
    }

    /// connect to the API (validates by listing models). Ok(()) means connected.
    pub async fn connect(&self) -> Result<(), ApiError> {
        self.connect_opts(false).await
    }

    pub async fn connect_opts(&self, silent: bool) -> Result<(), ApiError> {
        if self.connected() {
            return Ok(());
        }
        let url = config::get_str(&["api", "url"]).unwrap_or_default();
        if url == "http://API_URL_HERE/v1" {
            return Err(ApiError::new("The API connection has not been set up yet! Please set up your API connection by either using the WebUI, the /config command, or editing the config file"));
        }
        if crate::globals::insecure_tls() {
            self.log("API", "WARNING: TLS certificate and hostname verification are disabled");
        }
        let client = match Self::build_client() {
            Ok(c) => c,
            Err(e) => return Err(ApiError::with_exc(None, e.to_string())),
        };
        *self.client.lock() = Some(client.clone());

        if let Err(f) = Self::get_json(&client, &format!("{}/models", Self::base_url()), None).await {
            return Err(match &f {
                HttpFail::Status { code: 400, text } => {
                    let l = text.to_lowercase();
                    if l.contains("model") && (l.contains("not found") || l.contains("missing")) {
                        ApiError::new("Model not found.")
                    } else {
                        ApiError::with_exc(Some("Bad request"), f.sdk_str())
                    }
                }
                HttpFail::Status { code: 401, .. } => {
                    ApiError::with_exc(Some("Authentication failed. Check if your API key is valid."), f.sdk_str())
                }
                HttpFail::Connection(_) => ApiError::with_exc(Some("Failed to connect to the API"), f.sdk_str()),
                _ => ApiError::with_exc(None, f.sdk_str()),
            });
        }

        self.connected.store(true, Ordering::SeqCst);
        self.supports_developer_role
            .store(config::get_bool(&["api", "use_developer_role"], false), Ordering::SeqCst);
        self.detect_router().await;
        if !silent {
            self.log("API", "Successfully connected to AI");
        }
        Ok(())
    }

    /// connect if disconnected, else Ok
    pub async fn attempt_connect(&self) -> Result<(), ApiError> {
        if !self.connected() {
            return self.connect().await;
        }
        Ok(())
    }

    pub fn get_status(&self) -> Value {
        json!({
            "connected": self.connected(),
            "url": config::get(&["api", "url"]).unwrap_or(Value::Null),
            "model": config::get(&["model", "name"]).unwrap_or(Value::Null),
        })
    }

    pub async fn disconnect(&self) {
        *self.client.lock() = None;
        self.connected.store(false, Ordering::SeqCst);
    }

    pub async fn reconnect(&self) -> Result<(), ApiError> {
        self.disconnect().await;
        self.connect().await
    }

    /// probe the API to detect a llama.cpp instance in router mode
    async fn detect_router(&self) {
        *self.router_origin.lock() = None;
        let Some(client) = self.http() else { return };
        let Ok(parsed) = url::Url::parse(&config::get_str(&["api", "url"]).unwrap_or_default()) else { return };
        let Some(host) = parsed.host_str() else { return };
        let origin = match parsed.port() {
            Some(p) => format!("{}://{}:{}", parsed.scheme(), host, p),
            None => format!("{}://{}", parsed.scheme(), host),
        };
        if let Ok(body) = Self::get_json(&client, &format!("{}/models", origin), Some(Duration::from_secs(5))).await {
            if let Some(entries) = body.get("data").and_then(|d| d.as_array()) {
                if entries.iter().any(|e| e.as_object().map(|o| o.contains_key("status")).unwrap_or(false)) {
                    *self.router_origin.lock() = Some(origin);
                }
            }
        }
    }

    /// reads the llamacpp router's /models/sse feed while a model loads
    async fn watch_model_load(client: reqwest::Client, origin: String, model_name: String, tx: tokio::sync::mpsc::UnboundedSender<Value>) {
        let mut req = client.get(format!("{}/models/sse", origin));
        if let Some(a) = Self::auth_header() {
            req = req.header("Authorization", a);
        }
        let Ok(resp) = req.send().await else { return };
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        while let Some(Ok(chunk)) = stream.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find("\n\n") {
                let record = buffer[..pos].trim().to_string();
                buffer = buffer[pos + 2..].to_string();
                let Some(data) = record.strip_prefix("data:") else { continue };
                let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
                let ev = event.get("event").and_then(|e| e.as_str()).unwrap_or("");
                if ev != "status_change" && ev != "model_status" {
                    continue;
                }
                let data = event.get("data").cloned().unwrap_or(json!({}));
                let status = data.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string();
                let name = event.get("model").and_then(|s| s.as_str()).unwrap_or("").to_string();
                if status == "loaded" || status == "unloaded" {
                    if name == model_name {
                        return;
                    }
                    continue;
                }
                if !matches!(status.as_str(), "loading" | "queued" | "downloading") {
                    continue;
                }
                let progress = data.get("progress").cloned().unwrap_or(json!({}));
                let percent = progress.get("value").and_then(|v| v.as_f64()).map(|v| json!((v * 100.0).round() as i64)).unwrap_or(Value::Null);
                let stage = progress.get("current").filter(|v| pyjson::truthy(v)).cloned().unwrap_or(json!(""));
                let _ = tx.send(json!({"model": name, "status": status, "stage": stage, "percent": percent}));
            }
        }
    }

    pub fn get_model(&self) -> Option<String> {
        config::get_str(&["model", "name"])
    }

    pub fn set_model(&self, name: &str) -> bool {
        let name = name.to_string();
        config::update(|c| {
            let model = c.entry("model".to_string()).or_insert_with(|| json!({}));
            if let Some(m) = model.as_object_mut() {
                m.insert("name".into(), Value::String(name));
            }
        });
        true
    }

    fn build_request(&self, context: &[Value], tools: Option<Vec<Value>>, stream: bool, use_thinking: bool, extra: &Map<String, Value>) -> Map<String, Value> {
        let mut tools = tools;
        if !config::get_bool(&["model", "use_tools"], false) {
            tools = None;
        }
        let mut req = Map::new();
        req.insert("model".into(), config::get(&["model", "name"]).unwrap_or(Value::Null));
        req.insert("messages".into(), Value::Array(context.to_vec()));
        if let Some(t) = tools {
            if !t.is_empty() {
                req.insert("tools".into(), Value::Array(t));
            }
        }
        req.insert("stream".into(), Value::Bool(stream));
        req.insert("temperature".into(), config::get(&["model", "temperature"]).unwrap_or(json!(0.7)));
        req.insert("max_completion_tokens".into(), config::get(&["api", "max_output_tokens"]).unwrap_or(json!(8192)));
        // extra_body (merged into the top level, as the openai SDK does)
        req.insert(
            "chat_template_kwargs".into(),
            json!({"enable_thinking": config::get(&["model", "enable_thinking"]).unwrap_or(Value::Bool(use_thinking))}),
        );
        req.insert("return_progress".into(), Value::Bool(true));
        for (k, v) in extra {
            if k == "tools" || k == "stream" || k == "use_thinking" {
                continue;
            }
            req.insert(k.clone(), v.clone());
        }
        let effort = config::get(&["model", "reasoning_effort"]).unwrap_or(Value::Null);
        if effort != json!("none") {
            req.insert("reasoning_effort".into(), effort);
        }
        if let Some(Value::Object(custom)) = config::get(&["api", "custom_fields"]) {
            for (k, v) in custom {
                req.insert(k, v);
            }
        }
        if stream {
            req.insert("stream_options".into(), json!({"include_usage": true}));
        }
        req
    }

    fn classify_request_error(f: HttpFail) -> ApiError {
        match &f {
            HttpFail::Status { code: 400, text } => {
                let l = text.to_lowercase();
                if l.contains("model") && (l.contains("not found") || l.contains("missing")) {
                    ApiError::with_exc(Some("Model with that name does not exist!"), f.sdk_str())
                } else {
                    ApiError::with_exc(Some("Bad request"), f.sdk_str())
                }
            }
            HttpFail::Status { code: 401, .. } => {
                ApiError::with_exc(Some("Authentication failed. Check whether your API key is valid!"), f.sdk_str())
            }
            HttpFail::Connection(_) => ApiError::new("Failed to connect to API"),
            HttpFail::Status { code: 404, .. } => ApiError::with_exc(Some("Model with that name does not exist!"), f.sdk_str()),
            HttpFail::Status { code: 429, .. } => ApiError::with_exc(Some("Rate limit exceeded"), f.sdk_str()),
            HttpFail::Status { .. } => ApiError::with_exc(Some("API Status Error"), f.sdk_str()),
            HttpFail::Other(s) => ApiError::with_exc(None, s.clone()),
        }
    }

    /// send the request. Err(None) means the request was cancelled.
    async fn request(&self, context: &[Value], tools: Option<Vec<Value>>, stream: bool, use_thinking: bool, extra: &Map<String, Value>) -> Result<reqwest::Response, Option<ApiError>> {
        if context.is_empty() {
            return Err(Some(ApiError::new("Tried to send a blank request for some reason! This should NEVER happen. Notify the developer.")));
        }
        self.attempt_connect().await.map_err(Some)?;
        let body = self.build_request(context, tools, stream, use_thinking, extra);
        let Some(client) = self.http() else { return Err(Some(ApiError::new("Failed to connect to API"))) };

        if self.cancel_requested() {
            self.clear_cancel();
            return Err(None);
        }

        let mut req = client.post(format!("{}/chat/completions", Self::base_url())).json(&Value::Object(body));
        if let Some(a) = Self::auth_header() {
            req = req.header("Authorization", a);
        }
        let fut = req.send();
        tokio::pin!(fut);
        let resp = loop {
            tokio::select! {
                r = &mut fut => break r,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if self.cancel_requested() {
                        self.clear_cancel();
                        return Err(None);
                    }
                }
            }
        };
        let result = match resp {
            Err(e) => {
                if e.is_connect() || e.is_timeout() {
                    Err(Some(Self::classify_request_error(HttpFail::Connection(e.to_string()))))
                } else {
                    Err(Some(Self::classify_request_error(HttpFail::Other(e.to_string()))))
                }
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status >= 400 {
                    let text = resp.text().await.unwrap_or_default();
                    Err(Some(Self::classify_request_error(HttpFail::Status { code: status, text: status_body_text(&text) })))
                } else {
                    Ok(resp)
                }
            }
        };
        if result.is_err() {
            self.clear_cancel();
        }
        result
    }

    fn active_tools(&self) -> Vec<Value> {
        self.manager().map(|m| m.tools()).unwrap_or_default()
    }

    /// send a message to the LLM (non-streaming). returns the assistant message dict
    pub async fn send(&self, context: &[Value], opts: SendOpts) -> Result<Value, ApiError> {
        self.clear_cancel();
        self.attempt_connect().await?;
        let tools = match opts.tools {
            Some(t) if !t.is_empty() => t,
            _ => self.active_tools(),
        };
        let resp = match self.request(context, if opts.use_tools { Some(tools) } else { None }, false, opts.use_thinking, &opts.extra).await {
            Ok(r) => r,
            Err(Some(e)) => return Err(e),
            Err(None) => return Err(ApiError::new("Request cancelled")),
        };
        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => return Err(ApiError::with_exc(Some("While processing response from AI"), e.to_string())),
        };
        self.recv(&body).map_err(|e| ApiError::with_exc(Some("While processing response from AI"), e))
    }

    /// extracts the message from a non-streaming response
    fn recv(&self, body: &Value) -> Result<Value, String> {
        let choice = body
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| format!("list index out of range ({})", pyjson::py_str(body)))?;
        let msg = choice.get("message").cloned().unwrap_or(json!({}));
        let reasoning = msg
            .get("reasoning_content")
            .filter(|v| pyjson::truthy(v))
            .or_else(|| msg.get("reasoning").filter(|v| pyjson::truthy(v)))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !reasoning.is_empty() && crate::globals::debug() {
            self.log("debug:reasoning", &reasoning);
        }
        let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mut tool_calls: Option<Vec<Value>> = None;
        if config::get_bool(&["model", "use_tools"], false) {
            if let Some(Value::Array(tcs)) = msg.get("tool_calls") {
                if !tcs.is_empty() {
                    tool_calls = Some(
                        tcs.iter()
                            .map(|tc| {
                                let f = tc.get("function").cloned().unwrap_or(json!({}));
                                json!({
                                    "id": tc.get("id").cloned().unwrap_or(Value::Null),
                                    "function": {
                                        "arguments": f.get("arguments").cloned().unwrap_or(json!("")),
                                        "name": f.get("name").cloned().unwrap_or(Value::Null),
                                    },
                                    "type": tc.get("type").cloned().unwrap_or(json!("function")),
                                })
                            })
                            .collect(),
                    );
                }
            }
        }
        let mut result = Map::new();
        if !content.is_empty() {
            result.insert("content".into(), Value::String(content));
        }
        if !reasoning.is_empty() {
            result.insert("reasoning_content".into(), Value::String(reasoning));
        }
        if let Some(tc) = tool_calls {
            result.insert("tool_calls".into(), Value::Array(tc));
        }
        result.insert("role".into(), json!("assistant"));
        Ok(Value::Object(result))
    }

    /// send a message to the LLM, streaming tokens back
    pub fn send_stream(self: &Arc<Self>, context: Vec<Value>, opts: SendOpts) -> TokenStream {
        let this = self.clone();
        Box::pin(async_stream::stream! {
            this.clear_cancel();
            if let Err(e) = this.attempt_connect().await {
                yield json!({"type": "error", "content": e.to_string()});
                return;
            }
            let tools = match opts.tools.clone() {
                Some(t) if !t.is_empty() => t,
                _ => this.active_tools(),
            };

            // against a llama.cpp router, the request blocks while a model is swapped in:
            // watch the load feed alongside it and yield model_load_progress tokens
            let router = this.router_origin.lock().clone();
            let (load_tx, mut load_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
            let load_task = match (router, this.http()) {
                (Some(origin), Some(client)) => {
                    let model = config::get_str(&["model", "name"]).unwrap_or_default();
                    Some(tokio::spawn(Self::watch_model_load(client, origin, model, load_tx)))
                }
                _ => { drop(load_tx); None }
            };

            let req_fut = this.request(&context, if opts.use_tools { Some(tools) } else { None }, true, opts.use_thinking, &opts.extra);
            tokio::pin!(req_fut);
            let response = loop {
                tokio::select! {
                    r = &mut req_fut => break r,
                    Some(progress) = load_rx.recv(), if load_task.is_some() => {
                        yield json!({"type": "model_load_progress", "content": progress});
                    }
                }
            };
            while let Ok(progress) = load_rx.try_recv() {
                yield json!({"type": "model_load_progress", "content": progress});
            }
            if let Some(t) = load_task { t.abort(); }

            let response = match response {
                Ok(r) => r,
                Err(Some(e)) => {
                    yield json!({"type": "error", "content": e.to_string()});
                    return;
                }
                Err(None) => {
                    yield json!({"type": CANCELLED});
                    return;
                }
            };

            this.is_streaming.store(true, Ordering::SeqCst);
            let mut inner = this.recv_stream(response);
            while let Some(token) = inner.next().await {
                if is_cancelled_token(&token) {
                    this.clear_cancel();
                    this.is_streaming.store(false, Ordering::SeqCst);
                    yield token;
                    return;
                }
                if this.cancel_requested() {
                    this.clear_cancel();
                    this.is_streaming.store(false, Ordering::SeqCst);
                    yield json!({"type": CANCELLED});
                    return;
                }
                if token.get("type").and_then(|t| t.as_str()) == Some("__stream_error__") {
                    let msg = token.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                    yield json!({"type": "error", "content": format!("While sending request to AI: {}", msg)});
                    this.is_streaming.store(false, Ordering::SeqCst);
                    return;
                }
                if crate::globals::debug_stream() {
                    this.log("debug:stream", &pyjson::dumps(&token));
                }
                yield token;
            }
            this.is_streaming.store(false, Ordering::SeqCst);
        })
    }

    /// parse the SSE stream into tokens (python _recv_stream). errors are yielded as
    /// `__stream_error__` tokens, cancellation as the CANCELLED sentinel.
    fn recv_stream(self: &Arc<Self>, response: reqwest::Response) -> TokenStream {
        let this = self.clone();
        Box::pin(async_stream::stream! {
            let use_tools = true;
            let mut tool_call_buffer: std::collections::BTreeMap<i64, Value> = Default::default();
            let mut total_prompt_tokens: i64 = 0;
            let mut total_completion_tokens: i64 = 0;
            let mut last_token_time: f64 = 0.0;
            let mut response_started = false;
            let mut bytes = response.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            'outer: loop {
                let chunk = match bytes.next().await {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => {
                        yield json!({"type": "__stream_error__", "content": e.to_string()});
                        return;
                    }
                    None => break,
                };
                buf.extend_from_slice(&chunk);
                // split complete SSE events
                loop {
                    let Some((pos, sep_len)) = find_event_end(&buf) else { break };
                    let event = String::from_utf8_lossy(&buf[..pos]).replace("\r\n", "\n");
                    buf.drain(..pos + sep_len);

                    let mut data_lines = Vec::new();
                    let mut event_name: Option<String> = None;
                    for line in event.lines() {
                        if let Some(d) = line.strip_prefix("data:") {
                            data_lines.push(d.strip_prefix(' ').unwrap_or(d).to_string());
                        } else if let Some(n) = line.strip_prefix("event:") {
                            event_name = Some(n.trim().to_string());
                        }
                    }
                    if data_lines.is_empty() {
                        continue;
                    }
                    let data = data_lines.join("\n");
                    if data.starts_with("[DONE]") {
                        break 'outer;
                    }
                    let Ok(chunk) = serde_json::from_str::<Value>(&data) else { continue };
                    if event_name.is_none() {
                        if let Some(err) = chunk.get("error") {
                            let msg = err.get("message").and_then(|m| m.as_str()).map(|s| s.to_string())
                                .unwrap_or_else(|| "An error occurred during streaming".to_string());
                            yield json!({"type": "__stream_error__", "content": msg});
                            return;
                        }
                    }

                    response_started = true;
                    if this.cancel_requested() {
                        yield json!({"type": CANCELLED});
                        return;
                    }

                    if let Some(pp) = chunk.get("prompt_progress") {
                        if !pp.is_null() {
                            yield json!({"type": "prompt_progress", "content": pp});
                        }
                    }

                    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
                    let delta_ms = (now - last_token_time) * 1000.0;
                    last_token_time = now;

                    if let Some(choice) = chunk.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()) {
                        let delta = choice.get("delta").cloned().unwrap_or(json!({}));
                        let content = delta.get("content").filter(|v| pyjson::truthy(v)).cloned();
                        let reason = delta.get("reasoning_content").filter(|v| pyjson::truthy(v)).cloned()
                            .or_else(|| delta.get("reasoning").filter(|v| pyjson::truthy(v)).cloned());

                        let mut content_yield: Option<Map<String, Value>> = None;
                        if let Some(c) = &content {
                            let mut m = Map::new();
                            m.insert("type".into(), json!("content"));
                            m.insert("content".into(), c.clone());
                            content_yield = Some(m);
                        }
                        if let Some(r) = &reason {
                            let mut m = Map::new();
                            m.insert("type".into(), json!("reasoning"));
                            m.insert("content".into(), r.clone());
                            content_yield = Some(m);
                        }
                        if content.is_some() || reason.is_some() {
                            if let Some(cy) = content_yield.as_mut() {
                                match chunk.get("timings") {
                                    Some(t) if pyjson::truthy(t) => { cy.insert("timings".into(), t.clone()); }
                                    _ => {
                                        if delta_ms > 1.0 {
                                            cy.insert("timings".into(), json!({"predicted_ms": delta_ms, "predicted_n": 1}));
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(cy) = content_yield {
                            yield Value::Object(cy);
                        }

                        if let (Some(Value::Array(tcs)), true) = (delta.get("tool_calls"), use_tools) {
                            for tc in tcs {
                                let index = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                                let f = tc.get("function").cloned().unwrap_or(json!({}));
                                if let Some(existing) = tool_call_buffer.get_mut(&index) {
                                    if let Some(id) = tc.get("id").filter(|v| pyjson::truthy(v)) {
                                        existing["id"] = id.clone();
                                    }
                                    if let Some(name) = f.get("name").filter(|v| pyjson::truthy(v)) {
                                        existing["function"]["name"] = name.clone();
                                    }
                                    if let Some(args) = f.get("arguments").and_then(|a| a.as_str()).filter(|s| !s.is_empty()) {
                                        let cur = existing["function"]["arguments"].as_str().unwrap_or("").to_string();
                                        existing["function"]["arguments"] = Value::String(cur + args);
                                        yield json!({"type": "tool_call_delta", "tool_calls": [existing.clone()]});
                                    }
                                } else {
                                    let entry = json!({
                                        "index": index,
                                        "id": tc.get("id").cloned().unwrap_or(Value::Null),
                                        "function": {
                                            "arguments": f.get("arguments").filter(|v| !v.is_null()).cloned().unwrap_or(json!("")),
                                            "name": f.get("name").cloned().unwrap_or(Value::Null),
                                        },
                                        "type": tc.get("type").cloned().unwrap_or(Value::Null),
                                    });
                                    tool_call_buffer.insert(index, entry.clone());
                                    yield json!({"type": "tool_call_delta", "tool_calls": [entry]});
                                }
                            }
                        }
                    }

                    if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
                        if let Some(p) = usage.get("prompt_tokens").and_then(|v| v.as_i64()) {
                            total_prompt_tokens = p;
                        }
                        if let Some(c) = usage.get("completion_tokens").and_then(|v| v.as_i64()) {
                            total_completion_tokens = c;
                        }
                        let mut token_usage = 0;
                        if total_prompt_tokens > 0 {
                            token_usage = total_prompt_tokens;
                        } else if total_completion_tokens > 0 {
                            token_usage = total_completion_tokens;
                        }
                        if token_usage > 0 {
                            yield json!({"type": "token_usage", "content": token_usage, "source": "API"});
                        }
                    }

                    if let Some(t) = chunk.get("timings") {
                        yield json!({"type": "timings", "content": t});
                    }
                }
            }

            if use_tools {
                let finals: Vec<Value> = tool_call_buffer
                    .values()
                    .filter(|tc| pyjson::truthy(&tc["function"]["name"]))
                    .cloned()
                    .collect();
                if !finals.is_empty() && config::get_bool(&["model", "use_tools"], false) {
                    yield json!({"type": "tool_calls", "tool_calls": finals});
                }
            }

            if !response_started {
                yield json!({"type": "__stream_error__", "content": "The API returned a blank response. This likely means an error happened on the API end, but that API failed to report the error. Check your AI server's logs if you're using local AI, or if not, contact your cloud AI service. Also, try turning off `Use Developer Role` in the api settings, as some models don't support it."});
            }
        })
    }

    /// cancel a request that's been sent to the AI
    pub async fn cancel(&self) -> bool {
        if !self.is_streaming() {
            return false;
        }
        self.request_cancel();
        let start = std::time::Instant::now();
        while self.cancel_requested() {
            if start.elapsed() > Duration::from_secs(30) {
                self.clear_cancel();
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }

    /// alphabetically sorted list of model ids
    pub async fn list_models(&self) -> Result<Vec<String>, ApiError> {
        if !self.connected() {
            self.attempt_connect().await?;
            // python returns the connect() result (True) here, which the webui
            // then sends as data; return an empty list on first connect
        }
        let Some(client) = self.http() else { return Ok(vec![]) };
        match Self::get_json(&client, &format!("{}/models", Self::base_url()), None).await {
            Ok(body) => {
                let mut ids: Vec<String> = body
                    .get("data")
                    .and_then(|d| d.as_array())
                    .map(|a| a.iter().filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                ids.sort();
                Ok(ids)
            }
            Err(e) => {
                self.log("error", &format!("error while retrieving model list: {}", e.sdk_str()));
                Ok(vec![])
            }
        }
    }
}
