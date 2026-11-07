//! Port of modules/mcp/module.py: connects to MCP (Model Context Protocol) servers over
//! stdio, streamable HTTP and legacy SSE, and exposes their tools to the AI as
//! `mcp_<server>_<tool>` catalog entries.
//!
//! The python module uses the `mcp` SDK. Here the JSON-RPC 2.0 client and the three
//! transports are hand-written on top of serde_json, tokio and reqwest. Behaviour that
//! the python module observes (config keys, server naming, tool naming, the connection
//! state machine, `/mcp` output, tool result text) is reproduced in this file.

#[path = "../../../generated/modules/mcp.rs"]
pub mod meta;

use async_trait::async_trait;
use indexmap::IndexMap;
use ol_core::channel::Channel;
use ol_core::manager::Manager;
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec};
use ol_core::pyjson::{self, py_str, py_str_opt, truthy};
use ol_core::tool_loader::CatalogEntry;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use std::process::Stdio;

/// protocol revision this client asks for (the server may answer with another one)
const PROTOCOL_VERSION: &str = "2025-06-18";
/// revisions the python SDK accepts in an initialize answer (mcp_types HANDSHAKE_PROTOCOL_VERSIONS)
const HANDSHAKE_PROTOCOL_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const SESSION_HEADER: &str = "Mcp-Session-Id";
const PROTOCOL_HEADER: &str = "MCP-Protocol-Version";
const TOOL_LIST_CHANGED: &str = "notifications/tools/list_changed";
/// how long a stdio server gets to exit after its stdin is closed (python: PROCESS_TERMINATION_TIMEOUT 2s)
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// how long a stdio server gets after SIGTERM before SIGKILL (python: FORCE_KILL_TIMEOUT 2s)
const FORCE_KILL_TIMEOUT: Duration = Duration::from_secs(2);
/// how long the kill is given to be observed (python: _KILL_REAP_TIMEOUT 2s)
const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);
/// how long a server task gets to unwind on shutdown before it is aborted (python: 10s)
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// environment variables the python SDK passes to stdio servers (default environment)
const INHERITED_ENV: [&str; 6] = ["HOME", "LOGNAME", "PATH", "SHELL", "TERM", "USER"];
/// hostname labels that say nothing about who runs the server
const GENERIC_HOST_LABELS: [&str; 6] = ["mcp", "api", "www", "app", "server", "gateway"];
/// package runners: for these, the package name is more telling than the runner
const RUNNERS: [&str; 9] = ["uvx", "npx", "pnpx", "bunx", "deno", "node", "python", "python3", "py"];

// ---------------------------------------------------------------------------
//   errors (python: the exception types that _flatten_error reads)
// ---------------------------------------------------------------------------

/// a failure talking to an MCP server
#[derive(Clone, Debug)]
pub enum McpError {
    /// JSON-RPC error response (python: MCPError with `.error`)
    Rpc { code: i64, message: String, data: Option<Value> },
    /// any other failure: python exception class name + message
    Other { kind: &'static str, message: String },
}

impl McpError {
    fn other(kind: &'static str, message: impl Into<String>) -> Self {
        McpError::Other { kind, message: message.into() }
    }

    /// python `str(e)`: the bare message (MCPError.__str__ returns the message)
    pub fn raw_message(&self) -> &str {
        match self {
            McpError::Rpc { message, .. } | McpError::Other { message, .. } => message,
        }
    }
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&flatten_error(self))
    }
}

/// python SDK `CONNECTION_CLOSED` (-32000), raised as MCPError for pending and post-close requests
fn connection_closed() -> McpError {
    McpError::Rpc { code: -32000, message: "Connection closed".into(), data: None }
}

/// python `_flatten_error` for the errors this client can produce
pub fn flatten_error(e: &McpError) -> String {
    match e {
        McpError::Rpc { code, message, data } => {
            let mut text = format!("MCPError [{}] {}", code, message).trim().to_string();
            if let Some(data) = data.as_ref().filter(|d| truthy(d)) {
                text.push_str(&format!(" | data: {}", py_str(data)));
            }
            text
        }
        McpError::Other { kind, message } => format!("{}: {}", kind, message),
    }
}

/// error text followed by its cause chain (python appends `<- cause` when it adds information)
fn chained(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let inner = cause.to_string();
        if !inner.is_empty() && !text.contains(&inner) {
            text.push_str(" <- ");
            text.push_str(&inner);
        }
        source = cause.source();
    }
    text
}

/// python exception class CPython raises for an OSError errno (OSError subclass by errno)
fn os_error_class(code: i32) -> &'static str {
    match code {
        libc::EPIPE | libc::ESHUTDOWN => "BrokenPipeError",
        libc::EAGAIN | libc::EALREADY | libc::EINPROGRESS => "BlockingIOError",
        libc::ECHILD => "ChildProcessError",
        libc::ECONNABORTED => "ConnectionAbortedError",
        libc::ECONNREFUSED => "ConnectionRefusedError",
        libc::ECONNRESET => "ConnectionResetError",
        libc::EEXIST => "FileExistsError",
        libc::ENOENT => "FileNotFoundError",
        libc::EISDIR => "IsADirectoryError",
        libc::ENOTDIR => "NotADirectoryError",
        libc::EINTR => "InterruptedError",
        libc::EACCES | libc::EPERM => "PermissionError",
        libc::ESRCH => "ProcessLookupError",
        libc::ETIMEDOUT => "TimeoutError",
        _ => "OSError",
    }
}

/// python `str(OSError)`: `[Errno N] strerror` (Rust's Display appends ` (os error N)`, which python does not)
fn errno_text(e: &std::io::Error, code: i32) -> String {
    let full = e.to_string();
    let suffix = format!(" (os error {})", code);
    let detail = full.strip_suffix(suffix.as_str()).unwrap_or(full.as_str());
    format!("[Errno {}] {}", code, detail)
}

fn io_error(e: &std::io::Error) -> McpError {
    match e.raw_os_error() {
        Some(code) => McpError::Other { kind: os_error_class(code), message: errno_text(e, code) },
        None => McpError::other("OSError", e.to_string()),
    }
}

/// python subprocess spawn failure: the OSError subclass for the errno, with the command as filename
fn spawn_error(e: &std::io::Error, command: &str) -> McpError {
    match e.raw_os_error() {
        Some(code) => McpError::Other {
            kind: os_error_class(code),
            message: format!("{}: '{}'", errno_text(e, code), command),
        },
        None => io_error(e),
    }
}

/// the innermost std::io::Error in a reqwest error's source chain
fn find_io_error<'a>(e: &'a (dyn std::error::Error + 'static)) -> Option<&'a std::io::Error> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = current {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return Some(io);
        }
        current = err.source();
    }
    None
}

