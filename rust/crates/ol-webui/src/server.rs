//! Port of channels/webui/channel.py: the `Webui` channel (on_ready / run / on_push / on_stream /
//! on_log / on_shutdown) and `create_fastapi` (auth + admin-lockdown middleware, session cookie,
//! COOP/COEP headers, the page routes `/`, `/login`, `/logout`, and the router assembly).
//!
//! Route groups live next to this file: `ws` (`/ws`), `static_routes` (`/assets`, `/ext-assets`,
//! `/themes`, `/api/themes`, `/sw.js`, manifest, icons, favicon) and `ui_routes` (`/api/ext/...`).
//! The chat / settings / system API is added by `routes::add_api_routes`.

#[path = "../../../generated/channels/webui.rs"]
pub mod meta;

use crate::routes;
use crate::session::{self, LoginLimiter, SessionSigner, COOKIE_NAME, DEFAULT_SECRET, INVALID_CREDENTIALS};
use crate::static_routes;
use crate::templates::{discover_webui_dirs, ui_asset_urls, ui_extension_enabled, ExtHost, TemplateStore};
use crate::ui_routes;
use crate::util::{bind_host, channel_url};
use crate::ws::{self, WsManager};
use anyhow::anyhow;
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Json;
use axum::Router;
use ol_core::channel::{Channel, ChannelBehavior, TokenStream};
use ol_core::module::Module;
use ol_core::paths::get_path;
use ol_core::pyjson::{py_str, truthy};
use ol_core::registry::{module_settings, ChannelDescriptor};
use ol_core::storage::{StorageList, StorageType};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, watch};

/// python `Webui.settings` is read through the generated meta; the class-level metadata comes from there
pub fn descriptor() -> ChannelDescriptor {
    ChannelDescriptor {
        name: "webui",
        class_name: meta::CLASS_NAME,
        doc: meta::DOC,
        settings: meta::settings,
        unsafe_: meta::UNSAFE,
        category: meta::CATEGORY,
        factory,
    }
}

fn factory() -> Arc<dyn ChannelBehavior> {
    Arc::new(Webui::default())
}

/// python `api_result(obj=None, success=True)`: `None` becomes `{}`
pub fn api_result(data: Value, success: bool) -> Value {
    let data = if data.is_null() { json!({}) } else { data };
    json!({"data": data, "success": success})
}

/// FastAPI's `HTTPException` / default error body
pub fn detail_response(status: StatusCode, detail: &str) -> Response {
    (status, Json(json!({"detail": detail}))).into_response()
}

/// starlette's plain-text 500 (an unhandled exception in a route)
pub fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

/// python `ADMIN_ONLY_ENDPOINTS`: blocked outright when `enable_admin_settings` is off
pub const ADMIN_ONLY_ENDPOINTS: [&str; 15] = [
    "/api/settings/load",
    "/api/settings/save",
    "/api/check_connection",
    "/api/models",
    "/api/model/set",
    "/api/reconnect",
    "/api/chat/prompt",
    "/api/system/logs",
    "/api/system/restart",
    "/api/system/context_size",
    "/api/libraries/status",
    "/api/libraries/sync",
    "/api/libraries/install",
    "/api/libraries/update",
    "/api/libraries/uninstall",
];

/// python `Webui._ui_extension_enabled(module)`
pub fn module_ui_enabled(module: &Arc<dyn Module>) -> bool {
    let declared = module_settings(&module.base().name);
    let configured = module.base().config.get1("enable_webui_extension");
    ui_extension_enabled(configured.as_ref(), declared.get("enable_webui_extension"))
}

/// python `_ui_extension_dirs` (sub "templates") and `_ui_asset_dirs` (sub "assets"): every enabled,
/// extension-enabled module's `webui/<sub>` folder, keyed by module name
pub fn ui_dirs(ch: &Arc<Channel>, sub: &str) -> BTreeMap<String, PathBuf> {
    let root = ol_core::paths::project_root();
    let modules_paths = [root.join("modules")];
    let user_paths = [root.join("user_modules")];
    let modules_enabled = ol_core::config::get_str_list(&["modules", "enabled"]);
    let user_enabled = ol_core::config::get_str_list(&["user_modules", "enabled"]);
    let manager = ch.manager();
    discover_webui_dirs(
        &[(modules_paths.as_slice(), modules_enabled.as_slice()), (user_paths.as_slice(), user_enabled.as_slice())],
        sub,
        |name| manager.module(name).map(|m| module_ui_enabled(&m)).unwrap_or(false),
    )
}

