//! Oneshot tests of the webui page and api routes (`/`, `/login`, `/logout`, `/api/chat/load/{id}`,
//! `/api/ext/{module}/{path}`). Each test drives the real axum router built by `build_router`, against a
//! channel whose config and data folder live in a temp dir.
//!
//! The config is process-global, so every harness holds `HARNESS` for its whole lifetime.

use crate::server::{build_router, build_shared, Shared};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use axum::body::{to_bytes, Body};
use axum::extract::connect_info::ConnectInfo;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use ol_core::channel::Channel;
use ol_core::manager::Manager;
use ol_core::module::{Module, ModuleBase, UiRoute};
use ol_core::pyjson::py_str;
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
use tower::ServiceExt;

static HARNESS: Lazy<AsyncMutex<()>> = Lazy::new(|| AsyncMutex::new(()));

/// the client address of every request (not loopback, so X-Forwarded-For is ignored)
const CLIENT: &str = "198.51.100.2:5000";

/// a webui served over the real router, plus the objects it must keep alive
struct Harness {
    router: Router,
    shared: Arc<Shared>,
    manager: Arc<Manager>,
    ch: Arc<Channel>,
    _lock: MutexGuard<'static, ()>,
    _data: tempfile::TempDir,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not json ({e}): {}", self.body))
    }

    /// the `name=value` pair of the first Set-Cookie header, ready to send back as a Cookie header
    fn session_cookie(&self) -> Option<String> {
        let set = self.headers.get(header::SET_COOKIE)?.to_str().ok()?;
        Some(set.split(';').next()?.to_string())
    }
}

/// writes the config the channel reads: `require_login` plus the credentials the login form checks.
/// the same json is written as `config.yml` (json is valid yaml) so a reload from disk keeps it
fn install_config(data_dir: &Path, require_login: bool) {
    let config = json!({
        "core": {"data_folder": data_dir.to_string_lossy()},
        "channels": {"settings": {"webui": {
            "require_login": require_login,
            "username": "admin",
            "password": "hunter2",
            "port": 8080,
            "network_mode": "local",
        }}},
    });
    let dir = std::env::temp_dir().join(format!("ol-test-config-{}", std::process::id()));
    let _ = std::fs::remove_file(dir.join("config.yml"));
    ol_core::config::set_for_tests(config.clone());
    std::fs::write(dir.join("config.yml"), config.to_string()).expect("write test config");
}

async fn harness(require_login: bool) -> Harness {
    let lock = HARNESS.lock().await;
    let data = tempfile::tempdir().expect("temp data folder");
    install_config(data.path(), require_login);
    let manager = Manager::new(Default::default());
    let ch = Channel::new(&manager, "webui", false, Arc::new(crate::server::Webui::default())).expect("channel");
    manager.insert_channel(ch.clone());
    ch.set_as_active_channel().await;
    let shared = build_shared(&ch).expect("webui shared state");
    Harness { router: build_router(shared.clone()), shared, manager, ch, _lock: lock, _data: data }
}

fn request(method: Method, uri: &str, cookie: Option<&str>, content_type: Option<&str>, body: &str) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        builder = builder.header(header::COOKIE, c);
    }
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    let mut req = builder.body(Body::from(body.to_string())).expect("request");
    req.extensions_mut().insert(ConnectInfo(CLIENT.parse::<SocketAddr>().unwrap()));
    req
}

impl Harness {
    async fn send(&self, req: Request<Body>) -> Reply {
        let resp = self.router.clone().oneshot(req).await.expect("router is infallible");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        Reply { status, headers, body: String::from_utf8_lossy(&bytes).into_owned() }
    }

    async fn get(&self, uri: &str) -> Reply {
        self.send(request(Method::GET, uri, None, None, "")).await
    }

    async fn login_form(&self, username: &str, password: &str) -> Reply {
        let body = format!("username={username}&password={password}");
        self.send(request(Method::POST, "/login", None, Some("application/x-www-form-urlencoded"), &body)).await
    }
}

/// a module that exposes a few webui routes, used to exercise `/api/ext/{module}/{path}`
struct Probe {
    base: ModuleBase,
}

#[async_trait]
impl Module for Probe {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn ui_routes(&self) -> Vec<UiRoute> {
        vec![
            UiRoute { path: "ping".into(), method: "GET".into() },
            UiRoute { path: "echo".into(), method: "POST".into() },
            UiRoute { path: "boom".into(), method: "GET".into() },
            UiRoute { path: "result".into(), method: "GET".into() },
        ]
    }