/// python httpx errors: ConnectError carries the socket error text, timeouts are httpx's own classes
fn http_error(e: &reqwest::Error) -> McpError {
    if e.is_timeout() {
        let kind = if e.is_connect() { "ConnectTimeout" } else { "ReadTimeout" };
        return McpError::Other { kind, message: chained(e) };
    }
    if e.is_connect() {
        let os = find_io_error(e).and_then(|io| io.raw_os_error().map(|code| errno_text(io, code)));
        return McpError::Other { kind: "ConnectError", message: os.unwrap_or_else(|| chained(e)) };
    }
    McpError::Other { kind: "HTTPError", message: chained(e) }
}

/// python httpx `raise_for_status` message for a non-success response
fn status_error(status: reqwest::StatusCode, url: &str, location: Option<&str>) -> McpError {
    let code = status.as_u16();
    let class = match code / 100 {
        1 => "Informational response",
        3 => "Redirect response",
        4 => "Client error",
        5 => "Server error",
        _ => "Invalid status code",
    };
    let mut message = format!("{} '{} {}' for url '{}'", class, code, status.canonical_reason().unwrap_or(""), url);
    if matches!(code, 301 | 302 | 303 | 307 | 308) {
        if let Some(location) = location {
            message.push_str(&format!("\nRedirect location: '{}'", location));
        }
    }
    message.push_str(&format!("\nFor more information check: https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/{}", code));
    McpError::other("HTTPStatusError", message)
}

/// `status_error` for a response, using its final (normalized) URL like httpx does
fn response_status_error(resp: &reqwest::Response) -> McpError {
    let location = resp.headers().get(LOCATION).and_then(|v| v.to_str().ok());
    status_error(resp.status(), resp.url().as_str(), location)
}

/// python `Duration` for a float number of seconds (never panics on odd config values)
fn secs(t: f64) -> Duration {
    if t.is_finite() {
        Duration::from_secs_f64(t.clamp(0.0, 1.0e9))
    } else {
        Duration::from_secs(1_000_000_000)
    }
}

// ---------------------------------------------------------------------------
//   config parsing (pure helpers, python: _sanitize / _derive_* / _build_server_configs)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum ServerCfg {
    Stdio { command: String, args: Vec<String> },
    Http { url: String },
    Sse { url: String },
}

/// python `_sanitize`: reduce text to characters legal in tool names
fn sanitize(text: &str) -> String {
    let replaced: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let stripped = replaced.trim_matches(|c| c == '_' || c == '-');
    if stripped.is_empty() {
        "unnamed".to_string()
    } else {
        stripped.to_string()
    }
}

/// python `urllib.parse.urlparse(url).hostname`: the raw netloc host, lowercased, with no IDNA
/// encoding (so `bücher.de` keeps its unicode label)
fn url_hostname(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return String::new();
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostinfo = netloc.rsplit_once('@').map(|(_, host)| host).unwrap_or(netloc);
    let host = match hostinfo.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or(""),
        None => hostinfo.split(':').next().unwrap_or(""),
    };
    host.to_lowercase()
}

/// python `_derive_url_name`: mcp.exa.ai -> exa
fn derive_url_name(url: &str) -> String {
    let hostname = url_hostname(url);
    for label in hostname.split('.') {
        if !label.is_empty() && !GENERIC_HOST_LABELS.contains(&label.to_lowercase().as_str()) {
            return sanitize(label);
        }
    }
    "unnamed".to_string()
}

/// python `_derive_stdio_name`: 'uvx mcp-server-time' -> mcp-server-time
fn derive_stdio_name(argv: &[String]) -> String {
    let normalized = argv[0].replace('\\', "/");
    let command = normalized.rsplit('/').next().unwrap_or("");
    let args = &argv[1..];
    if RUNNERS.contains(&command) {
        if args.len() > 1 && (args[0] == "-m" || args[0] == "--module") {
            return sanitize(args[1].rsplit('.').next().unwrap_or(""));
        }
        for arg in args {
            if !arg.starts_with('-') {
                return sanitize(arg);
            }
        }
    }
    sanitize(command)
}

/// message python puts in `_parse_error` for a shlex failure (ValueError text).
/// Follows shlex's posix state machine: a backslash escapes the next char outside quotes and
/// inside double quotes (single quotes keep it literal); hitting EOF while escaping gives
/// "No escaped character", hitting EOF inside a quote gives "No closing quotation".
fn shlex_error(entry: &str) -> &'static str {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut quote = Quote::None;
    let mut chars = entry.chars();
    while let Some(c) = chars.next() {
        match quote {
            Quote::None => match c {
                '\\' => {
                    if chars.next().is_none() {
                        return "No escaped character";
                    }
                }
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                _ => {}
            },
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                }
            }
            Quote::Double => match c {
                '\\' => {
                    if chars.next().is_none() {
                        return "No escaped character";
                    }
                }
                '"' => quote = Quote::None,
                _ => {}
            },
        }
    }
    "No closing quotation"
}

/// parse one config entry: Ok(None) = skip, Err = invalid entry (python `_parse_error`)
fn parse_entry(setting: &str, entry: &str) -> Result<Option<(String, ServerCfg)>, String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Ok(None);
    }
    let transport = setting.to_lowercase();
    if transport == "stdio" {
        let Some(argv) = shlex::split(entry) else {
            return Err(format!("invalid command in '{}': '{}' ({})", setting, entry, shlex_error(entry)));
        };
        if argv.is_empty() {
            return Ok(None);
        }
        let name = derive_stdio_name(&argv);
        return Ok(Some((name, ServerCfg::Stdio { command: argv[0].clone(), args: argv[1..].to_vec() })));
    }
    let name = derive_url_name(entry);
    let cfg = if transport == "sse" {
        ServerCfg::Sse { url: entry.to_string() }
    } else {
        ServerCfg::Http { url: entry.to_string() }
    };
    Ok(Some((name, cfg)))
}

/// append _2, _3, ... until the server name no longer clashes
fn unique_server_name(configs: &IndexMap<String, ServerCfg>, name: String) -> String {
    let mut unique = name.clone();
    let mut counter = 2;
    while configs.contains_key(&unique) {
        unique = format!("{}_{}", name, counter);
        counter += 1;
    }
    unique
}

/// python `_tool_name`: unique, legal `mcp_<server>_<tool>` name (at most 64 characters)
fn unique_tool_name(used: &mut BTreeSet<String>, server: &str, tool: &str) -> String {
    let full: String = format!("mcp_{}_{}", sanitize(server), sanitize(tool)).chars().take(64).collect();
    let mut name = full.trim_end_matches(|c| c == '_' || c == '-').to_string();
    let base = name.clone();
    let mut counter = 2;
    while used.contains(&name) {
        let head: String = base.chars().take(61).collect();
        name = format!("{}_{}", head, counter);
        counter += 1;
    }
    used.insert(name.clone());
    name
}

