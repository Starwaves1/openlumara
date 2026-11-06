//! Oneshot tests of the webui page and api routes (`/`, `/login`, `/logout`, `/api/chat/load/{id}`,
//! `/api/ext/{module}/{path}`). Each test drives the real axum router built by `build_router`, against a
//! channel whose config and data folder live in a temp dir.
//!
//! The config is process-global, so every harness holds `HARNESS` for its whole lifetime.

use crate::server::{build_router, build_shared};
use async_trait::async_trait;
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
    Harness { router: build_router(shared), manager, ch, _lock: lock, _data: data }
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