    async fn ui_route(&self, path: &str, method: &str, body: Option<Value>, query: Map<String, Value>) -> anyhow::Result<Value> {
        match (path, method) {
            ("ping", "GET") => Ok(json!("pong")),
            ("echo", "POST") => Ok(json!({"body": body.unwrap_or(Value::Null), "query": Value::Object(query)})),
            ("boom", "GET") => anyhow::bail!("probe exploded"),
            // a self.result() dict is unwrapped into the envelope
            ("result", "GET") => Ok(json!({"status": "error", "content": "nope"})),
            _ => anyhow::bail!("route not found: {}", path),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

async fn harness_with_probe() -> Harness {
    let h = harness(false).await;
    let base = ModuleBase::new("probe", false, Arc::downgrade(&h.manager));
    h.manager.insert_module(Arc::new(Probe { base }));
    h
}

#[tokio::test]
async fn root_serves_the_index_page_as_html() {
    let h = harness(false).await;
    let resp = h.get("/").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap().starts_with("text/html"));
    assert!(resp.body.contains("<html"));
}

#[tokio::test]
async fn login_page_redirects_home_when_login_is_not_required() {
    let h = harness(false).await;
    let resp = h.get("/login").await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert_eq!(resp.headers.get(header::LOCATION).unwrap(), "/");
}

#[tokio::test]
async fn login_page_renders_the_form_when_login_is_required() {
    let h = harness(true).await;
    let resp = h.get("/login").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("<html"));
    assert!(!resp.body.contains("Invalid credentials"));
}

#[tokio::test]
async fn login_submit_with_wrong_password_renders_invalid_credentials() {
    let h = harness(true).await;
    let resp = h.login_form("admin", "nope").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("Invalid credentials"));
    assert!(resp.session_cookie().is_none(), "a failed login must not start a session");
}

#[tokio::test]
async fn login_submit_with_good_credentials_sets_session_and_redirects_home() {
    let h = harness(true).await;
    let resp = h.login_form("admin", "hunter2").await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert_eq!(resp.headers.get(header::LOCATION).unwrap(), "/");
    assert!(resp.session_cookie().unwrap().starts_with("session="));
}

#[tokio::test]
async fn login_submit_is_rate_limited_after_five_failures() {
    let h = harness(true).await;
    for _ in 0..5 {
        assert_eq!(h.login_form("admin", "nope").await.status, StatusCode::OK);
    }
    // even the right password is refused while the window is full
    let resp = h.login_form("admin", "hunter2").await;
    assert_eq!(resp.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(resp.json(), json!({"error": "Too many attempts. Try again later."}));
}

#[tokio::test]
async fn logout_clears_the_session_and_redirects_to_login() {
    let h = harness(true).await;
    let cookie = h.login_form("admin", "hunter2").await.session_cookie().unwrap();
    let resp = h.send(request(Method::GET, "/logout", Some(&cookie), None, "")).await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    assert_eq!(resp.headers.get(header::LOCATION).unwrap(), "/login");
    // the emptied session is written back as an expired cookie
    let set = resp.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(set.starts_with("session=null;") && set.contains("1970"), "got {set}");
}

#[tokio::test]
async fn chat_load_switches_to_the_chat_and_returns_its_details() {
    let h = harness(false).await;
    let id = h.ch.chat.new_chat("general", "Loaded chat", None).await;
    let resp = h.get(&format!("/api/chat/load/{id}")).await;
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["success"], json!(true));
    assert_eq!(body["data"]["id"], json!(id));
    assert!(body["data"]["turn_history"].is_array());
    assert!(body["data"]["token_usage"].is_number());
    // turn_history is a new key, so it lands last; token_usage keeps the position of the chat's own key
    let keys: Vec<&str> = body["data"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys.last(), Some(&"turn_history"));
    assert_eq!(h.ch.chat.get("id").map(|v| py_str(&v)), Some(id));
}

#[tokio::test]
async fn chat_load_of_an_unknown_id_reports_the_error_envelope() {
    let h = harness(false).await;
    let resp = h.get("/api/chat/load/does-not-exist").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json(), json!({"data": "error while loading chat", "success": false}));
}

#[tokio::test]
async fn ext_api_forwards_get_to_the_module_handler() {
    let h = harness_with_probe().await;
    let resp = h.get("/api/ext/probe/ping").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json(), json!({"data": "pong", "success": true}));
}