// ---------------------------------------------------------------------------
//   tool results (python: the block loop inside `call`)
// ---------------------------------------------------------------------------

/// turn a CallToolResult into the plain text the AI sees
pub fn call_result_text(result: &Value) -> String {
    let blocks: &[Value] = result.get("content").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        let ty = block.get("type").and_then(Value::as_str).unwrap_or("");
        let part = match ty {
            "text" => block.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
            "image" | "audio" => format!("[{} content omitted: {}]", ty, py_str_opt(block.get("mimeType"))),
            "resource" => {
                let resource = block.get("resource");
                match resource.and_then(|r| r.get("text")).filter(|t| truthy(t)) {
                    Some(text) => py_str(text),
                    None => format!("[binary resource: {}]", py_str_opt(resource.and_then(|r| r.get("uri")))),
                }
            }
            "resource_link" => format!("[resource link: {}]", py_str_opt(block.get("uri"))),
            other => format!("[unsupported content: {}]", other),
        };
        parts.push(part);
    }
    // no text at all: fall back to structured output, if any
    if !parts.iter().any(|p| !p.trim().is_empty()) {
        return match result.get("structuredContent") {
            Some(v) if !v.is_null() => pyjson::dumps(v),
            _ => "(empty result)".to_string(),
        };
    }
    parts.join("\n")
}

// ---------------------------------------------------------------------------
//   SSE decoding (legacy SSE and streamable HTTP responses)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// incremental decoder for `text/event-stream` bodies
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl SseDecoder {
    /// feed raw bytes; returns every event completed by them
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
            let line = text.strip_suffix('\r').unwrap_or(text.as_str()).to_string();
            if let Some(ev) = self.feed_line(&line) {
                out.push(ev);
            }
        }
        out
    }

    fn feed_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            let event = std::mem::take(&mut self.event);
            let data = std::mem::take(&mut self.data);
            if data.is_empty() {
                return None;
            }
            let event = if event.is_empty() { "message".to_string() } else { event };
            return Some(SseEvent { event, data: data.join("\n") });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }
}

/// forward every `message` event of an SSE response body to the session
async fn forward_sse(mut resp: reqwest::Response, tx: mpsc::UnboundedSender<Value>) {
    let mut decoder = SseDecoder::default();
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(chunk)) => chunk,
            _ => break,
        };
        for ev in decoder.push(&chunk) {
            if ev.event != "message" {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<Value>(&ev.data) {
                if tx.send(msg).is_err() {
                    return;
                }
            }
        }
    }
}

fn resolve_endpoint(base: &str, endpoint: &str) -> Result<String, McpError> {
    let base = url::Url::parse(base).map_err(|e| McpError::other("ValueError", e.to_string()))?;
    base.join(endpoint.trim()).map(|u| u.to_string()).map_err(|e| McpError::other("ValueError", e.to_string()))
}

// ---------------------------------------------------------------------------
//   transports
// ---------------------------------------------------------------------------

/// tasks owned by a transport; aborted on close and on drop
#[derive(Default)]
struct Tasks(Mutex<Vec<JoinHandle<()>>>);

impl Tasks {
    fn add(&self, handle: JoinHandle<()>) {
        let mut tasks = self.0.lock();
        tasks.retain(|h| !h.is_finished());
        tasks.push(handle);
    }

    fn abort_all(&self) {
        for handle in self.0.lock().drain(..) {
            handle.abort();
        }
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        self.abort_all();
    }
}

/// send a signal to a stdio server's process group (the group id is the leader's pid)
fn signal_group(pgid: Option<libc::pid_t>, signal: libc::c_int) {
    if let Some(pgid) = pgid {
        // SAFETY: killpg only sends a signal; a stale group id just fails with ESRCH
        unsafe {
            libc::killpg(pgid, signal);
        }
    }
}

/// stdio: newline-delimited JSON-RPC over a child process's stdin/stdout
pub struct StdioTransport {
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    child: tokio::sync::Mutex<Child>,
    tasks: Tasks,
}

impl StdioTransport {
    fn spawn(command: &str, args: &[String], tx: mpsc::UnboundedSender<Value>) -> Result<Self, McpError> {
        let mut cmd = Command::new(command);
        cmd.args(args).env_clear();
        for var in INHERITED_ENV {
            if let Ok(value) = std::env::var(var) {
                cmd.env(var, value);
            }
        }
        // own process group, so shutdown can signal the whole tree (python: start_new_session=True)
        cmd.process_group(0);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| spawn_error(&e, command))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| McpError::other("OSError", "server stdout is not available"))?;
        let tasks = Tasks::default();
        tasks.add(tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(msg) = serde_json::from_str::<Value>(line) {
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
            }
        }));
        Ok(Self { stdin: tokio::sync::Mutex::new(stdin), child: tokio::sync::Mutex::new(child), tasks })
    }

    async fn send(&self, msg: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(msg).map_err(|e| McpError::other("TypeError", e.to_string()))?;
        line.push('\n');
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(connection_closed());
        };
        stdin.write_all(line.as_bytes()).await.map_err(|e| io_error(&e))?;
        stdin.flush().await.map_err(|e| io_error(&e))
    }

    /// python stdio_client shutdown: close stdin, wait out the grace period, SIGTERM the process
    /// group, wait again, then SIGKILL the group (python terminate_posix_process_tree)
    async fn close(&self) {
        drop(self.stdin.lock().await.take());
        let mut child = self.child.lock().await;
        let pgid = child.id().map(|pid| pid as libc::pid_t);
        if tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await.is_err() {
            signal_group(pgid, libc::SIGTERM);
            if tokio::time::timeout(FORCE_KILL_TIMEOUT, child.wait()).await.is_err() {
                signal_group(pgid, libc::SIGKILL);
                if tokio::time::timeout(KILL_REAP_TIMEOUT, child.wait()).await.is_err() {
                    let _ = child.kill().await;
                }
            }
        }
        self.tasks.abort_all();
    }
}

#[derive(Default)]
struct HttpState {
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
}

impl HttpState {
    fn apply(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(id) = self.session_id.lock().clone() {
            req = req.header(SESSION_HEADER, id);
        }
        if let Some(version) = self.protocol_version.lock().clone() {
            req = req.header(PROTOCOL_HEADER, version);
        }
        req
    }
}

/// streamable HTTP: JSON-RPC POSTs, answered with JSON or an SSE stream; optional GET stream
pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    state: Arc<HttpState>,
    tx: mpsc::UnboundedSender<Value>,
    tasks: Tasks,
}