/// `ExtHost` for the template system: module settings and logs go through the running channel
struct WebuiHost {
    ch: Weak<Channel>,
}

impl ExtHost for WebuiHost {
    fn module_config(&self, module: &str) -> Value {
        let Some(ch) = self.ch.upgrade() else { return Value::Null };
        let Some(m) = ch.manager().module(module) else { return Value::Null };
        let names: Vec<String> = module_settings(module).as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
        Value::Object(names.into_iter().map(|n| {
            let v = m.base().config.get1(&n).unwrap_or(Value::Null);
            (n, v)
        }).collect::<Map<String, Value>>())
    }

    fn log(&self, category: &str, message: &str) {
        if let Some(ch) = self.ch.upgrade() {
            ch.log(category, message);
        }
    }
}

/// state shared by every route and by the channel hooks (python `self` of the Webui instance)
pub struct Shared {
    ch: Weak<Channel>,
    pub webui_dir: PathBuf,
    pub templates: TemplateStore,
    pub ws: WsManager,
    pub login: Mutex<LoginLimiter>,
    pub signer: SessionSigner,
    /// python `self.logs`: everything passed through channel.log()
    pub logs: Mutex<Vec<(String, String)>>,
    /// python `self.input_history`: StorageList name=webui_input_history, type=json
    pub input_history: Mutex<StorageList>,
    pub url: String,
    bind_host: Option<String>,
    bind_port: Value,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    stopped: watch::Sender<bool>,
}

impl Shared {
    pub fn ch(&self) -> Option<Arc<Channel>> {
        self.ch.upgrade()
    }

    pub fn asset_dirs(&self) -> BTreeMap<String, PathBuf> {
        self.ch().map(|ch| ui_dirs(&ch, "assets")).unwrap_or_default()
    }

    /// python `setup_ui_extensions()` dir refresh (run after module/settings changes)
    pub fn setup_ui_extensions(&self) {
        if let Some(ch) = self.ch() {
            self.templates.set_dirs(ui_dirs(&ch, "templates"));
        }
    }
}

/// session data for one http request (starlette's `request.session`), written back by the session layer
#[derive(Clone)]
pub struct SessionData(pub Arc<Mutex<Map<String, Value>>>);

#[derive(Default)]
pub struct Webui {
    shared: Mutex<Option<Arc<Shared>>>,
}

impl Webui {
    fn shared(&self) -> Option<Arc<Shared>> {
        self.shared.lock().clone()
    }
}

/// python `Webui.on_ready`: paths, template env, network mode -> host, url, websocket manager, session
fn build_shared(ch: &Arc<Channel>) -> anyhow::Result<Arc<Shared>> {
    let webui_dir = PathBuf::from(get_path("channels/webui")?);
    let template_path = webui_dir.join("templates");
    let host: Arc<dyn ExtHost> = Arc::new(WebuiHost { ch: Arc::downgrade(ch) });
    let templates = TemplateStore::new(template_path, ui_dirs(ch, "templates"), host);

    let network_mode = ch.config.get_str("network_mode");
    let custom_host = ch.config.get_str("custom_host");
    let bind = bind_host(network_mode.as_deref(), custom_host.as_deref());

    let port = ch.config.get1("port").unwrap_or(Value::Null);
    let url = channel_url(bind.as_deref(), py_str(&port));

    let secret = ch.config.get1("session_secret").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| DEFAULT_SECRET.to_string());
    let lifetime_days = ch.config.get1("login_lifetime").and_then(|v| ol_core::pyjson::as_i64(&v)).unwrap_or(30);
    let signer = SessionSigner::new(&secret, lifetime_days * 86400);

    let (stopped, _) = watch::channel(false);
    Ok(Arc::new(Shared {
        ch: Arc::downgrade(ch),
        webui_dir,
        templates,
        ws: WsManager::new(),
        login: Mutex::new(LoginLimiter::default()),
        signer,
        logs: Mutex::new(Vec::new()),
        input_history: Mutex::new(StorageList::new("webui_input_history", StorageType::Json, None)?),
        url,
        bind_host: bind,
        bind_port: port,
        shutdown: Mutex::new(None),
        stopped,
    }))
}

#[async_trait]
impl ChannelBehavior for Webui {
    async fn on_ready(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let shared = build_shared(ch)?;
        *self.shared.lock() = Some(shared);
        Ok(())
    }