#[tokio::test]
async fn ext_api_passes_the_json_body_and_query_to_post_handlers() {
    let h = harness_with_probe().await;
    let req = request(Method::POST, "/api/ext/probe/echo?a=1&a=2&b=x+y", None, Some("application/json"), r#"{"x": 1}"#);
    let resp = h.send(req).await;
    assert_eq!(resp.json(), json!({"data": {"body": {"x": 1}, "query": {"a": "2", "b": "x y"}}, "success": true}));
}

#[tokio::test]
async fn ext_api_parses_a_json_body_with_a_utf8_bom_like_python() {
    // python's json.loads(bytes) detects utf-8-sig, so a leading BOM is skipped
    let h = harness_with_probe().await;
    let mut req = request(Method::POST, "/api/ext/probe/echo", None, Some("application/json"), "");
    *req.body_mut() = Body::from("\u{feff}{\"x\": 1}");
    let resp = h.send(req).await;
    assert_eq!(resp.json(), json!({"data": {"body": {"x": 1}, "query": {}}, "success": true}));

    // a body that is not JSON (or a JSON null) reaches the handler as None
    let bad = h.send(request(Method::POST, "/api/ext/probe/echo", None, None, "{x")).await;
    assert_eq!(bad.json(), json!({"data": {"body": null, "query": {}}, "success": true}));
}

#[tokio::test]
async fn ext_api_rejects_the_wrong_method_and_unknown_routes_like_python() {
    let h = harness_with_probe().await;
    let wrong = h.send(request(Method::POST, "/api/ext/probe/ping", None, None, "")).await;
    assert_eq!(wrong.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(wrong.json(), json!({"data": "route only accepts GET", "success": false}));

    let no_module = h.get("/api/ext/nobody/ping").await;
    assert_eq!(no_module.status, StatusCode::NOT_FOUND);
    assert_eq!(no_module.json(), json!({"data": "unknown module", "success": false}));

    let no_route = h.get("/api/ext/probe/missing").await;
    assert_eq!(no_route.status, StatusCode::NOT_FOUND);
    assert_eq!(no_route.json(), json!({"data": "route not found", "success": false}));
}

#[tokio::test]
async fn ext_api_turns_handler_errors_into_a_failed_envelope_with_status_200() {
    let h = harness_with_probe().await;
    let boom = h.get("/api/ext/probe/boom").await;
    assert_eq!(boom.status, StatusCode::OK);
    assert_eq!(boom.json(), json!({"data": "probe exploded", "success": false}));

    // a self.result() dict with status != success unwraps to success false
    let result = h.get("/api/ext/probe/result").await;
    assert_eq!(result.json(), json!({"data": "nope", "success": false}));
}

#[tokio::test]
async fn ext_api_with_an_empty_route_path_returns_the_envelope_like_python() {
    // starlette's `:path` matches an empty remainder, so `/api/ext/<module>/` is the api envelope, not {"detail"}
    let h = harness_with_probe().await;
    let no_route = h.get("/api/ext/probe/").await;
    assert_eq!(no_route.status, StatusCode::NOT_FOUND);
    assert_eq!(no_route.json(), json!({"data": "route not found", "success": false}));

    let no_module = h.get("/api/ext/nobody/").await;
    assert_eq!(no_module.status, StatusCode::NOT_FOUND);
    assert_eq!(no_module.json(), json!({"data": "unknown module", "success": false}));

    let wrong = h.send(request(Method::POST, "/api/ext/probe/", None, None, "")).await;
    assert_eq!(wrong.status, StatusCode::NOT_FOUND);
    assert_eq!(wrong.json(), json!({"data": "route not found", "success": false}));
}

#[tokio::test]
async fn ext_api_answers_head_with_405_whatever_the_route_like_python() {
    // python's api_route has GET/POST/PUT/DELETE only, so HEAD is a starlette 405, even for an existing GET route
    let h = harness_with_probe().await;
    for uri in ["/api/ext/probe/ping", "/api/ext/nobody/ping", "/api/ext/probe/missing", "/api/ext/probe/"] {
        let resp = h.send(request(Method::HEAD, uri, None, None, "")).await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "HEAD {uri}");
    }
    // the GET route still answers normally
    assert_eq!(h.get("/api/ext/probe/ping").await.json(), json!({"data": "pong", "success": true}));
}

#[tokio::test]
async fn get_only_pages_answer_head_with_starlette_405_not_the_body() {
    // FastAPI's @app.get adds no HEAD, so starlette answers HEAD with its 405 detail body and `Allow: GET`
    let h = harness(false).await;
    for uri in ["/", "/login", "/logout", "/api/chat/load/some-id"] {
        let resp = h.send(request(Method::HEAD, uri, None, None, "")).await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "HEAD {uri}");
        assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET", "HEAD {uri}");
        // a HEAD response carries no body
        assert!(resp.body.is_empty(), "HEAD {uri} body: {}", resp.body);
    }
    // GET is unaffected
    assert_eq!(h.get("/").await.status, StatusCode::OK);
}