impl HttpTransport {
    fn new(url: &str, tx: mpsc::UnboundedSender<Value>) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: url.to_string(),
            state: Arc::new(HttpState::default()),
            tx,
            tasks: Tasks::default(),
        }
    }

    async fn post(&self, msg: &Value) -> Result<(), McpError> {
        let req = self
            .state
            .apply(self.client.post(&self.url))
            .header(ACCEPT, "application/json, text/event-stream")
            .json(msg);
        let resp = req.send().await.map_err(|e| http_error(&e))?;
        if let Some(id) = resp.headers().get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
            *self.state.session_id.lock() = Some(id.to_string());
        }
        if !resp.status().is_success() {
            return Err(response_status_error(&resp));
        }
        let content_type = resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
        if content_type.starts_with("text/event-stream") {
            self.tasks.add(tokio::spawn(forward_sse(resp, self.tx.clone())));
            return Ok(());
        }
        if content_type.starts_with("application/json") {
            let body: Value = resp.json().await.map_err(|e| http_error(&e))?;
            let items = match body {
                Value::Array(items) => items,
                single => vec![single],
            };
            for item in items {
                let _ = self.tx.send(item);
            }
        }
        // 202 Accepted (notifications) has no body
        Ok(())
    }

    /// standing GET stream for server-initiated messages (absent servers answer 405)
    fn open_server_stream(&self) {
        let client = self.client.clone();
        let url = self.url.clone();
        let state = self.state.clone();
        let tx = self.tx.clone();
        self.tasks.add(tokio::spawn(async move {
            let req = state.apply(client.get(&url)).header(ACCEPT, "text/event-stream");
            let Ok(resp) = req.send().await else { return };
            if !resp.status().is_success() {
                return;
            }
            forward_sse(resp, tx).await;
        }));
    }

    async fn close(&self) {
        let session = self.state.session_id.lock().clone();
        if session.is_some() {
            let req = self.state.apply(self.client.delete(&self.url));
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, req.send()).await;
        }
        self.tasks.abort_all();
    }
}

/// legacy SSE: GET stream announces the POST endpoint, responses arrive on the stream
pub struct SseTransport {
    client: reqwest::Client,
    endpoint: String,
    tasks: Tasks,
}