    /// python `Webui.run`: serve the app until on_shutdown asks for a graceful stop
    async fn run(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let app = self.shared().ok_or_else(|| anyhow!("webui is not ready"))?;
        ch.log("webui", &format!("Starting WebUI on {}", app.url));

        let port = match ol_core::pyjson::as_i64(&app.bind_port).and_then(|p| u16::try_from(p).ok()) {
            Some(p) => p,
            None => anyhow::bail!("port must be an integer between 0 and 65535"),
        };
        // python binds asyncio with host=None when custom mode has no custom_host: every interface
        let listener = match app.bind_host.as_deref().filter(|h| !h.is_empty()) {
            Some(host) => tokio::net::TcpListener::bind((host, port)).await?,
            None => match tokio::net::TcpListener::bind(("::", port)).await {
                Ok(l) => l,
                Err(_) => tokio::net::TcpListener::bind(("0.0.0.0", port)).await?,
            },
        };

        let (tx, rx) = oneshot::channel::<()>();
        *app.shutdown.lock() = Some(tx);
        let router = build_router(app.clone());
        let served = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
        app.stopped.send_replace(true);
        served.map_err(Into::into)
    }

    async fn on_push(&self, _ch: &Arc<Channel>, message: Value) -> anyhow::Result<()> {
        if let Some(app) = self.shared() {
            app.ws.broadcast(json!({"type": "push", "content": message}));
        }
        Ok(())
    }

    async fn on_stream(&self, _ch: &Arc<Channel>, stream: TokenStream) -> anyhow::Result<()> {
        if let Some(app) = self.shared() {
            ws::forward_stream(&app, stream).await;
        }
        Ok(())
    }

    fn on_log(&self, _ch: &Arc<Channel>, category: &str, message: &str) {
        let Some(app) = self.shared() else { return };
        app.logs.lock().push((category.to_string(), message.to_string()));
        app.ws.broadcast(json!({"type": "log", "category": category, "message": message}));
    }