#[tokio::test]
async fn wrong_methods_on_python_routes_carry_the_allow_header() {
    let h = harness_with_probe().await;
    // python's first partial match decides Allow: "/login" has GET first, so PATCH reports GET
    let root = h.send(request(Method::PATCH, "/", None, None, "")).await;
    assert_eq!(root.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(root.headers.get(header::ALLOW).unwrap(), "GET");
    assert_eq!(root.json(), json!({"detail": "Method Not Allowed"}));

    let login = h.send(request(Method::PATCH, "/login", None, None, "")).await;
    assert_eq!(login.headers.get(header::ALLOW).unwrap(), "GET");

    let ext = h.send(request(Method::PATCH, "/api/ext/probe/ping", None, None, "")).await;
    assert_eq!(ext.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(ext.headers.get(header::ALLOW).unwrap(), "GET, POST, PUT, DELETE");
    assert_eq!(ext.json(), json!({"detail": "Method Not Allowed"}));
}

#[tokio::test]
async fn trailing_slash_variants_redirect_with_307_like_starlette() {
    let h = harness(false).await;
    let with_host = |method: Method, uri: &str| {
        let mut req = request(method, uri, None, None, "");
        req.headers_mut().insert(header::HOST, "localhost:8080".parse().unwrap());
        req
    };

    let logout = h.send(with_host(Method::GET, "/logout/")).await;
    assert_eq!(logout.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(logout.headers.get(header::LOCATION).unwrap(), "http://localhost:8080/logout");

    // any method is redirected, and the query string is kept
    let login = h.send(with_host(Method::POST, "/login/?next=1")).await;
    assert_eq!(login.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(login.headers.get(header::LOCATION).unwrap(), "http://localhost:8080/login?next=1");

    let module = h.send(with_host(Method::GET, "/api/ext/probe")).await;
    assert_eq!(module.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(module.headers.get(header::LOCATION).unwrap(), "http://localhost:8080/api/ext/probe/");

    // a request that already matches is not redirected
    assert_eq!(h.get("/api/ext/probe/").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn percent_encoded_paths_route_on_the_decoded_path_like_python() {
    let h = harness_with_probe().await;
    h.ch.chat.new_chat("general", "Encoded", None).await;
    // "%61" is "a": starlette sees /api/chat/current and /api/ext/probe/ping
    assert_eq!(h.get("/%61pi/chat/current").await.json(), h.get("/api/chat/current").await.json());
    assert_eq!(h.get("/api/ext/%70robe/ping").await.json(), json!({"data": "pong", "success": true}));
    // an encoded slash is a path separator once decoded, so the route does not match
    let split = h.get("/api/chat/load/a%2Fb").await;
    assert_eq!(split.status, StatusCode::NOT_FOUND);
    assert_eq!(split.json(), json!({"detail": "Not Found"}));
}

#[tokio::test]
async fn encoded_prefix_is_authenticated_as_the_decoded_api_path() {
    // with login required, the decoded /api path gets python's 401 (not the 303 to /login of a page)
    let h = harness(true).await;
    let resp = h.get("/%61pi/chat/current").await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp.json(), json!({"detail": "Unauthorized"}));
}

// ------------------
// chat current / stream state / export / chat listings
// ------------------

/// creates a chat in `category` and pins its `updated` stamp and tags in the index, returning its id
async fn chat_at(h: &Harness, category: &str, title: &str, updated: &str, tags: &[&str]) -> String {
    let id = h.ch.chat.new_chat(category, title, None).await;
    let tags = json!(tags);
    h.ch.chat.with_data_mut(|data| {
        for chat in data.iter_mut().filter(|c| c["id"] == json!(id)) {
            chat["updated"] = json!(updated);
            chat["tags"] = tags.clone();
        }
    });
    id
}

/// the `id` of every message in a listing response, in order
fn message_ids(body: &Value) -> Vec<String> {
    body["data"]["messages"].as_array().unwrap().iter().map(|m| py_str(&m["id"])).collect()
}

#[tokio::test]
async fn chat_current_returns_the_loaded_chat_with_its_turn_history() {
    let h = harness(false).await;
    let id = h.ch.chat.new_chat("general", "Current chat", None).await;
    let body = h.get("/api/chat/current").await.json();
    assert_eq!(body["success"], json!(true));
    assert_eq!(body["data"]["id"], json!(id));
    assert_eq!(body["data"]["title"], json!("Current chat"));
    assert_eq!(body["data"]["turn_history"], json!([]));
    assert!(body["data"]["token_usage"].is_number());
    // turn_history is not a stored chat key, so it is appended after the chat's own keys
    let keys: Vec<&str> = body["data"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys.last(), Some(&"turn_history"));

    // a user message shows up as a turn in the live history
    let msg = json!({"role": "user", "content": "hi there"});
    assert!(h.ch.chat.messages_add(&msg, false, false).await);
    let body = h.get("/api/chat/current").await.json();
    assert_eq!(body["data"]["turn_history"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn stream_state_reports_the_backend_stream_state() {
    let h = harness(false).await;
    assert_eq!(h.get("/api/stream/state").await.json(), json!({"data": {"state": "idle"}, "success": true}));
    h.shared.ws.set_stream_state("processing");
    assert_eq!(h.get("/api/stream/state").await.json(), json!({"data": {"state": "processing"}, "success": true}));
}

#[tokio::test]
async fn chat_export_returns_the_transcript_and_a_sanitized_filename() {
    let h = harness(false).await;
    h.ch.chat.new_chat("general", r#"a/b:c*?"<>|"#, None).await;
    assert!(h.ch.chat.messages_add(&json!({"role": "user", "content": "hello"}), false, false).await);
    let resp = h.get("/api/chat/export").await;
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body, json!({"data": {"content": "--- user ---\nhello", "filename": "a_b_c______.txt"}, "success": true}));
}

#[tokio::test]
async fn chat_export_falls_back_to_chat_export_for_a_blank_title() {
    let h = harness(false).await;
    h.ch.chat.new_chat("general", "   ", None).await;
    let body = h.get("/api/chat/export").await.json();
    assert_eq!(body, json!({"data": {"content": "", "filename": "chat-export.txt"}, "success": true}));
}

#[tokio::test]
async fn chat_current_without_a_loaded_chat_is_the_plain_500() {
    // a fresh channel has no chat loaded
    let h = harness(false).await;
    assert!(h.ch.chat.current().is_none());
    let resp = h.get("/api/chat/current").await;
    assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(resp.body, "Internal Server Error");
}

#[tokio::test]
async fn chat_export_of_a_truthy_non_string_title_is_the_plain_500() {
    let h = harness(false).await;
    let id = h.ch.chat.new_chat("general", "placeholder", None).await;
    h.ch.chat.with_data_mut(|data| {
        for chat in data.iter_mut().filter(|c| c["id"] == json!(id)) {
            chat["title"] = json!(42);
        }
    });
    let resp = h.get("/api/chat/export").await;
    assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
    // a falsy non-string title is the blank-title fallback, as in python `or ""`
    h.ch.chat.with_data_mut(|data| {
        for chat in data.iter_mut().filter(|c| c["id"] == json!(id)) {
            chat["title"] = json!(0);
        }
    });
    let body = h.get("/api/chat/export").await.json();
    assert_eq!(body["data"]["filename"], json!("chat-export.txt"));
}

#[tokio::test]
async fn chat_listings_500_on_corrupt_updated_values_like_python() {
    let h = harness(false).await;
    chat_at(&h, "general", "Fine", "2026-01-01T10:00:00", &[]).await;
    h.ch.chat.with_data_mut(|data| data.push(json!({"id": "broken", "title": "x", "category": "general", "updated": 7})));
    // python sorts the index first, then the day grouping raises on the numeric 'updated'
    assert_eq!(h.get("/api/chats?offset=0&limit=5").await.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(h.get("/api/chats/days?tz_offset=0").await.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(h.get("/api/chats/day?day=2026-01-01&tz_offset=0").await.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn get_chats_pages_the_index_newest_first_with_has_more() {
    let h = harness(false).await;
    let oldest = chat_at(&h, "general", "Oldest", "2026-01-01T10:00:00", &[]).await;
    let middle = chat_at(&h, "general", "Middle", "2026-02-01T10:00:00", &[]).await;
    let newest = chat_at(&h, "general", "Newest", "2026-03-01T10:00:00", &[]).await;

    let first = h.get("/api/chats?limit=2").await.json();
    assert_eq!(first["success"], json!(true));
    assert_eq!(message_ids(&first), vec![newest.clone(), middle.clone()]);
    assert_eq!(first["data"]["has_more"], json!(true));

    let second = h.get("/api/chats?offset=2&limit=2").await.json();
    assert_eq!(message_ids(&second), vec![oldest]);
    assert_eq!(second["data"]["has_more"], json!(false));

    // the list projection keeps only the python CHAT_LIST_FIELDS, in that order
    let keys: Vec<&str> = first["data"]["messages"][0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["id", "title", "category", "tags", "created", "updated"]);
    assert_eq!(first["data"]["messages"][0]["title"], json!("Newest"));
}

#[tokio::test]
async fn get_chats_filters_by_category_and_rejects_a_non_integer_offset() {
    let h = harness(false).await;
    chat_at(&h, "general", "Plain", "2026-01-01T10:00:00", &[]).await;
    let work = chat_at(&h, "work", "Work item", "2026-02-01T10:00:00", &[]).await;

    let filtered = h.get("/api/chats?category=work").await.json();
    assert_eq!(message_ids(&filtered), vec![work]);
    assert_eq!(filtered["data"]["has_more"], json!(false));

    // an empty category is falsy in python, so it does not filter
    assert_eq!(message_ids(&h.get("/api/chats?category=").await.json()).len(), 2);

    // python int("x") raises, which starlette answers as a plain 500
    let bad = h.get("/api/chats?offset=abc").await;
    assert_eq!(bad.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn get_chat_days_groups_chats_by_day_month_and_counts_them_newest_first() {
    let h = harness(false).await;
    let today = Utc::now().date_naive();
    let today_key = today.format("%Y-%m-%d").to_string();
    let old_key = (today - Duration::days(40)).format("%Y-%m").to_string();

    chat_at(&h, "general", "Old chat", &format!("{}T12:00:00", (today - Duration::days(40)).format("%Y-%m-%d")), &[]).await;
    chat_at(&h, "general", "Morning", &format!("{today_key}T09:00:00"), &[]).await;
    chat_at(&h, "general", "Evening", &format!("{today_key}T10:00:00"), &["alpha"]).await;

    let groups = h.get("/api/chats/days").await.json();
    assert_eq!(groups["success"], json!(true));
    let groups = groups["data"].as_array().unwrap().clone();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0], json!({"key": today_key, "count": 2, "label": "Today"}));
    assert_eq!(groups[1]["key"], json!(old_key));
    assert_eq!(groups[1]["count"], json!(1));
    // a month group is labelled "<Month> <year>"
    assert_eq!(groups[1]["label"], json!((today - Duration::days(40)).format("%B %Y").to_string()));

    // repeated tags narrow the listing with AND semantics
    let tagged = h.get("/api/chats/days?tags=alpha").await.json();
    assert_eq!(tagged["data"], json!([{"key": today_key, "count": 1, "label": "Today"}]));
    let both = h.get("/api/chats/days?tags=alpha&tags=beta").await.json();
    assert_eq!(both["data"], json!([]));
}

#[tokio::test]
async fn get_chats_for_day_pages_one_group_and_reports_its_count() {
    let h = harness(false).await;
    let today = Utc::now().date_naive();
    let today_key = today.format("%Y-%m-%d").to_string();
    let old_key = (today - Duration::days(40)).format("%Y-%m").to_string();

    let morning = chat_at(&h, "general", "Morning", &format!("{today_key}T09:00:00"), &[]).await;
    let evening = chat_at(&h, "general", "Evening", &format!("{today_key}T10:00:00"), &[]).await;
    let old = chat_at(&h, "general", "Old chat", &format!("{}T12:00:00", (today - Duration::days(40)).format("%Y-%m-%d")), &[]).await;

    let page = h.get(&format!("/api/chats/day?day={today_key}&limit=1")).await.json();
    assert_eq!(page["success"], json!(true));
    assert_eq!(message_ids(&page), vec![evening.clone()]);
    assert_eq!(page["data"]["has_more"], json!(true));
    assert_eq!(page["data"]["count"], json!(2));

    let rest = h.get(&format!("/api/chats/day?day={today_key}&offset=1&limit=1")).await.json();
    assert_eq!(message_ids(&rest), vec![morning]);
    assert_eq!(rest["data"]["has_more"], json!(false));

    let month = h.get(&format!("/api/chats/day?day={old_key}")).await.json();
    assert_eq!(message_ids(&month), vec![old]);
    assert_eq!(month["data"]["count"], json!(1));

    let empty = h.get("/api/chats/day?day=1999-01-01").await.json();
    assert_eq!(empty["data"], json!({"messages": [], "has_more": false, "count": 0}));

    // repeated tags narrow the day group as well: no chat carries "nope"
    let tagged = h.get(&format!("/api/chats/day?day={today_key}&tags=nope")).await.json();
    assert_eq!(tagged["data"]["count"], json!(0));
}

#[tokio::test]
async fn get_only_chat_routes_answer_head_and_post_with_405_allow_get() {
    // FastAPI's @app.get adds no HEAD, and starlette's 405 carries `Allow: GET`; a POST gets the detail body too
    let h = harness(false).await;
    for uri in ["/api/chat/current", "/api/stream/state", "/api/chat/export", "/api/chats", "/api/chats/days", "/api/chats/day"] {
        let resp = h.send(request(Method::HEAD, uri, None, None, "")).await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "HEAD {uri}");
        assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET", "HEAD {uri}");
        assert!(resp.body.is_empty(), "HEAD {uri} body: {}", resp.body);
    }
    let resp = h.send(request(Method::POST, "/api/chats", None, None, "")).await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET");
    assert_eq!(resp.json(), json!({"detail": "Method Not Allowed"}));
    // GET still answers normally
    let state = h.get("/api/stream/state").await;
    assert_eq!(state.status, StatusCode::OK);
    assert!(state.json()["data"]["state"].is_string());
}

#[tokio::test]
async fn chat_current_and_load_fail_with_500_when_a_stored_token_count_is_not_a_python_int() {
    // python's get_total_tokens runs int() on token_usage and token_usage_mark: null, a list, or a non-numeric
    // string raises, so chat/current and chat/load are the plain 500
    let h = harness(false).await;
    let id = h.ch.chat.new_chat("general", "tokens", None).await;
    let set = |key: &'static str, value: Value| {
        h.ch.chat.with_data_mut(|data| {
            for chat in data.iter_mut().filter(|c| c["id"] == json!(id)) {
                chat[key] = value.clone();
            }
        });
    };
    set("token_usage", Value::Null);
    let resp = h.get("/api/chat/current").await;
    assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(resp.body, "Internal Server Error");
    assert_eq!(h.get(&format!("/api/chat/load/{id}")).await.status, StatusCode::INTERNAL_SERVER_ERROR);

    set("token_usage", json!(0));
    set("token_usage_mark", json!([1]));
    assert_eq!(h.get("/api/chat/current").await.status, StatusCode::INTERNAL_SERVER_ERROR);

    // a numeric string that python's int() accepts (underscores included) is no error
    set("token_usage", json!("1_000"));
    set("token_usage_mark", json!("0"));
    let ok = h.get("/api/chat/current").await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(ok.json()["success"], json!(true));
}

// ------------------
// chat categories / tags / search (channels/webui/channel.py api-3)
// ------------------

fn post_json(uri: &str, body: &str) -> Request<Body> {
    request(Method::POST, uri, None, Some("application/json"), body)
}

#[tokio::test]
async fn get_chat_categories_lists_sorted_names_with_general_first() {
    let h = harness(false).await;
    chat_at(&h, "work", "w", "2026-11-01T10:00:00", &[]).await;
    chat_at(&h, "alpha", "a", "2026-11-01T10:00:00", &[]).await;
    chat_at(&h, "general", "g", "2026-11-01T10:00:00", &[]).await;
    let resp = h.get("/api/chats/categories").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json(), json!({"data": ["general", "alpha", "work"], "success": true}));
}

#[tokio::test]
async fn get_chat_category_counts_counts_each_category_and_missing_as_general() {
    let h = harness(false).await;
    chat_at(&h, "work", "w1", "2026-11-01T10:00:00", &[]).await;
    chat_at(&h, "work", "w2", "2026-11-01T10:00:00", &[]).await;
    chat_at(&h, "general", "g", "2026-11-01T10:00:00", &[]).await;
    let resp = h.get("/api/chats/categories/count").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json(), json!({"data": {"work": 2, "general": 1}, "success": true}));
}

#[tokio::test]
async fn get_chat_tags_lists_sorted_unique_tags_scoped_by_category() {
    let h = harness(false).await;
    chat_at(&h, "work", "w1", "2026-11-01T10:00:00", &["urgent", "bug"]).await;
    chat_at(&h, "work", "w2", "2026-11-01T10:00:00", &["bug", ""]).await;
    chat_at(&h, "home", "h1", "2026-11-01T10:00:00", &["recipe"]).await;
    let all = h.get("/api/chats/tags").await;
    assert_eq!(all.json(), json!({"data": ["bug", "recipe", "urgent"], "success": true}));
    let work = h.get("/api/chats/tags?category=work").await;
    assert_eq!(work.json(), json!({"data": ["bug", "urgent"], "success": true}));
}

#[tokio::test]
async fn category_get_routes_answer_head_and_post_with_405_allow_get() {
    let h = harness(false).await;
    for uri in ["/api/chats/categories", "/api/chats/categories/count", "/api/chats/tags"] {
        let resp = h.send(request(Method::HEAD, uri, None, None, "")).await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "HEAD {uri}");
        assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET", "HEAD {uri}");
        assert!(resp.body.is_empty(), "HEAD {uri} body: {}", resp.body);
        let resp = h.send(request(Method::POST, uri, None, None, "")).await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "POST {uri}");
        assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET", "POST {uri}");
    }
}

#[tokio::test]
async fn create_chat_category_validates_then_opens_a_chat_in_the_new_category() {
    let h = harness(false).await;
    chat_at(&h, "work", "w", "2026-11-01T10:00:00", &[]).await;

    let created = h.send(post_json("/api/chats/categories/create", r#"{"name": "  Travel  "}"#)).await;
    assert_eq!(created.status, StatusCode::OK);
    let id = created.json()["data"]["id"].as_str().expect("new chat id").to_string();
    assert_eq!(created.json()["success"], json!(true));
    let chats = h.ch.chat.data();
    let new_chat = chats.iter().find(|c| c["id"] == json!(id)).expect("new chat in index");
    assert_eq!(new_chat["category"], json!("travel"));

    let dup = h.send(post_json("/api/chats/categories/create", r#"{"name": "WORK"}"#)).await;
    assert_eq!(dup.json(), json!({"data": "'work' already exists", "success": false}));
    let colon = h.send(post_json("/api/chats/categories/create", r#"{"name": "a:b"}"#)).await;
    assert_eq!(colon.json(), json!({"data": "':' is reserved for subcategories", "success": false}));
    let blank = h.send(post_json("/api/chats/categories/create", r#"{"name": "  "}"#)).await;
    assert_eq!(blank.json(), json!({"data": "category name cannot be empty", "success": false}));
    let missing = h.send(post_json("/api/chats/categories/create", "{}")).await;
    assert_eq!(missing.json(), json!({"data": "category name cannot be empty", "success": false}));
    // a non-string name raises in python's .strip(): the plain 500
    let number = h.send(post_json("/api/chats/categories/create", r#"{"name": 5}"#)).await;
    assert_eq!(number.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn delete_chat_category_moves_its_chats_to_general() {
    let h = harness(false).await;
    let w1 = chat_at(&h, "work", "w1", "2026-11-01T10:00:00", &[]).await;
    let w2 = chat_at(&h, "work", "w2", "2026-11-01T10:00:00", &[]).await;
    chat_at(&h, "home", "h", "2026-11-01T10:00:00", &[]).await;

    let refused = h.send(post_json("/api/chats/categories/delete", r#"{"name": " general "}"#)).await;
    assert_eq!(refused.json(), json!({"data": "'general' cannot be deleted", "success": false}));
    let blank = h.send(post_json("/api/chats/categories/delete", r#"{"name": null}"#)).await;
    assert_eq!(blank.json(), json!({"data": "'general' cannot be deleted", "success": false}));

    let nothing = h.send(post_json("/api/chats/categories/delete", r#"{"name": "missing"}"#)).await;
    assert_eq!(nothing.json(), json!({"data": {"moved": 0}, "success": true}));

    let deleted = h.send(post_json("/api/chats/categories/delete", r#"{"name": " work "}"#)).await;
    assert_eq!(deleted.json(), json!({"data": {"moved": 2}, "success": true}));
    let chats = h.ch.chat.data();
    for id in [&w1, &w2] {
        let chat = chats.iter().find(|c| c["id"] == json!(id)).expect("chat kept");
        assert_eq!(chat["category"], json!("general"));
    }
    let categories = h.get("/api/chats/categories").await;
    assert_eq!(categories.json()["data"], json!(["general", "home"]));
}

#[tokio::test]
async fn search_chats_matches_titles_highlights_them_and_sorts_newest_first() {
    let h = harness(false).await;
    let older = chat_at(&h, "work", "Budget review", "2026-11-01T09:00:00", &["money"]).await;
    let newer = chat_at(&h, "work", "Budget plan", "2026-11-02T09:00:00", &["money", "urgent"]).await;
    chat_at(&h, "home", "Recipes", "2026-11-03T09:00:00", &[]).await;

    let resp = h.send(post_json("/api/chats/search", r#"{"query": " budget "}"#)).await;
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["success"], json!(true));
    let hits = body["data"].as_array().expect("hit list");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0]["id"], json!(newer));
    assert_eq!(hits[1]["id"], json!(older));
    assert_eq!(hits[0]["title_highlighted"], json!(r#"<strong class="search-highlight">Budget</strong> plan"#));
    assert_eq!(hits[0]["title_match"], json!(true));
    assert_eq!(hits[0]["snippets_highlighted"], json!([]));
    assert!(hits[0]["group_key"].is_string() && hits[0]["group_label"].is_string());
    assert_ne!(hits[0]["group_key"], json!("undated"));

    // a category filter, a tag filter (AND) and the empty query
    let in_home = h.send(post_json("/api/chats/search", r#"{"query": "budget", "category": "general"}"#)).await;
    assert_eq!(in_home.json()["data"], json!([]));
    let urgent = h.send(post_json("/api/chats/search", r#"{"query": "budget", "tags": ["money", "urgent"]}"#)).await;
    assert_eq!(urgent.json()["data"].as_array().unwrap().len(), 1);
    assert_eq!(urgent.json()["data"][0]["id"], json!(newer));
    let empty = h.send(post_json("/api/chats/search", r#"{"query": "   "}"#)).await;
    assert_eq!(empty.json(), json!({"data": [], "success": true}));
}

#[tokio::test]
async fn search_chats_escapes_html_and_rejects_a_non_string_query_like_python() {
    let h = harness(false).await;
    chat_at(&h, "work", "<b>Plan</b> & more", "2026-11-01T09:00:00", &[]).await;
    let resp = h.send(post_json("/api/chats/search", r#"{"query": "plan"}"#)).await;
    let hit = &resp.json()["data"][0];
    assert_eq!(hit["title_highlighted"], json!(r#"&lt;b&gt;<strong class="search-highlight">Plan</strong>&lt;/b&gt; &amp; more"#));
    // `query.strip()` on an int raises: plain 500
    let bad = h.send(post_json("/api/chats/search", r#"{"query": 7}"#)).await;
    assert_eq!(bad.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn search_chats_500s_when_a_ranked_hit_has_a_non_string_title() {
    let h = harness(false).await;
    let hit = chat_at(&h, "work", "Budget", "2026-11-01T09:00:00", &[]).await;
    let other = chat_at(&h, "work", "Recipes", "2026-11-02T09:00:00", &[]).await;
    // message text makes both chats searchable through their history
    h.ch.chat.load(&hit).await.unwrap();
    h.ch.chat.messages_add(&json!({"role": "user", "content": "budget notes"}), false, false).await;
    h.ch.chat.messages_save().await;
    h.ch.chat.load(&other).await.unwrap();
    h.ch.chat.messages_add(&json!({"role": "user", "content": "dinner ideas"}), false, false).await;
    h.ch.chat.messages_save().await;
    let before = h.send(post_json("/api/chats/search", r#"{"query": "budget"}"#)).await;
    assert_eq!(before.json()["data"].as_array().unwrap().len(), 1);
    // python raises AttributeError on `.lower()` for the ranked hit, so the whole search is the plain 500
    h.ch.chat.with_data_mut(|data| {
        for chat in data.iter_mut().filter(|c| c["id"] == json!(hit)) {
            chat["title"] = json!(5);
        }
    });
    let resp = h.send(post_json("/api/chats/search", r#"{"query": "budget"}"#)).await;
    assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
    // a non-string title on a chat that is not a hit does not reach `.lower()`
    let miss = h.send(post_json("/api/chats/search", r#"{"query": "zzz-nothing"}"#)).await;
    assert_eq!(miss.json(), json!({"data": [], "success": true}));
}

#[tokio::test]
async fn category_and_search_routes_reject_a_non_json_body_with_the_plain_500() {
    let h = harness(false).await;
    for uri in ["/api/chats/categories/create", "/api/chats/categories/delete", "/api/chats/search"] {
        let resp = h.send(post_json(uri, "not json")).await;
        assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR, "{uri}");
    }
}

#[tokio::test]
async fn category_routes_follow_python_type_errors_on_odd_stored_values() {
    let h = harness(false).await;
    chat_at(&h, "work", "w", "2026-11-01T10:00:00", &[]).await;
    chat_at(&h, "home", "h", "2026-11-01T10:00:00", &[]).await;
    // a numeric category next to strings makes python's sorted(set(...)) raise
    h.ch.chat.with_data_mut(|data| data[0]["category"] = json!(5));
    assert_eq!(h.get("/api/chats/categories").await.status, StatusCode::INTERNAL_SERVER_ERROR);
    // create checks the format first, then `.lower()` on the stored non-string category raises
    let format = h.send(post_json("/api/chats/categories/create", r#"{"name": "a:b"}"#)).await;
    assert_eq!(format.json(), json!({"data": "':' is reserved for subcategories", "success": false}));
    let dup = h.send(post_json("/api/chats/categories/create", r#"{"name": "fresh"}"#)).await;
    assert_eq!(dup.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn search_tags_filter_only_fails_for_non_iterable_tags_when_there_are_results() {
    let h = harness(false).await;
    chat_at(&h, "work", "Budget", "2026-11-01T10:00:00", &["m", "y"]).await;
    // `for t in 5` raises, but only once the comprehension has a result to test
    let with_hits = h.send(post_json("/api/chats/search", r#"{"query": "budget", "tags": 5}"#)).await;
    assert_eq!(with_hits.status, StatusCode::INTERNAL_SERVER_ERROR);
    let no_hits = h.send(post_json("/api/chats/search", r#"{"query": "nothing-matches", "tags": 5}"#)).await;
    assert_eq!(no_hits.json(), json!({"data": [], "success": true}));
    // a string is iterated by character: each character must be one of the chat's tags
    let chars = h.send(post_json("/api/chats/search", r#"{"query": "budget", "tags": "my"}"#)).await;
    assert_eq!(chars.json()["data"].as_array().unwrap().len(), 1);
    let not_all = h.send(post_json("/api/chats/search", r#"{"query": "budget", "tags": "money"}"#)).await;
    assert_eq!(not_all.json()["data"], json!([]));
}
