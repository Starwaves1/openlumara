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
use ol_modules_data::calendar::Calendar;
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

// ------------------
// chat prompt / new / rename / delete / set-category / input history (channels/webui/channel.py api-4)
// ------------------

/// a module named like the agentic-free `time` module, so its system prompt is listed without tools
struct PromptModule {
    base: ModuleBase,
}

#[async_trait]
impl Module for PromptModule {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        Ok(Some("PROMPT BODY".to_string()))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// a local stand-in for the OpenAI-style API: every request is answered with an empty model list.
/// returns the api url to put in the config
async fn mock_api() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock api");
    let addr = listener.local_addr().expect("mock api address");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let body = r#"{"data": []}"#;
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(answer.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://{addr}/v1")
}

/// points the live api client at `url`
fn set_api_url(url: &str) {
    ol_core::config::update(|c| {
        c.insert("api".to_string(), json!({"url": url}));
    });
}

/// the index entry of chat `id`
fn index_entry(h: &Harness, id: &str) -> Value {
    h.ch.chat.data().into_iter().find(|c| c["id"] == json!(id)).expect("chat in index")
}

#[tokio::test]
async fn chat_prompt_reports_the_api_error_object_when_the_api_is_not_set_up() {
    let h = harness(false).await;
    set_api_url("http://API_URL_HERE/v1");
    let resp = h.get("/api/chat/prompt").await;
    assert_eq!(resp.status, StatusCode::OK);
    // python returns the APIError itself as data: its vars() are message, then exc (None here)
    assert_eq!(
        resp.json(),
        json!({
            "data": {
                "message": "The API connection has not been set up yet! Please set up your API connection by either using the WebUI, the /config command, or editing the config file",
                "exc": null,
            },
            "success": false,
        })
    );
}

#[tokio::test]
async fn chat_prompt_returns_the_system_prompt_once_the_api_is_connected() {
    let h = harness(false).await;
    h.manager.insert_module(Arc::new(PromptModule { base: ModuleBase::new("time", false, Arc::downgrade(&h.manager)) }));
    set_api_url(&mock_api().await);
    let expected = h.ch.chat.get_system_prompt().await;
    assert!(expected.contains("PROMPT BODY"), "system prompt: {expected}");
    let resp = h.get("/api/chat/prompt").await;
    assert_eq!(resp.json(), json!({"data": expected, "success": true}));
}

#[tokio::test]
async fn chat_prompt_is_the_plain_500_when_no_system_prompt_is_built() {
    let h = harness(false).await;
    set_api_url(&mock_api().await);
    // with no module contributing a prompt the context is empty, and python's `[][-1]` raises
    let resp = h.get("/api/chat/prompt").await;
    assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(resp.body, "Internal Server Error");
}

#[tokio::test]
async fn chat_new_opens_a_chat_in_the_posted_category_or_general() {
    let h = harness(false).await;
    let created = h.send(post_json("/api/chat/new", r#"{"category": "  Travel  "}"#)).await;
    assert_eq!(created.status, StatusCode::OK);
    assert_eq!(created.json()["success"], json!(true));
    let id = created.json()["data"]["id"].as_str().expect("new chat id").to_string();
    let entry = index_entry(&h, &id);
    assert_eq!(entry["category"], json!("travel"));
    assert_eq!(entry["title"], json!("New chat"));

    // the reserved separator is refused by the shared category validator, and no chat is created
    let reserved = h.send(post_json("/api/chat/new", r#"{"category": "a:b"}"#)).await;
    assert_eq!(reserved.json(), json!({"data": "':' is reserved for subcategories", "success": false}));
    assert_eq!(h.ch.chat.data().len(), 1);

    // a body that is not a json object, or a category that is not a string, falls back to general
    for body in ["not json", "[]", r#"{"category": 5}"#, r#"{"category": "   "}"#] {
        let plain = h.send(post_json("/api/chat/new", body)).await;
        assert_eq!(plain.json()["success"], json!(true), "body {body}");
        let id = plain.json()["data"]["id"].as_str().expect("new chat id").to_string();
        assert_eq!(index_entry(&h, &id)["category"], json!("general"), "body {body}");
    }
}

#[tokio::test]
async fn chat_new_reads_bodies_python_json_loads_accepts_like_python() {
    let h = harness(false).await;
    // python's json.loads takes NaN in another key, so the category is still read
    let kept = h.send(post_json("/api/chat/new", r#"{"category": "work", "x": NaN, "y": -Infinity}"#)).await;
    assert_eq!(kept.json()["success"], json!(true));
    let id = kept.json()["data"]["id"].as_str().expect("new chat id").to_string();
    assert_eq!(index_entry(&h, &id)["category"], json!("work"));

    // a non-string category (nan is truthy, then fails at .strip()) and a non-object body keep 'general'
    for body in [r#"{"category": NaN}"#, r#"{"category": Infinity}"#, "NaN", "[NaN]"] {
        let plain = h.send(post_json("/api/chat/new", body)).await;
        assert_eq!(plain.json()["success"], json!(true), "body {body}");
        let id = plain.json()["data"]["id"].as_str().expect("new chat id").to_string();
        assert_eq!(index_entry(&h, &id)["category"], json!("general"), "body {body}");
    }
}

#[tokio::test]
async fn chat_rename_retitles_the_chat_and_reports_each_failure() {
    let h = harness(false).await;
    let id = chat_at(&h, "general", "old", "2026-11-01T10:00:00", &[]).await;
    let renamed = h.send(post_json(&format!("/api/chat/rename/{id}"), r#"{"title": "  Fresh title  "}"#)).await;
    assert_eq!(renamed.json(), json!({"data": {}, "success": true}));
    assert_eq!(index_entry(&h, &id)["title"], json!("Fresh title"));

    let cases = [
        (r#"{"title": "   "}"#, "Title cannot be empty"),
        ("{}", "Title cannot be empty"),
        (r#"{"title": null}"#, "'NoneType' object has no attribute 'strip'"),
        (r#"{"title": 5}"#, "'int' object has no attribute 'strip'"),
        ("[]", "'list' object has no attribute 'get'"),
        ("", "Expecting value: line 1 column 1 (char 0)"),
    ];
    for (body, message) in cases {
        let resp = h.send(post_json(&format!("/api/chat/rename/{id}"), body)).await;
        assert_eq!(resp.json(), json!({"data": message, "success": false}), "body {body}");
    }
    assert_eq!(index_entry(&h, &id)["title"], json!("Fresh title"));

    let missing = h.send(post_json("/api/chat/rename/missing", r#"{"title": "x"}"#)).await;
    assert_eq!(missing.json(), json!({"data": "Chat not found", "success": false}));
}

#[tokio::test]
async fn chat_delete_removes_the_chat_and_answers_the_empty_envelope() {
    let h = harness(false).await;
    let doomed = chat_at(&h, "general", "gone", "2026-11-01T10:00:00", &[]).await;
    let kept = chat_at(&h, "general", "kept", "2026-11-01T10:00:00", &[]).await;
    let resp = h.send(request(Method::POST, &format!("/api/chat/delete/{doomed}"), None, None, "")).await;
    assert_eq!(resp.json(), json!({"data": {}, "success": true}));
    let ids: Vec<String> = h.ch.chat.data().iter().map(|c| py_str(&c["id"])).collect();
    assert!(!ids.contains(&doomed));
    assert!(ids.contains(&kept));
    // python does not look the id up first, so deleting an unknown chat still succeeds
    let unknown = h.send(request(Method::POST, "/api/chat/delete/missing", None, None, "")).await;
    assert_eq!(unknown.json(), json!({"data": {}, "success": true}));
}

#[tokio::test]
async fn chat_delete_keeps_the_chat_when_its_history_file_cannot_be_removed() {
    let h = harness(false).await;
    let stuck = chat_at(&h, "general", "stuck", "2026-11-01T10:00:00", &[]).await;
    // a directory where the history file should be fails remove_file with an error other than not-found
    let path = ol_core::paths::get_data_path(Some(&format!("chats/webui/history/{stuck}.json"))).unwrap();
    let _ = std::fs::remove_file(&path);
    std::fs::create_dir_all(&path).unwrap();
    // python: ChatStore.delete raises before it pops the index, so the route is an unhandled 500
    let resp = h.send(request(Method::POST, &format!("/api/chat/delete/{stuck}"), None, None, "")).await;
    assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
    let ids: Vec<String> = h.ch.chat.data().iter().map(|c| py_str(&c["id"])).collect();
    assert!(ids.contains(&stuck));
}

#[tokio::test]
async fn chat_set_category_moves_the_chat_and_reports_each_failure() {
    let h = harness(false).await;
    let id = chat_at(&h, "work", "w", "2026-11-01T10:00:00", &[]).await;
    // python stores the stripped name as typed: no lowercasing and no subcategory validation on this path
    let moved = h.send(post_json(&format!("/api/chat/set-category/{id}"), r#"{"category": "  Home  "}"#)).await;
    assert_eq!(moved.json(), json!({"data": {}, "success": true}));
    assert_eq!(index_entry(&h, &id)["category"], json!("Home"));

    let cases = [
        ("{}", "Category cannot be empty"),
        (r#"{"category": "   "}"#, "Category cannot be empty"),
        (r#"{"category": 5}"#, "'int' object has no attribute 'strip'"),
        ("[]", "'list' object has no attribute 'get'"),
        ("", "Expecting value: line 1 column 1 (char 0)"),
    ];
    for (body, message) in cases {
        let resp = h.send(post_json(&format!("/api/chat/set-category/{id}"), body)).await;
        assert_eq!(resp.json(), json!({"data": message, "success": false}), "body {body}");
    }
    assert_eq!(index_entry(&h, &id)["category"], json!("Home"));

    let missing = h.send(post_json("/api/chat/set-category/missing", r#"{"category": "x"}"#)).await;
    assert_eq!(missing.json(), json!({"data": "Chat not found", "success": false}));
}

#[tokio::test]
async fn input_history_get_returns_the_saved_entries_oldest_first() {
    let h = harness(false).await;
    assert_eq!(h.get("/api/input_history").await.json(), json!({"data": [], "success": true}));
    for message in ["ls -la", "cd /tmp"] {
        let added = h.send(post_json("/api/input_history", &json!({"message": message}).to_string())).await;
        assert_eq!(added.status, StatusCode::OK);
    }
    assert_eq!(h.get("/api/input_history").await.json(), json!({"data": ["ls -la", "cd /tmp"], "success": true}));
}

#[tokio::test]
async fn input_history_add_reads_python_json_loads_bodies() {
    let h = harness(false).await;
    // python's json.loads takes NaN in another key, so the message is still appended
    let kept = h.send(post_json("/api/input_history", r#"{"message": "ls", "x": NaN, "y": -Infinity}"#)).await;
    assert_eq!(kept.json(), json!({"data": ["ls"], "success": true}));

    // a NaN message is truthy and has no .strip(), so python raises and the response is a plain 500
    for body in [r#"{"message": NaN}"#, r#"{"message": Infinity}"#, "NaN", "[NaN]"] {
        let resp = h.send(post_json("/api/input_history", body)).await;
        assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR, "body {body}");
    }
    assert_eq!(h.get("/api/input_history").await.json(), json!({"data": ["ls"], "success": true}));
}

#[tokio::test]
async fn chat_prompt_and_input_history_answer_head_and_other_methods_with_405_allow_get() {
    let h = harness(false).await;
    for uri in ["/api/chat/prompt", "/api/input_history"] {
        let resp = h.send(request(Method::HEAD, uri, None, None, "")).await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "HEAD {uri}");
        assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET", "HEAD {uri}");
        assert!(resp.body.is_empty(), "HEAD {uri} body: {}", resp.body);
    }
    let resp = h.send(request(Method::PUT, "/api/input_history", None, None, "")).await;
    assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET", "PUT input_history");
    let resp = h.send(request(Method::POST, "/api/chat/prompt", None, None, "")).await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET", "POST prompt");
}

#[tokio::test]
async fn chat_write_routes_answer_get_with_405_allow_post() {
    let h = harness(false).await;
    for uri in ["/api/chat/new", "/api/chat/rename/x", "/api/chat/delete/x", "/api/chat/set-category/x"] {
        let resp = h.get(uri).await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "GET {uri}");
        assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "POST", "GET {uri}");
        assert_eq!(resp.json(), json!({"detail": "Method Not Allowed"}), "GET {uri}");
    }
}

// ------------------
// reconnect and module libraries (channels/webui/channel.py api-6)
// ------------------

#[tokio::test]
async fn reconnect_without_an_api_setup_reports_the_api_error_envelope() {
    // python: API.reconnect() returns the APIError from connect(); the route answers success=False with its str().
    // a fresh python config merges the default api.url placeholder, so the test sets it explicitly
    let h = harness(false).await;
    set_api_url("http://API_URL_HERE/v1");
    let resp = h.send(request(Method::POST, "/api/reconnect", None, None, "")).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json(),
        json!({
            "data": "The API connection has not been set up yet! Please set up your API connection by either using the WebUI, the /config command, or editing the config file",
            "success": false,
        })
    );
    // @app.post only: GET gets starlette's 405 with the detail body and Allow: POST
    let resp = h.get("/api/reconnect").await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "POST");
    assert_eq!(resp.json(), json!({"detail": "Method Not Allowed"}));
}

#[tokio::test]
async fn libraries_status_lists_cloned_libraries_and_their_installable_modules() {
    // python: {"repos": list_repos(), "libraries": sorted library names, "modules": list_available()}
    let h = harness(false).await;
    let libs = h.ch.manager().module_manager().library_folder();
    let repo = libs.join("demo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let module = repo.join("ol_test_lib_hello_zz");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(module.join("module.py"), "class Hello(Module):\n    \"\"\"Says hello to people\"\"\"\n    pass\n").unwrap();
    // a folder without .git is not a library
    std::fs::create_dir_all(libs.join("not_a_repo")).unwrap();

    let resp = h.get("/api/libraries/status").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json(),
        json!({
            "data": {
                "repos": [],
                "libraries": ["demo"],
                "modules": [{
                    "library": "demo",
                    "name": "ol_test_lib_hello_zz",
                    "type": "module",
                    "doc": "Says hello to people",
                    "installed": false,
                }],
            },
            "success": true,
        })
    );
}

#[tokio::test]
async fn libraries_status_is_get_only_like_python() {
    // @app.get: HEAD is starlette's 405 with Allow: GET and an empty body, POST gets the detail body
    let h = harness(false).await;
    let resp = h.send(request(Method::HEAD, "/api/libraries/status", None, None, "")).await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET");
    assert!(resp.body.is_empty(), "HEAD body: {}", resp.body);
    let resp = h.send(request(Method::POST, "/api/libraries/status", None, None, "")).await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET");
    assert_eq!(resp.json(), json!({"detail": "Method Not Allowed"}));
}

#[tokio::test]
async fn libraries_sync_without_repos_reports_the_no_libraries_message() {
    // python returns this failure before any clone is attempted
    let h = harness(false).await;
    let resp = h.send(request(Method::POST, "/api/libraries/sync", None, None, "")).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json(),
        json!({
            "data": "No module libraries configured. Add github repo urls under Core settings first.",
            "success": false,
        })
    );
}

#[tokio::test]
async fn libraries_install_and_update_report_unknown_libraries_and_reject_bad_bodies() {
    let h = harness(false).await;
    let resp = h.send(post_json("/api/libraries/install", r#"{"library": "nope", "name": "x"}"#)).await;
    assert_eq!(resp.json(), json!({"data": "library 'nope' not found - sync it first", "success": false}));

    // python strips library/name; a missing or blank library is '' and reaches the same message
    let resp = h.send(post_json("/api/libraries/install", r#"{"name": "  "}"#)).await;
    assert_eq!(resp.json(), json!({"data": "library '' not found - sync it first", "success": false}));

    // update: a library that is not a configured repo
    let resp = h.send(post_json("/api/libraries/update", r#"{"library": "nope", "name": "x"}"#)).await;
    assert_eq!(
        resp.json(),
        json!({"data": "library 'nope' is not a configured repo - cannot update", "success": false})
    );

    // a non-object body, invalid json, or a non-string library are python's plain 500s
    for body in ["[1]", "not json", r#"{"library": 5, "name": "x"}"#] {
        for uri in ["/api/libraries/install", "/api/libraries/update"] {
            let resp = h.send(post_json(uri, body)).await;
            assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR, "{uri} {body}");
            assert_eq!(resp.body, "Internal Server Error", "{uri} {body}");
        }
    }
}

#[tokio::test]
async fn libraries_uninstall_reports_a_module_that_is_not_installed() {
    let h = harness(false).await;
    let resp = h.send(post_json("/api/libraries/uninstall", r#"{"name": "ol_test_not_installed_zz"}"#)).await;
    assert_eq!(
        resp.json(),
        json!({"data": "'ol_test_not_installed_zz' is not installed as a user module/channel", "success": false})
    );
    let resp = h.send(post_json("/api/libraries/uninstall", "[]")).await;
    assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(resp.body, "Internal Server Error");
}

#[tokio::test]
async fn library_bodies_read_python_json_loads_nan_like_python() {
    // python's json.loads takes NaN in a key the handler never reads; the name is read with `or ""` and `.strip()`
    let h = harness(false).await;
    let resp = h.send(post_json("/api/libraries/uninstall", r#"{"name": "ol_test_not_installed_zz", "junk": NaN}"#)).await;
    assert_eq!(
        resp.json(),
        json!({"data": "'ol_test_not_installed_zz' is not installed as a user module/channel", "success": false})
    );
    // a NaN name is a truthy non-string, so `.strip()` raises: a plain 500
    let resp = h.send(post_json("/api/libraries/uninstall", r#"{"name": NaN}"#)).await;
    assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn library_post_routes_answer_get_with_405_allow_post() {
    let h = harness(false).await;
    for uri in ["/api/libraries/sync", "/api/libraries/install", "/api/libraries/update", "/api/libraries/uninstall"] {
        let resp = h.get(uri).await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "GET {uri}");
        assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "POST", "GET {uri}");
        assert_eq!(resp.json(), json!({"detail": "Method Not Allowed"}), "GET {uri}");
    }
}

// ---------------- /api/system ----------------

/// writes the config keys the system routes read over the harness config
fn system_config(max_context: i64, threshold: f64, auto_compress: bool) {
    ol_core::config::update(|m| {
        m.insert("api".into(), json!({"max_context": max_context}));
        m.insert(
            "model".into(),
            json!({"context_compression_threshold": threshold, "automatically_compress_context": auto_compress}),
        );
    });
}

#[tokio::test]
async fn system_data_reports_the_configured_max_context() {
    let h = harness(false).await;
    system_config(32000, 0.25, true);
    let resp = h.get("/api/system/data").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json(), json!({"data": {"max_context": 32000}, "success": true}));
}

#[tokio::test]
async fn system_context_size_appends_threshold_percent_and_auto_compress_last() {
    let h = harness(false).await;
    system_config(32000, 0.255, true);
    let resp = h.get("/api/system/context_size").await;
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["success"], json!(true));
    let data = body["data"].as_object().expect("breakdown object");
    let keys: Vec<&str> = data.keys().map(|k| k.as_str()).collect();
    assert_eq!(
        keys,
        [
            "max_context",
            "total_tokens",
            "percent_full",
            "compaction_count",
            "total_words",
            "system_prompt",
            "tools",
            "message_history",
            "end_prompt",
            "compression_threshold",
            "auto_compress",
        ]
    );
    assert_eq!(data["max_context"], json!(32000));
    // python: round(0.255 * 100, 1) == 25.5 and bool(True) == true
    assert_eq!(data["compression_threshold"], json!(25.5));
    assert_eq!(data["auto_compress"], json!(true));
}

#[tokio::test]
async fn system_logs_lists_each_logged_entry_as_category_and_message() {
    let h = harness(false).await;
    // the webui channel's on_log hook fills this buffer; the harness does not run that hook, so fill it directly
    h.shared.logs.lock().push(("core".into(), "first line".into()));
    h.shared.logs.lock().push(("webui".into(), "second line".into()));
    let resp = h.get("/api/system/logs").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json(),
        json!({
            "data": [
                {"category": "core", "message": "first line"},
                {"category": "webui", "message": "second line"},
            ],
            "success": true,
        })
    );
}

#[tokio::test]
async fn system_restart_answers_null_and_logs_the_restart() {
    let h = harness(false).await;
    let resp = h.send(request(Method::POST, "/api/system/restart", None, None, "")).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.body, "null");
    assert_eq!(resp.json(), Value::Null);
}

#[tokio::test]
async fn system_routes_answer_wrong_methods_like_python() {
    let h = harness(false).await;
    for uri in ["/api/system/data", "/api/system/context_size", "/api/system/logs"] {
        let resp = h.send(request(Method::HEAD, uri, None, None, "")).await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "HEAD {uri}");
        assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET", "HEAD {uri}");
        assert!(resp.body.is_empty(), "HEAD {uri} body: {}", resp.body);
        let resp = h.send(request(Method::POST, uri, None, None, "")).await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "POST {uri}");
        assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "GET", "POST {uri}");
        assert_eq!(resp.json(), json!({"detail": "Method Not Allowed"}), "POST {uri}");
    }
    let resp = h.get("/api/system/restart").await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.headers.get(header::ALLOW).unwrap(), "POST");
}

// ---------------- /api/ext/calendar (upcoming, channels) ----------------

/// the calendar module loaded the way the manager loads it at startup, so its webui routes are reachable
async fn harness_with_calendar() -> Harness {
    let h = harness(false).await;
    let base = ModuleBase::new("calendar", false, Arc::downgrade(&h.manager));
    let calendar: Arc<dyn Module> = Arc::new(Calendar::new(base));
    calendar.on_ready().await.expect("calendar loads its events");
    h.manager.insert_module(calendar);
    h
}

/// `YYYY-MM-DD`, `days` after today in local time (the calendar's own clock)
fn day_from_today(days: i64) -> String {
    (chrono::Local::now().date_naive() + Duration::days(days)).format("%Y-%m-%d").to_string()
}

/// adds a 09:00 event without a notification through the module's own create route
async fn add_calendar_event(h: &Harness, title: &str, days: i64) {
    let body = json!({"title": title, "date": day_from_today(days), "time": "09:00", "notify": false}).to_string();
    let resp = h.send(post_json("/api/ext/calendar/create", &body)).await;
    assert_eq!(resp.json()["success"], json!(true), "create {title}: {}", resp.body);
}

/// the titles of the upcoming items, in the order the route returned them
fn upcoming_titles(resp: &Reply) -> Vec<String> {
    let body = resp.json();
    body["data"]
        .as_array()
        .unwrap_or_else(|| panic!("upcoming data is not a list: {}", resp.body))
        .iter()
        .map(|e| e["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn calendar_upcoming_lists_events_in_the_window_soonest_first() {
    let h = harness_with_calendar().await;
    add_calendar_event(&h, "Later", 10).await;
    add_calendar_event(&h, "Soon", 2).await;
    add_calendar_event(&h, "Far", 30).await;

    let resp = h.get("/api/ext/calendar/upcoming?days=14").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(upcoming_titles(&resp), ["Soon", "Later"]);
    let body = resp.json();
    let first = body["data"][0].as_object().expect("item object");
    let keys: Vec<&str> = first.keys().map(|k| k.as_str()).collect();
    assert_eq!(keys, ["id", "title", "date", "notify", "notify_channel"]);
    assert_eq!(first["notify"], json!(false));
    assert_eq!(first["notify_channel"], Value::Null);
    assert!(first["date"].as_str().unwrap_or_default().starts_with(&day_from_today(2)), "{body}");
    assert_eq!(body["success"], json!(true));
}

#[tokio::test]
async fn calendar_upcoming_defaults_bad_days_to_14_and_clamps_the_range_like_python() {
    let h = harness_with_calendar().await;
    add_calendar_event(&h, "Two", 2).await;
    add_calendar_event(&h, "Twenty", 20).await;

    // a non-integer or empty `days` is the default 14: day 20 is outside, day 2 inside
    assert_eq!(upcoming_titles(&h.get("/api/ext/calendar/upcoming?days=abc").await), ["Two"]);
    assert_eq!(upcoming_titles(&h.get("/api/ext/calendar/upcoming?days=").await), ["Two"]);
    // days below 1 clamps to 1: the day-2 event is outside a one-day window
    assert!(upcoming_titles(&h.get("/api/ext/calendar/upcoming?days=0").await).is_empty());
    // days above 365 clamps to 365, so both events are inside
    assert_eq!(upcoming_titles(&h.get("/api/ext/calendar/upcoming?days=9999").await), ["Two", "Twenty"]);
}

#[tokio::test]
async fn calendar_upcoming_is_get_only_like_the_python_webui_route() {
    let h = harness_with_calendar().await;
    let resp = h.send(post_json("/api/ext/calendar/upcoming", "{}")).await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.json(), json!({"data": "route only accepts GET", "success": false}));
}

#[tokio::test]
async fn calendar_channels_answers_the_available_channel_names() {
    let h = harness_with_calendar().await;
    let resp = h.get("/api/ext/calendar/channels").await;
    assert_eq!(resp.status, StatusCode::OK);
    let expected = ol_modules_data::scheduler::available_channels();
    assert_eq!(resp.json(), json!({"data": expected, "success": true}), "{}", resp.body);
}

// ---------------- /api/ext/characters (list, current, get, switch, switch_default) ----------------

use ol_modules_data::characters::Characters;

/// the characters module loaded the way the manager loads it at startup, so its webui routes are reachable
async fn harness_with_characters() -> Harness {
    characters_harness(true).await
}

async fn characters_harness(open_chat: bool) -> Harness {
    let h = harness(false).await;
    let base = ModuleBase::new("characters", false, Arc::downgrade(&h.manager));
    let characters: Arc<dyn Module> = Arc::new(Characters::new(base));
    characters.on_ready().await.expect("characters load");
    h.manager.insert_module(characters);
    if open_chat {
        // the character lives in the loaded chat's metadata, so a chat must be open (the app always has one)
        let opened = h.send(post_json("/api/chat/new", "{}")).await;
        assert_eq!(opened.json()["success"], json!(true), "open chat: {}", opened.body);
    }
    h
}

/// adds a character through the module's own add route
async fn add_character(h: &Harness, name: &str, category: &str, tags: &[&str], first_message: &str) {
    let body = json!({
        "name": name,
        "profile": "A test profile",
        "category": category,
        "tags": tags,
        "first_message": first_message,
    })
    .to_string();
    let resp = h.send(post_json("/api/ext/characters/add", &body)).await;
    assert_eq!(resp.json()["success"], json!(true), "add {name}: {}", resp.body);
}

#[tokio::test]
async fn characters_list_groups_by_category_with_general_default_and_name_sorted_members() {
    let h = harness_with_characters().await;
    add_character(&h, "bob", "wizards", &["mage"], "").await;
    add_character(&h, "Ann", "pirates", &["sea", "bold"], "").await;
    add_character(&h, "zed", "", &[], "").await;
    add_character(&h, "Cy", "pirates", &[], "").await;

    let resp = h.get("/api/ext/characters/list").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.json(),
        json!({
            "data": {"categories": [
                {"name": "general", "characters": [{"name": "zed", "tags": []}]},
                {"name": "pirates", "characters": [
                    {"name": "Ann", "tags": ["sea", "bold"]},
                    {"name": "Cy", "tags": []},
                ]},
                {"name": "wizards", "characters": [{"name": "bob", "tags": ["mage"]}]},
            ]},
            "success": true,
        }),
        "{}",
        resp.body
    );
}

#[tokio::test]
async fn characters_get_matches_names_case_and_space_insensitively_and_reports_a_miss() {
    let h = harness_with_characters().await;
    add_character(&h, "Ann", "pirates", &["sea"], "").await;

    let found = h.get("/api/ext/characters/get?name=%20ANN%20").await;
    assert_eq!(found.status, StatusCode::OK);
    let body = found.json();
    assert_eq!(body["success"], json!(true));
    assert_eq!(body["data"]["name"], json!("Ann"));
    assert_eq!(body["data"]["category"], json!("pirates"));
    assert_eq!(body["data"]["tags"], json!(["sea"]));

    assert_eq!(
        h.get("/api/ext/characters/get?name=nobody").await.json(),
        json!({"data": "character not found", "success": false})
    );
    // a missing name parameter is the empty name, which matches nothing
    assert_eq!(
        h.get("/api/ext/characters/get").await.json(),
        json!({"data": "character not found", "success": false})
    );
}

#[tokio::test]
async fn characters_switch_then_current_then_switch_default_round_trips_the_active_character() {
    let h = harness_with_characters().await;
    add_character(&h, "Ann", "pirates", &[], "").await;

    assert_eq!(h.get("/api/ext/characters/current").await.json(), json!({"data": {}, "success": true}));

    let switched = h.send(post_json("/api/ext/characters/switch", r#"{"name": "  ann  "}"#)).await;
    assert_eq!(switched.status, StatusCode::OK);
    assert_eq!(
        switched.json(),
        json!({"data": "Switch successful. Write your response as the character's first message.", "success": true})
    );
    // the active character is stored under the canonical key, not the spelling that was asked for
    assert_eq!(h.get("/api/ext/characters/current").await.json(), json!({"data": "Ann", "success": true}));

    let reset = h.send(post_json("/api/ext/characters/switch_default", "")).await;
    assert_eq!(reset.json(), json!({"data": "switched to default character", "success": true}));
    assert_eq!(h.get("/api/ext/characters/current").await.json(), json!({"data": {}, "success": true}));
}

#[tokio::test]
async fn characters_switch_reports_missing_name_and_unknown_character_like_python() {
    let h = harness_with_characters().await;
    add_character(&h, "Ann", "", &[], "").await;

    let no_name = h.send(post_json("/api/ext/characters/switch", "{}")).await;
    assert_eq!(no_name.json(), json!({"data": "error: no character name given", "success": false}));
    let empty = h.send(post_json("/api/ext/characters/switch", r#"{"name": ""}"#)).await;
    assert_eq!(empty.json(), json!({"data": "error: no character name given", "success": false}));
    let unknown = h.send(post_json("/api/ext/characters/switch", r#"{"name": "nobody"}"#)).await;
    assert_eq!(unknown.json(), json!({"data": "character not found", "success": false}));
    // a failed switch leaves the chat on the default character
    assert_eq!(h.get("/api/ext/characters/current").await.json(), json!({"data": {}, "success": true}));
}

#[tokio::test]
async fn characters_switch_with_first_messages_on_returns_the_greeting_to_stream_verbatim() {
    let h = harness_with_characters().await;
    add_character(&h, "Ann", "", &[], "Ahoy {{user}}, I am {{char}}.").await;
    ol_core::config::update(|m| {
        m.insert("modules".into(), json!({"settings": {"characters": {"use_first_messages": true}}}));
    });

    let resp = h.send(post_json("/api/ext/characters/switch", r#"{"name": "Ann"}"#)).await;
    assert_eq!(
        resp.json(),
        json!({
            "data": "Switched to Ann. Begin your reply now with this exact greeting, verbatim, and add nothing else:\n\nAhoy user, I am Ann.",
            "success": true,
        }),
        "{}",
        resp.body
    );
}

#[tokio::test]
async fn characters_current_and_switch_default_raise_python_errors_without_a_loaded_chat() {
    // python: chat.get("metadata") raises "No chat is currently loaded!" when no chat is current
    let h = characters_harness(false).await;
    assert_eq!(h.get("/api/ext/characters/current").await.json(), json!({"data": "No chat is currently loaded!", "success": false}));
    let resp = h.send(post_json("/api/ext/characters/switch_default", "")).await;
    assert_eq!(resp.json(), json!({"data": "No chat is currently loaded!", "success": false}));
}

#[tokio::test]
async fn characters_switch_default_fails_like_python_when_chat_metadata_is_not_a_dict() {
    // python: chat.get("metadata")["character"] = "" raises on a string metadata value
    let h = harness_with_characters().await;
    assert!(h.ch.chat.set("metadata", json!("oops"), None).expect("set metadata"));
    let resp = h.send(post_json("/api/ext/characters/switch_default", "")).await;
    assert_eq!(resp.json(), json!({"data": "'str' object does not support item assignment", "success": false}));
}

#[tokio::test]
async fn characters_routes_answer_wrong_methods_with_python_messages() {
    let h = harness_with_characters().await;
    let resp = h.get("/api/ext/characters/switch").await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.json(), json!({"data": "route only accepts POST", "success": false}));

    let resp = h.get("/api/ext/characters/switch_default").await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.json(), json!({"data": "route only accepts POST", "success": false}));

    let resp = h.send(post_json("/api/ext/characters/list", "{}")).await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.json(), json!({"data": "route only accepts GET", "success": false}));
    let resp = h.send(post_json("/api/ext/characters/current", "{}")).await;
    assert_eq!(resp.json(), json!({"data": "route only accepts GET", "success": false}));
}

#[tokio::test]
async fn characters_list_keeps_a_non_string_category_as_its_raw_value() {
    // python: `char.get("category") or "general"` keeps the imported number, so the JSON category is 5
    let h = harness_with_characters().await;
    let card = json!({"name": "Neo", "description": "The one", "category": 5}).to_string();
    let imported = h.send(post_json("/api/ext/characters/import", &json!({"json_code": card}).to_string())).await;
    assert_eq!(imported.json(), json!({"data": "character successfully imported", "success": true}), "{}", imported.body);

    assert_eq!(
        h.get("/api/ext/characters/list").await.json(),
        json!({"data": {"categories": [{"name": 5, "characters": [{"name": "Neo", "tags": []}]}]}, "success": true})
    );
}

#[tokio::test]
async fn characters_switch_rejects_non_string_names_and_non_object_bodies_like_python() {
    let h = harness_with_characters().await;
    // with no characters, python never calls name.lower(), so the name is just not found
    let empty = h.send(post_json("/api/ext/characters/switch", r#"{"name": 5}"#)).await;
    assert_eq!(empty.json(), json!({"data": "character not found", "success": false}));

    add_character(&h, "Ann", "", &[], "").await;
    let number = h.send(post_json("/api/ext/characters/switch", r#"{"name": 5}"#)).await;
    assert_eq!(number.json(), json!({"data": "'int' object has no attribute 'lower'", "success": false}));
    let boolean = h.send(post_json("/api/ext/characters/switch", r#"{"name": true}"#)).await;
    assert_eq!(boolean.json(), json!({"data": "'bool' object has no attribute 'lower'", "success": false}));
    let list = h.send(post_json("/api/ext/characters/switch", r#"{"name": ["Ann"]}"#)).await;
    assert_eq!(list.json(), json!({"data": "'list' object has no attribute 'lower'", "success": false}));

    // a truthy non-object body fails on `.get`, before the name is looked at
    let array = h.send(post_json("/api/ext/characters/switch", "[1]")).await;
    assert_eq!(array.json(), json!({"data": "'list' object has no attribute 'get'", "success": false}));
    let string = h.send(post_json("/api/ext/characters/switch", r#""Ann""#)).await;
    assert_eq!(string.json(), json!({"data": "'str' object has no attribute 'get'", "success": false}));
    // a falsy body is `{}`, so the missing name is reported as usual
    let falsy = h.send(post_json("/api/ext/characters/switch", "0")).await;
    assert_eq!(falsy.json(), json!({"data": "error: no character name given", "success": false}));
}

#[tokio::test]
async fn characters_lookups_strip_python_separator_whitespace() {
    // python str.strip() also removes U+001C..U+001F, which rust's trim() keeps
    let h = harness_with_characters().await;
    add_character(&h, "Ann", "", &[], "").await;

    let found = h.get("/api/ext/characters/get?name=%1cANN%1f").await;
    assert_eq!(found.json()["data"]["name"], json!("Ann"), "{}", found.body);

    let switched = h.send(post_json("/api/ext/characters/switch", "{\"name\": \"\\u001cann\"}")).await;
    assert_eq!(switched.json()["success"], json!(true), "{}", switched.body);
    assert_eq!(h.get("/api/ext/characters/current").await.json(), json!({"data": "Ann", "success": true}));
}


// ---------------- /api/ext/characters (add, edit, all, delete, remove, import) ----------------

#[tokio::test]
async fn characters_add_validates_then_stores_a_stripped_character_in_python_shape() {
    let h = harness_with_characters().await;
    let add = |body: &str| post_json("/api/ext/characters/add", body);

    let blank_name = h.send(add(r#"{"name": "   ", "profile": "p"}"#)).await;
    assert_eq!(blank_name.json(), json!({"data": "character name cannot be empty", "success": false}));
    let blank_profile = h.send(add(r#"{"name": "Ann", "profile": " \n "}"#)).await;
    assert_eq!(blank_profile.json(), json!({"data": "character profile must not be blank", "success": false}));

    // name, profile, scenario and tags are stripped; a comma string becomes a tag list without empties
    let added = h
        .send(add(r#"{"name": "  Ann  ", "profile": " A profile ", "scenario": "Cafe", "category": " ", "tags": "a, b,, c "}"#))
        .await;
    assert_eq!(added.json(), json!({"data": "character Ann added", "success": true}), "{}", added.body);

    let dup = h.send(add(r#"{"name": "ann", "profile": "p"}"#)).await;
    assert_eq!(dup.json(), json!({"data": "a character with that name already exists", "success": false}));

    assert_eq!(
        h.get("/api/ext/characters/all").await.json(),
        json!({"data": [{
            "key": "Ann",
            "name": "Ann",
            "profile": "A profile",
            "scenario": "Cafe",
            "first_message": "",
            "post_history": "",
            "category": "",
            "tags": ["a", "b", "c"]
        }], "success": true})
    );
}

#[tokio::test]
async fn characters_all_sorts_by_lowercase_name_and_keeps_python_key_order() {
    let h = harness_with_characters().await;
    add_character(&h, "bob", "", &[], "").await;
    add_character(&h, "Ann", "", &[], "").await;
    add_character(&h, "cara", "", &[], "").await;

    let resp = h.get("/api/ext/characters/all").await;
    let data = resp.json()["data"].clone();
    let names: Vec<&str> = data.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Ann", "bob", "cara"]);
    let keys: Vec<&str> = data[0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["key", "name", "profile", "scenario", "first_message", "post_history", "category", "tags"]);
}

#[tokio::test]
async fn characters_edit_renames_restamps_the_chat_and_clears_fields_left_out() {
    let h = harness_with_characters().await;
    add_character(&h, "Ann", "", &[], "Hello there").await;
    add_character(&h, "Bob", "", &[], "").await;
    let switched = h.send(post_json("/api/ext/characters/switch", r#"{"name": "Ann"}"#)).await;
    assert_eq!(switched.json()["success"], json!(true), "{}", switched.body);

    let missing = h.send(post_json("/api/ext/characters/edit", r#"{"key": "nobody", "name": "x", "profile": "p"}"#)).await;
    assert_eq!(missing.json(), json!({"data": "character not found", "success": false}));
    let blank = h.send(post_json("/api/ext/characters/edit", r#"{"key": "Ann", "name": " ", "profile": "p"}"#)).await;
    assert_eq!(blank.json(), json!({"data": "character name cannot be empty", "success": false}));
    let no_profile = h.send(post_json("/api/ext/characters/edit", r#"{"key": "Ann", "name": "Ann", "profile": ""}"#)).await;
    assert_eq!(no_profile.json(), json!({"data": "character profile must not be blank", "success": false}));
    let clash = h.send(post_json("/api/ext/characters/edit", r#"{"key": "Ann", "name": "BOB", "profile": "p"}"#)).await;
    assert_eq!(clash.json(), json!({"data": "a character with that name already exists", "success": false}));

    // a rename re-keys the entry and re-stamps the open chat that used it
    let renamed = h
        .send(post_json("/api/ext/characters/edit", r#"{"key": "Ann", "name": "Anna", "profile": "New profile", "tags": "x"}"#))
        .await;
    assert_eq!(renamed.json(), json!({"data": "character edited", "success": true}), "{}", renamed.body);
    assert_eq!(h.get("/api/ext/characters/current").await.json(), json!({"data": "Anna", "success": true}));
    assert_eq!(
        h.get("/api/ext/characters/get?name=anna").await.json(),
        json!({"data": {"name": "Anna", "profile": "New profile", "scenario": "", "first_message": "", "post_history": "", "category": null, "tags": ["x"]}, "success": true})
    );

    // a case-only rename is not a clash with itself, and an omitted first_message is cleared like python
    let case_only = h.send(post_json("/api/ext/characters/edit", r#"{"key": "Anna", "name": "anna", "profile": "P"}"#)).await;
    assert_eq!(case_only.json(), json!({"data": "character edited", "success": true}));
    assert_eq!(h.get("/api/ext/characters/current").await.json(), json!({"data": "anna", "success": true}));
    assert_eq!(h.get("/api/ext/characters/get?name=anna").await.json()["data"]["first_message"], json!(""));
}

#[tokio::test]
async fn characters_delete_and_remove_report_their_python_messages() {
    let h = harness_with_characters().await;
    add_character(&h, "Ann", "", &[], "").await;
    add_character(&h, "Bob", "", &[], "").await;

    let deleted = h.send(post_json("/api/ext/characters/delete", r#"{"name": " ann "}"#)).await;
    assert_eq!(deleted.json(), json!({"data": "deleted character Ann", "success": true}), "{}", deleted.body);
    let again = h.send(post_json("/api/ext/characters/delete", r#"{"name": "Ann"}"#)).await;
    assert_eq!(again.json(), json!({"data": "character not found", "success": false}));

    // remove reports through the tool's delete(): its own wording, and a success flag of false when missing
    let removed = h.send(post_json("/api/ext/characters/remove", r#"{"name": "BOB"}"#)).await;
    assert_eq!(removed.json(), json!({"data": "character Bob deleted", "success": true}), "{}", removed.body);
    let missing = h.send(post_json("/api/ext/characters/remove", r#"{"name": "Bob"}"#)).await;
    assert_eq!(missing.json(), json!({"data": "character doesn't exist!", "success": false}));
    assert_eq!(h.get("/api/ext/characters/all").await.json(), json!({"data": [], "success": true}));
}

#[tokio::test]
async fn characters_delete_rejects_non_string_names_and_non_object_bodies_like_python() {
    let h = harness_with_characters().await;
    // with no characters the name is never lowercased, so it is simply not found
    let empty = h.send(post_json("/api/ext/characters/delete", r#"{"name": null}"#)).await;
    assert_eq!(empty.json(), json!({"data": "character not found", "success": false}));

    add_character(&h, "Ann", "", &[], "").await;
    let number = h.send(post_json("/api/ext/characters/delete", r#"{"name": 5}"#)).await;
    assert_eq!(number.json(), json!({"data": "'int' object has no attribute 'lower'", "success": false}));
    let null = h.send(post_json("/api/ext/characters/delete", r#"{"name": null}"#)).await;
    assert_eq!(null.json(), json!({"data": "'NoneType' object has no attribute 'lower'", "success": false}));
    // a missing name is "" and simply matches nothing
    let missing = h.send(post_json("/api/ext/characters/delete", "{}")).await;
    assert_eq!(missing.json(), json!({"data": "character not found", "success": false}));
    let array = h.send(post_json("/api/ext/characters/remove", "[1]")).await;
    assert_eq!(array.json(), json!({"data": "'list' object has no attribute 'get'", "success": false}));
    let add_array = h.send(post_json("/api/ext/characters/add", "[1]")).await;
    assert_eq!(add_array.json(), json!({"data": "'list' object has no attribute 'get'", "success": false}));
}

#[tokio::test]
async fn characters_import_accepts_json_text_or_objects_and_reports_python_errors() {
    let h = harness_with_characters().await;
    let card = json!({"name": "Neo", "description": "The one", "tags": ["a"]});

    // a card object is dumped back to text, so it imports the same as its json text
    let as_object = h.send(post_json("/api/ext/characters/import", &json!({"json_code": card}).to_string())).await;
    assert_eq!(as_object.json(), json!({"data": "character successfully imported", "success": true}), "{}", as_object.body);
    let updated = json!({"name": "Neo", "description": "Reworked"}).to_string();
    let as_text = h.send(post_json("/api/ext/characters/import", &json!({"json_code": updated}).to_string())).await;
    assert_eq!(as_text.json(), json!({"data": "character successfully imported", "success": true}));

    // re-importing updates the existing character in place
    let all = h.get("/api/ext/characters/all").await.json();
    assert_eq!(all["data"].as_array().unwrap().len(), 1);
    assert_eq!(all["data"][0]["profile"], json!("Reworked"));
    assert_eq!(all["data"][0]["tags"], json!([]));

    // python: json.loads of a non-str raises its TypeError, a missing json_code is the empty text
    let null = h.send(post_json("/api/ext/characters/import", r#"{"json_code": null}"#)).await;
    assert_eq!(null.json(), json!({"data": "error: the JSON object must be str, bytes or bytearray, not NoneType", "success": false}));
    let number = h.send(post_json("/api/ext/characters/import", r#"{"json_code": 5}"#)).await;
    assert_eq!(number.json(), json!({"data": "error: the JSON object must be str, bytes or bytearray, not int", "success": false}));

    // an empty dict, a non-dict document, and a card with no profile are each rejected
    let empty = h.send(post_json("/api/ext/characters/import", r#"{"json_code": "{}"}"#)).await;
    assert_eq!(empty.json(), json!({"data": "error: character card was empty", "success": false}));
    let list = h.send(post_json("/api/ext/characters/import", r#"{"json_code": "[1]"}"#)).await;
    assert_eq!(list.json(), json!({"data": "error: character card was empty", "success": false}));
    let no_profile = h.send(post_json("/api/ext/characters/import", r#"{"json_code": "{\"name\": \"X\"}"}"#)).await;
    assert_eq!(
        no_profile.json(),
        json!({"data": "error: failed to extract a usable name and profile from this character card", "success": false})
    );
}

#[tokio::test]
async fn characters_import_rejects_an_unhashable_name_on_an_empty_store_like_python() {
    let h = harness_with_characters().await;
    // python: a truthy list or dict name passes the usable-card check, then `self.characters[name] = ...` raises
    let list = h
        .send(post_json("/api/ext/characters/import", r#"{"json_code": "{\"name\": [\"a\"], \"profile\": \"x\"}"}"#))
        .await;
    assert_eq!(list.json(), json!({"data": "unhashable type: 'list'", "success": false}));
    let dict = h
        .send(post_json("/api/ext/characters/import", r#"{"json_code": "{\"name\": {\"a\": 1}, \"profile\": \"x\"}"}"#))
        .await;
    assert_eq!(dict.json(), json!({"data": "unhashable type: 'dict'", "success": false}));
    // nothing was stored
    assert_eq!(h.get("/api/ext/characters/all").await.json(), json!({"data": [], "success": true}));
}

// ---------------- /api/ext/memory (tags, list, list_pinned, search, pin, unpin) ----------------

use ol_modules_data::memory::Memory;

/// the memory module loaded the way the manager loads it at startup. the typed handle is returned so a test
/// can seed memories through the module's own create method
async fn harness_with_memory() -> (Harness, Arc<Memory>) {
    let h = harness(false).await;
    let base = ModuleBase::new("memory", false, Arc::downgrade(&h.manager));
    let memory = Arc::new(Memory::new(base));
    memory.on_ready().await.expect("memory load");
    h.manager.insert_module(memory.clone());
    (h, memory)
}

/// adds a memory with create() and returns its id, taken from the "memory added. ID: <id>" answer
async fn seed(memory: &Memory, content: &str, tags: &[&str], pinned: bool) -> String {
    let answer = memory.create(json!(content), json!(tags), json!(pinned)).await.expect("create");
    answer["content"].as_str().expect("create answer").trim_start_matches("memory added. ID: ").to_string()
}

/// the `id` of every item in a route's data array, in order
fn ids(data: &Value) -> Vec<String> {
    data.as_array().expect("data array").iter().map(|i| i["id"].as_str().expect("id").to_string()).collect()
}

#[tokio::test]
async fn memory_list_is_newest_first_with_the_sidebar_shape_tag_filter_and_limits() {
    let (h, memory) = harness_with_memory().await;
    let a = seed(&memory, "likes tea", &["Food"], false).await;
    let b = seed(&memory, "owns a cat", &["pets"], true).await;
    let c = seed(&memory, "loves tea too", &["food", "Work"], false).await;

    let resp = h.get("/api/ext/memory/list").await;
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["success"], json!(true), "{}", resp.body);
    assert_eq!(ids(&body["data"]), vec![c.clone(), b.clone(), a.clone()], "{}", resp.body);
    let newest = &body["data"][0];
    assert_eq!(newest["content"], json!("loves tea too"));
    assert_eq!(newest["tags"], json!(["food", "Work"]));
    assert_eq!(newest["pinned"], json!(false));
    assert!(newest["date_created"].is_string(), "{}", resp.body);
    // a plain list carries no score key
    assert!(newest.get("score").is_none(), "{}", resp.body);
    assert_eq!(body["data"][1]["pinned"], json!(true));

    // the tag filter is an exact, case-insensitive match on a whole tag, not a substring
    assert_eq!(ids(&h.get("/api/ext/memory/list?tag=FOOD").await.json()["data"]), vec![c.clone(), a.clone()]);
    assert_eq!(h.get("/api/ext/memory/list?tag=wor").await.json(), json!({"data": [], "success": true}));

    // limit: clamped to 1..=500, and a value python's int() rejects falls back to the default of 100
    assert_eq!(ids(&h.get("/api/ext/memory/list?limit=2").await.json()["data"]), vec![c.clone(), b.clone()]);
    assert_eq!(ids(&h.get("/api/ext/memory/list?limit=0").await.json()["data"]), vec![c.clone()]);
    assert_eq!(ids(&h.get("/api/ext/memory/list?limit=abc").await.json()["data"]).len(), 3);
}

#[tokio::test]
async fn memory_list_pinned_returns_only_pinned_newest_first_and_treats_limit_zero_as_unlimited() {
    let (h, memory) = harness_with_memory().await;
    let first = seed(&memory, "first pin", &[], true).await;
    seed(&memory, "not pinned", &[], false).await;
    let second = seed(&memory, "second pin", &["x"], true).await;

    let resp = h.get("/api/ext/memory/list_pinned").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(ids(&resp.json()["data"]), vec![second.clone(), first.clone()], "{}", resp.body);
    assert_eq!(ids(&h.get("/api/ext/memory/list_pinned?limit=1").await.json()["data"]), vec![second.clone()]);
    // python: limit 0 (or anything int() rejects) never stops early
    assert_eq!(ids(&h.get("/api/ext/memory/list_pinned?limit=abc").await.json()["data"]).len(), 2);
    // python stops after the first item for any non-zero limit below 1, since len() >= limit already holds
    assert_eq!(ids(&h.get("/api/ext/memory/list_pinned?limit=-1").await.json()["data"]), vec![second]);
}

#[tokio::test]
async fn memory_pin_and_unpin_flip_the_flag_and_answer_with_python_messages() {
    let (h, memory) = harness_with_memory().await;
    let id = seed(&memory, "remember me", &[], false).await;
    assert_eq!(h.get("/api/ext/memory/list_pinned").await.json(), json!({"data": [], "success": true}));

    let pinned = h.send(post_json("/api/ext/memory/pin", &json!({"id": id}).to_string())).await;
    assert_eq!(pinned.status, StatusCode::OK);
    assert_eq!(pinned.json(), json!({"data": format!("memory {id} pinned"), "success": true}), "{}", pinned.body);
    assert_eq!(ids(&h.get("/api/ext/memory/list_pinned").await.json()["data"]), vec![id.clone()]);
    assert_eq!(h.get("/api/ext/memory/list").await.json()["data"][0]["pinned"], json!(true));

    let unpinned = h.send(post_json("/api/ext/memory/unpin", &json!({"id": id}).to_string())).await;
    assert_eq!(unpinned.json(), json!({"data": format!("memory {id} unpinned"), "success": true}), "{}", unpinned.body);
    assert_eq!(h.get("/api/ext/memory/list_pinned").await.json(), json!({"data": [], "success": true}));
    assert_eq!(h.get("/api/ext/memory/list").await.json()["data"][0]["pinned"], json!(false));

    // a missing or empty id is a failed envelope; an unknown id is python's pin()/unpin() answer, which
    // is self.result(...) with the default success=True. all of them answer 200
    assert_eq!(h.send(post_json("/api/ext/memory/pin", "{}")).await.json(), json!({"data": "no memory id given", "success": false}));
    assert_eq!(
        h.send(post_json("/api/ext/memory/unpin", r#"{"id": ""}"#)).await.json(),
        json!({"data": "no memory id given", "success": false})
    );
    let unknown = h.send(post_json("/api/ext/memory/pin", r#"{"id": "nope"}"#)).await;
    assert_eq!(unknown.status, StatusCode::OK);
    assert_eq!(unknown.json(), json!({"data": "memory with that ID not found!", "success": true}));
}

#[tokio::test]
async fn memory_search_ranks_hits_with_two_decimal_scores_and_honours_search_content() {
    let (h, memory) = harness_with_memory().await;
    let pasta = seed(&memory, "pasta night on friday", &["dinner"], false).await;
    let gym = seed(&memory, "gym on monday", &["fitness"], false).await;

    let resp = h.get("/api/ext/memory/search?q=pasta").await;
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["success"], json!(true), "{}", resp.body);
    assert_eq!(ids(&body["data"]), vec![pasta.clone()], "{}", resp.body);
    let score = body["data"][0]["score"].as_f64().expect("score");
    assert_eq!((score * 100.0).round() / 100.0, score, "score is rounded to 2 places: {}", resp.body);
    assert_eq!(body["data"][0]["content"], json!("pasta night on friday"));

    // with search_content off only tags are weighted: a content-only word finds nothing, a tag still matches
    assert_eq!(h.get("/api/ext/memory/search?q=pasta&search_content=no").await.json(), json!({"data": [], "success": true}));
    assert_eq!(ids(&h.get("/api/ext/memory/search?q=dinner&search_content=no").await.json()["data"]), vec![pasta.clone()]);

    // the tag filter runs before ranking, a blank query answers an empty list, and so does a miss
    assert_eq!(ids(&h.get("/api/ext/memory/search?q=monday&tag=fitness").await.json()["data"]), vec![gym]);
    assert_eq!(ids(&h.get("/api/ext/memory/search?q=monday&tag=dinner").await.json()["data"]), Vec::<String>::new());
    assert_eq!(h.get("/api/ext/memory/search?q=%20%20").await.json(), json!({"data": [], "success": true}));
    assert_eq!(h.get("/api/ext/memory/search?q=zebra").await.json(), json!({"data": [], "success": true}));
}

#[tokio::test]
async fn memory_tags_are_unique_sorted_case_insensitively_and_scoped_by_tab() {
    let (h, memory) = harness_with_memory().await;
    seed(&memory, "pasta night", &["Food", "zeta"], false).await;
    seed(&memory, "gym", &["Food", "apple"], false).await;
    seed(&memory, "deadline", &["Work"], true).await;

    assert_eq!(
        h.get("/api/ext/memory/tags").await.json(),
        json!({"data": ["apple", "Food", "Work", "zeta"], "success": true})
    );
    assert_eq!(h.get("/api/ext/memory/tags?tab=pinned").await.json(), json!({"data": ["Work"], "success": true}));
    assert_eq!(
        h.get("/api/ext/memory/tags?tab=search&q=pasta").await.json(),
        json!({"data": ["Food", "zeta"], "success": true})
    );
    // tab=search without a query falls back to every memory
    assert_eq!(
        h.get("/api/ext/memory/tags?tab=search").await.json(),
        json!({"data": ["apple", "Food", "Work", "zeta"], "success": true})
    );
}

#[tokio::test]
async fn memory_routes_answer_wrong_methods_with_python_messages() {
    let (h, _memory) = harness_with_memory().await;
    let resp = h.get("/api/ext/memory/pin").await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.json(), json!({"data": "route only accepts POST", "success": false}));

    let resp = h.send(post_json("/api/ext/memory/list", "{}")).await;
    assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.json(), json!({"data": "route only accepts GET", "success": false}));
    let resp = h.send(post_json("/api/ext/memory/search", "{}")).await;
    assert_eq!(resp.json(), json!({"data": "route only accepts GET", "success": false}));
}

// ---------------- /api/ext/memory (update, delete) ----------------

#[tokio::test]
async fn memory_update_strips_content_and_tags_and_answers_with_the_python_message() {
    // python _route_update: str(content).strip(), tags stringified and stripped, empty tags dropped
    let (h, memory) = harness_with_memory().await;
    let id = seed(&memory, "likes tea", &["food"], false).await;

    let body = json!({"id": id, "content": "  likes green tea  ", "tags": [" tea ", "", 7]});
    let resp = h.send(post_json("/api/ext/memory/update", &body.to_string())).await;
    assert_eq!(resp.json(), json!({"data": format!("memory {id} edited"), "success": true}), "{}", resp.body);

    let listed = h.get("/api/ext/memory/list").await.json();
    assert_eq!(listed["data"][0]["content"], json!("likes green tea"));
    assert_eq!(listed["data"][0]["tags"], json!(["tea", "7"]));
}

#[tokio::test]
async fn memory_update_refuses_missing_empty_and_unknown_targets_like_python() {
    let (h, memory) = harness_with_memory().await;
    let id = seed(&memory, "keep me", &[], false).await;

    assert_eq!(
        h.send(post_json("/api/ext/memory/update", r#"{"content": "x"}"#)).await.json(),
        json!({"data": "no memory id given", "success": false})
    );
    // a missing or whitespace-only content is refused before the id is looked up
    assert_eq!(
        h.send(post_json("/api/ext/memory/update", &json!({"id": id}).to_string())).await.json(),
        json!({"data": "memory content cannot be empty", "success": false})
    );
    assert_eq!(
        h.send(post_json("/api/ext/memory/update", &json!({"id": id, "content": "  \t "}).to_string())).await.json(),
        json!({"data": "memory content cannot be empty", "success": false})
    );
    assert_eq!(
        h.send(post_json("/api/ext/memory/update", r#"{"id": "nope", "content": "x"}"#)).await.json(),
        json!({"data": "memory with that ID not found!", "success": false})
    );
    // a refused update leaves the stored memory untouched
    assert_eq!(h.get("/api/ext/memory/list").await.json()["data"][0]["content"], json!("keep me"));
}

#[tokio::test]
async fn memory_update_and_delete_are_post_only_like_python() {
    let (h, _memory) = harness_with_memory().await;
    for path in ["update", "delete"] {
        let resp = h.get(&format!("/api/ext/memory/{path}")).await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
        assert_eq!(resp.json(), json!({"data": "route only accepts POST", "success": false}), "{path}");
    }
}

#[tokio::test]
async fn memory_delete_removes_the_memory_and_answers_with_python_messages() {
    let (h, memory) = harness_with_memory().await;
    let keep = seed(&memory, "keep", &[], false).await;
    let gone = seed(&memory, "gone", &[], false).await;

    // ids are matched after strip(), but the answer echoes the id as it was sent
    let padded = format!(" {gone} ");
    let resp = h.send(post_json("/api/ext/memory/delete", &json!({"id": padded}).to_string())).await;
    assert_eq!(resp.json(), json!({"data": format!("memory  {gone}  deleted"), "success": true}), "{}", resp.body);
    assert_eq!(ids(&h.get("/api/ext/memory/list").await.json()["data"]), vec![keep.clone()]);

    // an unknown id is a success answer with the not-found text, as in python's delete()
    let unknown = h.send(post_json("/api/ext/memory/delete", r#"{"id": "nope"}"#)).await;
    assert_eq!(unknown.json(), json!({"data": "memory with that ID not found!", "success": true}));
    assert_eq!(
        h.send(post_json("/api/ext/memory/delete", "{}")).await.json(),
        json!({"data": "no memory id given", "success": false})
    );
    assert_eq!(ids(&h.get("/api/ext/memory/list").await.json()["data"]), vec![keep]);
}