    async fn on_shutdown(&self, _ch: &Arc<Channel>) -> anyhow::Result<()> {
        let Some(app) = self.shared() else { return Ok(()) };
        // broadcast first so clients know we're going away
        app.ws.broadcast(json!({"type": "shutdown"}));
        if let Some(tx) = app.shutdown.lock().take() {
            let _ = tx.send(());
        }
        // python waits up to 5s for uvicorn to finish, then gives the socket half a second
        let mut stopped = app.stopped.subscribe();
        if tokio::time::timeout(Duration::from_secs(5), stopped.wait_for(|v| *v)).await.is_err() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ------------------
// middleware (python `create_fastapi`)
// ------------------

/// python `coi_middleware`: cross-origin isolation headers on every http response
async fn coi_middleware(req: Request, next: Next) -> Response {
    if req.uri().path() == "/ws" {
        return next.run(req).await;
    }
    let mut response = next.run(req).await;
    response.headers_mut().insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    response.headers_mut().insert("cross-origin-embedder-policy", HeaderValue::from_static("credentialless"));
    response
}

/// python `auth_middleware`: admin lockdown, then the login gate
async fn auth_middleware(State(app): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if path == "/ws" {
        return next.run(req).await;
    }
    let Some(ch) = app.ch() else { return internal_error() };

    if !ch.config.get_bool("enable_admin_settings", true) && ADMIN_ONLY_ENDPOINTS.contains(&path.as_str()) {
        return (StatusCode::FORBIDDEN, Json(json!({"data": "Admin settings are disabled", "success": false}))).into_response();
    }

    if !ch.config.get_bool("require_login", false) {
        return next.run(req).await;
    }
    if path == "/login" || path == "/logout" || path.starts_with("/assets/") {
        return next.run(req).await;
    }

    let authenticated = req
        .extensions()
        .get::<SessionData>()
        .map(|s| s.0.lock().get("authenticated").map(truthy).unwrap_or(false))
        .unwrap_or(false);
    if !authenticated {
        if path.starts_with("/api") {
            return detail_response(StatusCode::UNAUTHORIZED, "Unauthorized");
        }
        return Redirect::to("/login").into_response();
    }
    next.run(req).await
}

/// starlette `SessionMiddleware`: the session is loaded from the cookie, exposed to the handlers, and
/// written back as a Set-Cookie (or cleared when it was non-empty and is now empty)
async fn session_middleware(State(app): State<Arc<Shared>>, mut req: Request, next: Next) -> Response {
    if req.uri().path() == "/ws" {
        return next.run(req).await;
    }
    let initial = session::cookie_value(req.headers(), COOKIE_NAME).and_then(|v| app.signer.unsign(&v)).unwrap_or_default();
    let initially_empty = initial.is_empty();
    let data = SessionData(Arc::new(Mutex::new(initial)));
    req.extensions_mut().insert(data.clone());

    let mut response = next.run(req).await;
    let session = data.0.lock().clone();
    if !session.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&app.signer.set_cookie(&session)) {
            response.headers_mut().append(header::SET_COOKIE, v);
        }
    } else if !initially_empty {
        if let Ok(v) = HeaderValue::from_str(&SessionSigner::clear_cookie()) {
            response.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    response
}

// ------------------
// pages
// ------------------

/// `TemplateResponse`: renders a template from the webui template store
fn render_page(app: &Shared, name: &str, ctx: Value) -> Response {
    match app.templates.render(name, minijinja::Value::from_serialize(&ctx)) {
        Ok(html) => Html(html).into_response(),
        Err(_) => internal_error(),
    }
}

/// python `os.listdir` (a missing folder is an error, which python turns into a 500)
fn list_dir_names(dir: &Path) -> Option<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
    names.sort();
    Some(names)
}

/// python `get_recursive_assets(assets_path, ext, skip)`: `.ext` files below `base`, relative to it, sorted
pub fn get_recursive_assets(base: &Path, ext: &str, skip: &[&str]) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, ext: &str, skip: &[&str], out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if skip.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                // os.walk does not descend into symlinked directories (followlinks=False)
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    walk(&path, base, ext, skip, out);
                }
            } else if name.ends_with(&format!(".{}", ext)) {
                if let Ok(rel) = path.strip_prefix(base) {
                    out.push(rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(base, base, ext, skip, &mut out);
    out.sort();
    out
}

async fn root(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let assets = app.webui_dir.join("assets");
    let (Some(alpine_stores), Some(js_utils)) = (list_dir_names(&assets.join("js").join("stores")), list_dir_names(&assets.join("js").join("utils"))) else {
        return internal_error();
    };
    let dirs = app.asset_dirs();
    let ctx = json!({
        "version": 2.0,
        "config": ch.config.to_dict(),
        "css_files": get_recursive_assets(&assets.join("css"), "css", &["code-themes"]),
        "alpine_stores": alpine_stores,
        "js_utils": js_utils,
        "js_files": get_recursive_assets(&assets.join("js"), "js", &["init.js", "stores", "libs", "utils"]),
        "ext_css_files": ui_asset_urls(&dirs, "css"),
        "ext_js_files": ui_asset_urls(&dirs, "js"),
        "login_enabled": ch.config.get1("require_login").unwrap_or(Value::Null),
        "core_config": Value::Object(ol_core::config::snapshot()),
    });
    render_page(&app, "index.html", ctx)
}

async fn login_page(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    if truthy(&ch.config.get1("require_login").unwrap_or(Value::Null)) {
        let css = ui_asset_urls(&app.asset_dirs(), "css");
        render_page(&app, "login.html", json!({"error": null, "ext_css_files": css}))
    } else {
        Redirect::to("/").into_response()
    }
}

/// uvicorn `proxy_headers=True, forwarded_allow_ips="127.0.0.1"`: a loopback peer's X-Forwarded-For
/// names the client (the rightmost entry that is not the trusted proxy itself)
fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    if peer.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST) {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            for part in xff.split(',').rev() {
                let p = part.trim();
                if !p.is_empty() && p != "127.0.0.1" {
                    return p.to_string();
                }
            }
        }
    }
    peer.ip().to_string()
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// `request.form.get(name)` for url-encoded bodies. starlette's FormData keeps the LAST value of a
/// repeated key (it is built as a dict), so this takes the last match too
fn form_field(body: &[u8], name: &str) -> Option<String> {
    url::form_urlencoded::parse(body).filter(|(k, _)| k == name).last().map(|(_, v)| v.into_owned())
}