impl SseTransport {
    async fn connect(url: &str, tx: mpsc::UnboundedSender<Value>) -> Result<Self, McpError> {
        let client = reqwest::Client::new();
        let resp = client
            .get(url)
            .header(ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|e| http_error(&e))?;
        if !resp.status().is_success() {
            return Err(response_status_error(&resp));
        }
        let (endpoint_tx, endpoint_rx) = oneshot::channel::<Result<String, McpError>>();
        let base = url.to_string();
        let tasks = Tasks::default();
        tasks.add(tokio::spawn(async move {
            let mut endpoint_tx = Some(endpoint_tx);
            let mut decoder = SseDecoder::default();
            let mut resp = resp;
            loop {
                let chunk = match resp.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    _ => break,
                };
                for ev in decoder.push(&chunk) {
                    match ev.event.as_str() {
                        "endpoint" => {
                            if let Some(waiter) = endpoint_tx.take() {
                                let _ = waiter.send(resolve_endpoint(&base, &ev.data));
                            }
                        }
                        "message" => {
                            if let Ok(msg) = serde_json::from_str::<Value>(&ev.data) {
                                if tx.send(msg).is_err() {
                                    return;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }));
        let endpoint = match endpoint_rx.await {
            Ok(Ok(endpoint)) => endpoint,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(McpError::other("McpError", "SSE stream closed before the endpoint event")),
        };
        Ok(Self { client, endpoint, tasks })
    }

    async fn post(&self, msg: &Value) -> Result<(), McpError> {
        let resp = self
            .client
            .post(&self.endpoint)
            .json(msg)
            .send()
            .await
            .map_err(|e| http_error(&e))?;
        if !resp.status().is_success() {
            return Err(response_status_error(&resp));
        }
        Ok(())
    }
}

pub enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
    Sse(SseTransport),
}

impl Transport {
    async fn send(&self, msg: &Value) -> Result<(), McpError> {
        match self {
            Transport::Stdio(t) => t.send(msg).await,
            Transport::Http(t) => t.post(msg).await,
            Transport::Sse(t) => t.post(msg).await,
        }
    }

    fn on_negotiated(&self, version: &str) {
        if let Transport::Http(t) = self {
            *t.state.protocol_version.lock() = Some(version.to_string());
        }
    }

    fn open_server_stream(&self) {
        if let Transport::Http(t) = self {
            t.open_server_stream();
        }
    }

    async fn close(&self) {
        match self {
            Transport::Stdio(t) => t.close().await,
            Transport::Http(t) => t.close().await,
            Transport::Sse(t) => t.tasks.abort_all(),
        }
    }
}

// ---------------------------------------------------------------------------
//   JSON-RPC session
// ---------------------------------------------------------------------------

/// called with the method of every server notification
pub type NotifyFn = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct Pending {
    calls: HashMap<i64, oneshot::Sender<Result<Value, McpError>>>,
    closed: bool,
}

pub struct Session {
    transport: Arc<Transport>,
    pending: Arc<Mutex<Pending>>,
    next_id: AtomicI64,
    reader: JoinHandle<()>,
}

impl Session {
    /// open the transport for a server config and run the initialize handshake
    async fn connect(cfg: &ServerCfg, on_notify: NotifyFn) -> Result<Arc<Session>, McpError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let transport = match cfg {
            ServerCfg::Stdio { command, args } => Transport::Stdio(StdioTransport::spawn(command, args, tx)?),
            ServerCfg::Http { url } => Transport::Http(HttpTransport::new(url, tx)),
            ServerCfg::Sse { url } => Transport::Sse(SseTransport::connect(url, tx).await?),
        };
        let transport = Arc::new(transport);
        let pending = Arc::new(Mutex::new(Pending::default()));
        let reader = tokio::spawn(dispatch_loop(rx, Arc::downgrade(&transport), pending.clone(), on_notify));
        let session = Arc::new(Session { transport, pending, next_id: AtomicI64::new(0), reader });

        // a failed handshake tears the transport down the way the python context exit does
        match session.handshake().await {
            Ok(()) => Ok(session),
            Err(e) => {
                session.close().await;
                Err(e)
            }
        }
    }

    /// python `ClientSession.initialize` plus the stream the SDK opens afterwards
    async fn handshake(&self) -> Result<(), McpError> {
        let init = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "mcp", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await?;
        let negotiated = negotiated_version(&init)?;
        self.transport.on_negotiated(&negotiated);
        self.notify("notifications/initialized", json!({})).await?;
        self.transport.open_server_stream();
        Ok(())
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (waiter, answer) = oneshot::channel();
        {
            let mut pending = self.pending.lock();
            if pending.closed {
                return Err(connection_closed());
            }
            pending.calls.insert(id, waiter);
        }
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.transport.send(&msg).await {
            self.pending.lock().calls.remove(&id);
            return Err(e);
        }
        match answer.await {
            Ok(result) => result,
            Err(_) => Err(connection_closed()),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.transport.send(&json!({"jsonrpc": "2.0", "method": method, "params": params})).await
    }

    /// fail every waiting request, then tear the transport down
    async fn close(&self) {
        {
            let mut pending = self.pending.lock();
            pending.closed = true;
            for (_, waiter) in pending.calls.drain() {
                let _ = waiter.send(Err(connection_closed()));
            }
        }
        self.transport.close().await;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// the protocol revision of an initialize answer (python `ClientSession.initialize` checks it
/// against HANDSHAKE_PROTOCOL_VERSIONS and raises RuntimeError when it is not one of them)
fn negotiated_version(init: &Value) -> Result<String, McpError> {
    let Some(version) = init.get("protocolVersion").and_then(Value::as_str) else {
        return Err(McpError::other(
            "ValidationError",
            "1 validation error for InitializeResult\nprotocolVersion\n  Field required",
        ));
    };
    if HANDSHAKE_PROTOCOL_VERSIONS.contains(&version) {
        Ok(version.to_string())
    } else {
        Err(McpError::other(
            "RuntimeError",
            format!("Unsupported protocol version from the server: {}", version),
        ))
    }
}

async fn dispatch_loop(
    mut rx: mpsc::UnboundedReceiver<Value>,
    transport: Weak<Transport>,
    pending: Arc<Mutex<Pending>>,
    on_notify: NotifyFn,
) {
    while let Some(msg) = rx.recv().await {
        route_message(msg, &transport, &pending, &on_notify);
    }
    let mut guard = pending.lock();
    guard.closed = true;
    for (_, waiter) in guard.calls.drain() {
        let _ = waiter.send(Err(connection_closed()));
    }
}

fn route_message(msg: Value, transport: &Weak<Transport>, pending: &Mutex<Pending>, on_notify: &NotifyFn) {
    let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
    let id = msg.get("id").filter(|v| !v.is_null()).cloned();
    match (method, id) {
        (Some(method), Some(id)) => {
            // server -> client request: answer ping, refuse everything else
            let reply = if method == "ping" {
                json!({"jsonrpc": "2.0", "id": id, "result": {}})
            } else {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})
            };
            if let Some(t) = transport.upgrade() {
                tokio::spawn(async move {
                    let _ = t.send(&reply).await;
                });
            }
        }
        (Some(method), None) => (**on_notify)(&method),
        (None, Some(id)) => {
            let Some(key) = id.as_i64() else { return };
            let waiter = pending.lock().calls.remove(&key);
            if let Some(waiter) = waiter {
                let result = match msg.get("error").filter(|e| !e.is_null()) {
                    Some(err) => Err(McpError::Rpc {
                        code: err.get("code").and_then(pyjson::as_i64).unwrap_or(0),
                        message: err.get("message").map(py_str).unwrap_or_default(),
                        data: err.get("data").cloned(),
                    }),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = waiter.send(result);
            }
        }
        (None, None) => {}
    }
}

// ---------------------------------------------------------------------------
//   module state
// ---------------------------------------------------------------------------

/// where an injected tool dispatches to (python: the `call` closure set as an attribute)
#[derive(Clone, Debug)]
struct Dispatch {
    server: String,
    remote: String,
}

#[derive(Debug)]
struct ServerState {
    /// connecting / connected / timeout / error / stopped
    state: &'static str,
    /// catalog names of the tools currently injected
    tools: Vec<String>,
    /// last error, flattened to readable text
    error: Option<String>,
}

/// one configured server (python: the `srv` record)
struct Server {
    cfg: ServerCfg,
    task: Mutex<Option<JoinHandle<()>>>,
    session: Mutex<Option<Arc<Session>>>,
    stop: watch::Sender<bool>,
    ready: watch::Sender<bool>,
    state: Mutex<ServerState>,
}

impl Server {
    fn new(cfg: ServerCfg) -> Self {
        Self {
            cfg,
            task: Mutex::new(None),
            session: Mutex::new(None),
            stop: watch::channel(false).0,
            ready: watch::channel(false).0,
            state: Mutex::new(ServerState { state: "connecting", tools: Vec::new(), error: None }),
        }
    }
}

/// shared state reachable from the per-server connection tasks
struct Inner {
    name: String,
    manager: Weak<Manager>,
    servers: Mutex<IndexMap<String, Arc<Server>>>,
    parse_error: Mutex<Option<String>>,
    used_names: Mutex<BTreeSet<String>>,
    dispatch: Mutex<HashMap<String, Dispatch>>,
}

impl Inner {
    fn log(&self, message: &str) {
        match self.manager.upgrade() {
            Some(m) => m.log(&self.name, message),
            None => ol_core::log(&self.name, message),
        }
    }

    /// every channel's tool loader (python: `_loaders`)
    fn channels(&self) -> Vec<Arc<Channel>> {
        self.manager.upgrade().map(|m| m.channels().values().cloned().collect()).unwrap_or_default()
    }

    fn server(&self, name: &str) -> Option<Arc<Server>> {
        self.servers.lock().get(name).cloned()
    }

    fn snapshot(&self) -> Vec<(String, Arc<Server>)> {
        self.servers.lock().iter().map(|(n, s)| (n.clone(), s.clone())).collect()
    }

    /// python `_purge_server`: remove every injected tool belonging to a server
    fn purge_server(&self, name: &str) {
        let names: Vec<String> = match self.server(name) {
            Some(srv) => std::mem::take(&mut srv.state.lock().tools),
            None => Vec::new(),
        };
        {
            let mut used = self.used_names.lock();
            for n in &names {
                used.remove(n);
            }
        }
        {
            let mut dispatch = self.dispatch.lock();
            for n in &names {
                dispatch.remove(n);
            }
        }
        for ch in self.channels() {
            ch.tool_loader.remove_tools(&names);
        }
    }
}

/// python `_connection_lifecycle` body: connect, serve until stop, then unwind
async fn connection_lifecycle(inner: Arc<Inner>, name: String, srv: Arc<Server>) {
    let mut stop = srv.stop.subscribe();

    // servers may announce that their tool list changed: re-sync when they do
    let notify_inner = inner.clone();
    let notify_name = name.clone();
    let on_notify: NotifyFn = Arc::new(move |method: &str| {
        if method == TOOL_LIST_CHANGED {
            notify_inner.log(&format!("server '{}' announced a tool list change", notify_name));
            let inner = notify_inner.clone();
            let name = notify_name.clone();
            tokio::spawn(async move { resync_tools(&inner, &name).await });
        }
    });

    let outcome: Result<(), McpError> = match Session::connect(&srv.cfg, on_notify).await {
        Err(e) => Err(e),
        Ok(session) => {
            *srv.session.lock() = Some(session);
            resync_tools(&inner, &name).await;
            {
                let mut st = srv.state.lock();
                st.state = "connected";
                st.error = None;
            }
            let count = srv.state.lock().tools.len();
            inner.log(&format!("server '{}' connected ({} tools)", name, count));
            srv.ready.send_replace(true);
            // stay connected until shutdown is requested
            let _ = stop.wait_for(|stopped| *stopped).await;
            Ok(())
        }
    };

    if let Err(e) = &outcome {
        {
            let mut st = srv.state.lock();
            if st.state != "timeout" {
                st.state = "error";
                st.error = Some(flatten_error(e));
            }
        }
        // python logs core.detail_error(e), which is str(e) (the bare message) without debug
        inner.log(&format!("server '{}' stopped: {}", name, ol_core::detail_error(e.raw_message())));
    }

    // always release the startup wait, even on early failure
    let session = srv.session.lock().take();
    if let Some(session) = session {
        session.close().await;
    }
    srv.ready.send_replace(true);
    let mut st = srv.state.lock();
    if st.state != "error" && st.state != "timeout" {
        st.state = "stopped";
        st.tools.clear();
    }
}

/// python `_resync_tools`: fetch the server's tool list and (re)inject all of it
async fn resync_tools(inner: &Inner, name: &str) {
    let Some(srv) = inner.server(name) else { return };
    let session = srv.session.lock().clone();
    let Some(session) = session else { return };
    let listing = match session.request("tools/list", json!({})).await {
        Ok(listing) => listing,
        Err(e) => {
            inner.log(&format!("could not list tools for '{}': {}", name, flatten_error(&e)));
            return;
        }
    };

    let channels = inner.channels();
    // remember which tools chats currently have loaded, to restore that
    let was_active: BTreeSet<String> = channels.iter().flat_map(|ch| ch.tool_loader.active_names()).collect();
    inner.purge_server(name);

    if let Some(Value::Array(tools)) = listing.get("tools") {
        for tool in tools {
            inject_tool(inner, name, &srv, tool, &was_active, &channels);
        }
    }
    srv.state.lock().tools.sort();

    // keep the tools_load meta tool's module description in sync
    for ch in &channels {
        ch.tool_loader.refresh_meta_tool_descriptions();
    }
}

/// python `_inject_tool`: register one MCP tool in every channel's catalog
fn inject_tool(
    inner: &Inner,
    server: &str,
    srv: &Server,
    tool: &Value,
    was_active: &BTreeSet<String>,
    channels: &[Arc<Channel>],
) {
    let Some(remote) = tool.get("name").and_then(Value::as_str) else { return };
    let tool_name = unique_tool_name(&mut inner.used_names.lock(), server, remote);
    let description = match tool.get("description").filter(|d| truthy(d)) {
        Some(d) => py_str(d),
        None => format!("MCP tool '{}' from server '{}'.", remote, server),
    };
    // the server provides a ready-made JSON Schema for its input
    let schema = match tool.get("inputSchema").filter(|s| truthy(s)) {
        Some(s) => s.clone(),
        None => json!({"type": "object", "properties": {}}),
    };
    let definition = json!({
        "type": "function",
        "function": {"name": tool_name, "description": description, "parameters": schema},
    });

    inner.dispatch.lock().insert(tool_name.clone(), Dispatch { server: server.to_string(), remote: remote.to_string() });
    for ch in channels {
        let entry = CatalogEntry {
            tool: definition.clone(),
            module: inner.name.clone(),
            method: tool_name.clone(),
            description: description.clone(),
        };
        // if a chat had this tool loaded before the resync, load it again
        ch.tool_loader.insert_catalog_entry(&tool_name, entry, was_active.contains(&tool_name));
    }
    srv.state.lock().tools.push(tool_name);
}

pub struct Mcp {
    base: ModuleBase,
    inner: Arc<Inner>,
}

impl Mcp {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        let manager = base.manager().map(|m| Arc::downgrade(&m)).unwrap_or_default();
        let inner = Arc::new(Inner {
            name: base.name.clone(),
            manager,
            servers: Mutex::new(IndexMap::new()),
            parse_error: Mutex::new(None),
            used_names: Mutex::new(BTreeSet::new()),
            dispatch: Mutex::new(HashMap::new()),
        });
        Self { base, inner }
    }

    /// python `self.config.get(setting, default=[])`, iterated like a python list
    fn config_entries(&self, setting: &str) -> Vec<String> {
        match self.base.config.get1(setting) {
            Some(Value::Array(items)) => items.iter().map(py_str).collect(),
            Some(Value::String(s)) => s.chars().map(|c| c.to_string()).collect(),
            Some(Value::Object(o)) => o.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }

    /// python `_build_server_configs`: {unique name: cfg}; bad entries set the parse error
    fn build_server_configs(&self) -> IndexMap<String, ServerCfg> {
        *self.inner.parse_error.lock() = None;
        let mut configs = IndexMap::new();
        for setting in ["http", "SSE", "stdio"] {
            for entry in self.config_entries(setting) {
                match parse_entry(setting, &entry) {
                    Err(message) => {
                        *self.inner.parse_error.lock() = Some(message.clone());
                        self.inner.log(&message);
                    }
                    Ok(None) => {}
                    Ok(Some((name, cfg))) => {
                        let unique = unique_server_name(&configs, name);
                        configs.insert(unique, cfg);
                    }
                }
            }
        }
        configs
    }

    /// python `_start_servers`
    async fn start_servers(&self) {
        let configs = self.build_server_configs();
        for (name, cfg) in configs {
            let srv = Arc::new(Server::new(cfg));
            let task = tokio::spawn(connection_lifecycle(self.inner.clone(), name.clone(), srv.clone()));
            *srv.task.lock() = Some(task);
            self.inner.servers.lock().insert(name, srv);
        }

        // then wait for them to settle
        let timeout = self.base.config.get_f64("connect_timeout").unwrap_or(30.0);
        for (name, srv) in self.inner.snapshot() {
            let mut ready = srv.ready.subscribe();
            if tokio::time::timeout(secs(timeout), ready.wait_for(|up| *up)).await.is_err() {
                {
                    let mut st = srv.state.lock();
                    st.state = "timeout";
                    st.error = Some(format!("did not connect within {}s", pyjson::float_repr(timeout)));
                }
                self.inner.log(&format!("server '{}' timed out while connecting", name));
            }
        }
    }

    /// python `_stop_servers`: signal every task, give it time, then force
    async fn stop_servers(&self) {
        let servers = self.inner.snapshot();
        for (_, srv) in &servers {
            srv.stop.send_replace(true);
        }
        for (_, srv) in &servers {
            let task = srv.task.lock().take();
            if let Some(mut task) = task {
                let finished = matches!(tokio::time::timeout(STOP_TIMEOUT, &mut task).await, Ok(Ok(())));
                if !finished {
                    task.abort();
                }
            }
            let session = srv.session.lock().take();
            if let Some(session) = session {
                session.close().await;
            }
        }
        for (name, _) in &servers {
            self.inner.purge_server(name);
        }
        self.inner.servers.lock().clear();
    }

    /// python `_inject`-side dispatch for one injected tool
    async fn call_remote(&self, dispatch: &Dispatch, arguments: Map<String, Value>) -> Value {
        let session = self.inner.server(&dispatch.server).and_then(|srv| {
            let session = srv.session.lock().clone();
            session
        });
        let Some(session) = session else {
            return self.base.result(format!("MCP server '{}' is not connected. Check /mcp.", dispatch.server), false);
        };
        match session.request("tools/call", json!({"name": dispatch.remote, "arguments": Value::Object(arguments)})).await {
            Ok(result) => {
                let is_error = result.get("isError").map(truthy).unwrap_or(false);
                self.base.result(call_result_text(&result), !is_error)
            }
            Err(e) => self.base.result(format!("MCP call '{}' failed: {}", dispatch.remote, flatten_error(&e)), false),
        }
    }

    /// python `mcp_command`
    async fn mcp_command(&self, args: &[String]) -> String {
        let reload = args.first().map(|a| a.to_lowercase() == "reload").unwrap_or(false);
        let mut lines: Vec<String> = Vec::new();
        if reload {
            self.stop_servers().await;
            self.start_servers().await;
            lines.push("reloaded MCP servers:".to_string());
        } else {
            lines.push("MCP servers:".to_string());
        }

        let servers = self.inner.snapshot();
        if servers.is_empty() {
            lines.push("  (none configured)".to_string());
        }
        for (name, srv) in &servers {
            let st = srv.state.lock();
            let mut line = format!("  {}: {}", name, st.state);
            if st.state == "connected" {
                line.push_str(&format!(" ({} tools)", st.tools.len()));
            }
            if let Some(err) = st.error.as_ref().filter(|e| !e.is_empty()) {
                line.push_str(&format!(" - {}", err));
            }
            lines.push(line);
        }
        let parse_error = self.inner.parse_error.lock().clone();
        if let Some(parse_error) = parse_error {
            lines.push(format!("  config error: {}", parse_error));
        }
        lines.join("\n")
    }
}

#[async_trait]
impl Module for Mcp {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        let dispatch = self.inner.dispatch.lock().get(method).cloned();
        let Some(dispatch) = dispatch else {
            anyhow::bail!("'Mcp' object has no attribute '{}'", method);
        };
        Ok(self.call_remote(&dispatch, args.0).await)
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        self.inner.servers.lock().clear();
        *self.inner.parse_error.lock() = None;
        self.inner.used_names.lock().clear();
        self.start_servers().await;
        Ok(())
    }

    async fn on_shutdown(&self) -> anyhow::Result<()> {
        self.stop_servers().await;
        Ok(())
    }

    fn commands(&self) -> Vec<CommandSpec> {
        meta::commands().into_iter().map(|(c, _)| c).collect()
    }

    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        match name {
            "mcp" => Ok(Some(self.mcp_command(&args).await)),
            _ => anyhow::bail!("unknown command {}", name),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a stdio server that answers initialize (id 0), tools/list (id 1) and tools/call (id 2)
    const FAKE_SERVER: &str = r#"read init; printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}'; read note; read list; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"echo","description":"Echo back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}'; read call; printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}'; read rest"#;

    fn no_notify() -> NotifyFn {
        Arc::new(|_: &str| {})
    }

    #[test]
    fn server_names_follow_python_rules() {
        assert_eq!(derive_url_name("https://mcp.exa.ai/mcp"), "exa");
        assert_eq!(derive_url_name("http://localhost:8000/mcp"), "localhost");
        assert_eq!(derive_url_name("http://mcp.example.com/mcp"), "example");
        assert_eq!(derive_stdio_name(&["uvx".into(), "mcp-server-time".into()]), "mcp-server-time");
        assert_eq!(derive_stdio_name(&["/usr/bin/python3".into(), "-m".into(), "pkg.sub.tool".into()]), "tool");
        assert_eq!(derive_stdio_name(&["npx".into(), "-y".into(), "@scope/thing".into()]), "scope_thing");
        assert_eq!(derive_stdio_name(&["./my server!".into()]), "my_server");
        assert_eq!(sanitize("__ ..."), "unnamed");

        let (name, cfg) = parse_entry("stdio", "  uvx mcp-server-time --flag  ").unwrap().unwrap();
        assert_eq!(name, "mcp-server-time");
        assert_eq!(cfg, ServerCfg::Stdio { command: "uvx".into(), args: vec!["mcp-server-time".into(), "--flag".into()] });
        let (name, cfg) = parse_entry("SSE", "http://h.example.org:9000/sse").unwrap().unwrap();
        assert_eq!(name, "h");
        assert_eq!(cfg, ServerCfg::Sse { url: "http://h.example.org:9000/sse".into() });
        assert_eq!(parse_entry("stdio", "   ").unwrap(), None);
        assert_eq!(parse_entry("stdio", "run 'unclosed").unwrap_err(), "invalid command in 'stdio': 'run 'unclosed' (No closing quotation)");

        let mut configs = IndexMap::new();
        configs.insert("exa".to_string(), ServerCfg::Http { url: "x".into() });
        assert_eq!(unique_server_name(&configs, "exa".into()), "exa_2");
    }

    #[test]
    fn tool_names_are_legal_truncated_and_unique() {
        let mut used = BTreeSet::new();
        assert_eq!(unique_tool_name(&mut used, "exa", "web search"), "mcp_exa_web_search");
        assert_eq!(unique_tool_name(&mut used, "exa", "web search"), "mcp_exa_web_search_2");
        let long = unique_tool_name(&mut used, "a-very-long-server-name", &"t".repeat(80));
        assert_eq!(long.chars().count(), 64);
        assert!(long.starts_with("mcp_a-very-long-server-name_"));
    }

    #[test]
    fn call_results_render_like_python() {
        let result = json!({"content": [
            {"type": "text", "text": "hello"},
            {"type": "image", "data": "AAAA", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "file:///a.bin", "blob": "AAAA"}},
            {"type": "resource", "resource": {"uri": "file:///b.txt", "text": "body"}},
            {"type": "resource_link", "uri": "https://x.test/r"},
            {"type": "video"}
        ]});
        assert_eq!(
            call_result_text(&result),
            "hello\n[image content omitted: image/png]\n[binary resource: file:///a.bin]\nbody\n[resource link: https://x.test/r]\n[unsupported content: video]"
        );
        assert_eq!(call_result_text(&json!({"content": []})), "(empty result)");
        assert_eq!(call_result_text(&json!({"content": [{"type": "text", "text": "  "}], "structuredContent": {"a": 1}})), r#"{"a": 1}"#);
    }

    #[test]
    fn rpc_errors_flatten_like_python() {
        let e = McpError::Rpc { code: -32602, message: "Invalid params".into(), data: Some(json!({"path": "x"})) };
        assert_eq!(flatten_error(&e), "MCPError [-32602] Invalid params | data: {'path': 'x'}");
        let e = McpError::Rpc { code: -1, message: "boom".into(), data: None };
        assert_eq!(flatten_error(&e), "MCPError [-1] boom");
        assert_eq!(flatten_error(&connection_closed()), "MCPError [-32000] Connection closed");
        assert_eq!(connection_closed().raw_message(), "Connection closed");
    }

    #[test]
    fn os_errors_use_python_errno_text_and_class() {
        let broken = std::io::Error::from_raw_os_error(libc::EPIPE);
        assert_eq!(flatten_error(&io_error(&broken)), "BrokenPipeError: [Errno 32] Broken pipe");
        let exec = std::io::Error::from_raw_os_error(libc::ENOEXEC);
        assert_eq!(flatten_error(&spawn_error(&exec, "./server")), "OSError: [Errno 8] Exec format error: './server'");
        let missing = std::io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(
            flatten_error(&spawn_error(&missing, "/nope")),
            "FileNotFoundError: [Errno 2] No such file or directory: '/nope'"
        );
    }

    #[test]
    fn http_status_errors_match_httpx_text() {
        assert_eq!(
            status_error(reqwest::StatusCode::NOT_FOUND, "http://h.test/mcp/", None).raw_message(),
            "Client error '404 Not Found' for url 'http://h.test/mcp/'\nFor more information check: https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/404"
        );
        assert_eq!(
            status_error(reqwest::StatusCode::MOVED_PERMANENTLY, "http://h.test/", Some("/new")).raw_message(),
            "Redirect response '301 Moved Permanently' for url 'http://h.test/'\nRedirect location: '/new'\nFor more information check: https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/301"
        );
        assert!(status_error(reqwest::StatusCode::BAD_GATEWAY, "http://h.test/", None)
            .raw_message()
            .starts_with("Server error '502 Bad Gateway'"));
    }

    #[test]
    fn shlex_failures_name_the_python_reason() {
        assert_eq!(shlex_error("run \"a\\\""), "No closing quotation");
        assert_eq!(shlex_error("run 'abc\\"), "No closing quotation");
        assert_eq!(shlex_error("run abc\\"), "No escaped character");
        assert_eq!(shlex_error("run \"abc\\"), "No escaped character");
    }

    #[test]
    fn url_hostnames_are_not_idna_encoded() {
        assert_eq!(url_hostname("https://bücher.de/mcp"), "bücher.de");
        assert_eq!(derive_url_name("https://bücher.de/mcp"), "b_cher");
        assert_eq!(derive_url_name("http://user:pw@[::1]:8000/mcp"), "1");
    }

    #[test]
    fn unsupported_protocol_version_fails_the_handshake() {
        assert_eq!(negotiated_version(&json!({"protocolVersion": "2025-06-18"})).unwrap(), "2025-06-18");
        let err = negotiated_version(&json!({"protocolVersion": "1999-01-01"})).unwrap_err();
        assert_eq!(flatten_error(&err), "RuntimeError: Unsupported protocol version from the server: 1999-01-01");
    }

    #[tokio::test]
    async fn stdio_shutdown_sends_sigterm_before_sigkill() {
        // ignores its stdin closing; exits 0 only when it gets SIGTERM (trap), so a SIGKILL would show
        let script = "trap 'exit 0' TERM; while true; do sleep 0.1; done";
        let (tx, _rx) = mpsc::unbounded_channel();
        let transport = StdioTransport::spawn("sh", &["-c".to_string(), script.to_string()], tx).expect("spawn sh");
        transport.close().await;
        let status = transport.child.lock().await.try_wait().expect("try_wait").expect("child exited");
        assert_eq!(status.code(), Some(0), "server should exit on SIGTERM, not be killed");
    }

    #[test]
    fn sse_decoder_handles_chunks_crlf_and_defaults() {
        let mut dec = SseDecoder::default();
        assert!(dec.push(b"data: {\"a\"").is_empty());
        let evs = dec.push(b":1}\r\n\r\nevent: endpoint\ndata: /messages?session=1\n\n: comment\ndata: line one\ndata: line two\n\n");
        assert_eq!(
            evs,
            vec![
                SseEvent { event: "message".into(), data: "{\"a\":1}".into() },
                SseEvent { event: "endpoint".into(), data: "/messages?session=1".into() },
                SseEvent { event: "message".into(), data: "line one\nline two".into() },
            ]
        );
        assert_eq!(resolve_endpoint("http://h.test:9/sse", "/messages?s=1").unwrap(), "http://h.test:9/messages?s=1");
    }

    #[tokio::test]
    async fn stdio_session_runs_handshake_list_and_call() {
        let cfg = ServerCfg::Stdio { command: "sh".into(), args: vec!["-c".into(), FAKE_SERVER.into()] };
        let session = Session::connect(&cfg, no_notify()).await.expect("connect to fake server");
        let listing = session.request("tools/list", json!({})).await.unwrap();
        assert_eq!(listing["tools"][0]["name"], json!("echo"));
        let result = session
            .request("tools/call", json!({"name": "echo", "arguments": {"text": "ping"}}))
            .await
            .unwrap();
        assert_eq!(call_result_text(&result), "pong");
        session.close().await;
    }

    #[tokio::test]
    async fn stdio_spawn_failure_reports_python_error_text() {
        let cfg = ServerCfg::Stdio { command: "/nonexistent/ol-mcp-test-binary".into(), args: vec![] };
        let err = Session::connect(&cfg, no_notify()).await.err().expect("spawn must fail");
        assert_eq!(
            flatten_error(&err),
            "FileNotFoundError: [Errno 2] No such file or directory: '/nonexistent/ol-mcp-test-binary'"
        );
    }
}