async fn login_submit(
    State(app): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Extension(session): Extension<SessionData>,
    body: Bytes,
) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let ip = client_ip(&headers, peer);
    let now = now_secs();

    if app.login.lock().is_limited(&ip, now) {
        return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": session::LOGIN_RATE_LIMITED}))).into_response();
    }

    // python: a missing field is None, and compare_digest(None, ...) raises
    let (Some(username), Some(password)) = (form_field(&body, "username"), form_field(&body, "password")) else {
        return internal_error();
    };
    let verified = session::verify_credentials(
        &username,
        &password,
        ch.config.get_str("username").as_deref(),
        ch.config.get_str("password").as_deref(),
    );
    match verified {
        Err(_) => internal_error(),
        Ok(true) => {
            app.login.lock().record_success(&ip);
            session.0.lock().insert("authenticated".into(), Value::Bool(true));
            Redirect::to("/").into_response()
        }
        Ok(false) => {
            app.login.lock().record_failure(&ip, now);
            let css = ui_asset_urls(&app.asset_dirs(), "css");
            render_page(&app, "login.html", json!({"error": INVALID_CREDENTIALS, "ext_css_files": css}))
        }
    }
}

async fn logout(Extension(session): Extension<SessionData>) -> Response {
    session.0.lock().remove("authenticated");
    Redirect::to("/login").into_response()
}

async fn not_found() -> Response {
    detail_response(StatusCode::NOT_FOUND, "Not Found")
}

async fn method_not_allowed() -> Response {
    detail_response(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
}

/// the whole app: page routes, static/ui/api route groups, then the middleware stack
pub fn build_router(app: Arc<Shared>) -> Router {
    let router = Router::new()
        .route("/", get(root))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", get(logout))
        .route("/ws", get(ws::ws_route));
    let router = static_routes::add_routes(router, &app);
    let router = ui_routes::add_routes(router);
    let router = routes::add_api_routes(router);

    // layers wrap everything registered above, so they must come last. the last layer is the
    // outermost: session, then auth (its early answers skip the coi headers, as in python), then coi
    router
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn(coi_middleware))
        .layer(middleware::from_fn_with_state(app.clone(), auth_middleware))
        .layer(middleware::from_fn_with_state(app.clone(), session_middleware))
        .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::py_int;
    use serde_json::json;

    #[test]
    fn api_envelope_maps_none_to_empty_object() {
        assert_eq!(api_result(Value::Null, true), json!({"data": {}, "success": true}));
        assert_eq!(api_result(json!("Invalid credentials"), false), json!({"data": "Invalid credentials", "success": false}));
    }

    #[test]
    fn admin_lockdown_list_matches_python() {
        assert!(ADMIN_ONLY_ENDPOINTS.contains(&"/api/settings/save"));
        assert!(ADMIN_ONLY_ENDPOINTS.contains(&"/api/libraries/uninstall"));
        assert!(!ADMIN_ONLY_ENDPOINTS.contains(&"/api/system/data"));
        assert_eq!(ADMIN_ONLY_ENDPOINTS.len(), 15);
    }

    #[test]
    fn asset_listing_is_sorted_relative_and_skips_names() {
        let dir = tempfile::tempdir().unwrap();
        let css = dir.path().join("css");
        std::fs::create_dir_all(css.join("code-themes")).unwrap();
        std::fs::create_dir_all(css.join("sub")).unwrap();
        std::fs::write(css.join("b.css"), "").unwrap();
        std::fs::write(css.join("a.css"), "").unwrap();
        std::fs::write(css.join("sub").join("c.css"), "").unwrap();
        std::fs::write(css.join("code-themes").join("x.css"), "").unwrap();
        std::fs::write(css.join("skip.txt"), "").unwrap();
        assert_eq!(get_recursive_assets(&css, "css", &["code-themes"]), vec!["a.css", "b.css", "sub/c.css"]);
    }

    #[test]
    fn login_client_ip_honours_forwarded_for_only_from_loopback() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9, 127.0.0.1"));
        let loopback: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "198.51.100.2:5000".parse().unwrap();
        assert_eq!(client_ip(&h, loopback), "203.0.113.9");
        assert_eq!(client_ip(&h, remote), "198.51.100.2");
        assert_eq!(client_ip(&HeaderMap::new(), loopback), "127.0.0.1");
    }

    #[test]
    fn form_field_reads_urlencoded_body() {
        assert_eq!(form_field(b"username=admin&password=p%40ss+x", "password").as_deref(), Some("p@ss x"));
        assert_eq!(form_field(b"username=admin", "password"), None);
        // a repeated key: starlette's dict-built FormData keeps the last value
        assert_eq!(form_field(b"username=a&username=b", "username").as_deref(), Some("b"));
        assert_eq!(py_int(&json!(3)), Some(3));
    }
}
