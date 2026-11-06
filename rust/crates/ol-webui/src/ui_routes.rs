//! Port of the module extension API (`/api/ext/<module>/<path>`, python `ext_api` and `_ui_api_lookup`):
//! a request is forwarded to the module's `ui_routes()` entry with the same method, and the result is
//! wrapped in the api envelope.

use crate::server::{api_result, internal_error, method_not_allowed_allowing, module_ui_enabled, slash_redirect, Shared};
use crate::util::py_strip_lower;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::handler::Handler;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, MethodRouter};
use axum::Json;
use axum::Router;
use ol_core::channel::Channel;
use ol_core::module::Module;
use ol_core::detail_error;
use serde_json::{Map, Value};
use std::sync::Arc;

pub fn add_routes(router: Router<Arc<Shared>>) -> Router<Arc<Shared>> {
    // starlette's `{route_path:path}` also matches an empty remainder, so `/api/ext/<module>/` reaches
    // the handler with route_path "". matchit's `{*catchall}` does not match an empty segment, hence the
    // separate route
    router
        .route("/api/ext/{module_name}", any(ext_module_slash))
        .route("/api/ext/{module_name}/", ext_methods(ext_api_root))
        .route("/api/ext/{module_name}/{*route_path}", ext_methods(ext_api))
}

/// python registers `ext_api` for GET/POST/PUT/DELETE only, and FastAPI adds no HEAD to GET, so every other
/// method (HEAD included) is starlette's 405 with the python `Allow` list
fn ext_methods<H, T>(handler: H) -> MethodRouter<Arc<Shared>>
where
    H: Handler<T, Arc<Shared>> + Clone,
    T: 'static,
{
    get(handler.clone())
        .post(handler.clone())
        .put(handler.clone())
        .delete(handler)
        .head(ext_not_allowed)
        .fallback(ext_not_allowed)
}

async fn ext_not_allowed() -> Response {
    method_not_allowed_allowing("GET, POST, PUT, DELETE")
}

/// `/api/ext/<module>` without the trailing slash: starlette's redirect_slashes to the slashed form
async fn ext_module_slash(headers: HeaderMap, Path(module_name): Path<String>, RawQuery(query): RawQuery) -> Response {
    slash_redirect(&headers, &format!("/api/ext/{module_name}/"), query.as_deref())
}

/// python `_ui_api_lookup`: `(module, None)` on a hit, `(None, reason)` on a miss
fn find_route(ch: &Channel, module_name: &str, wanted: &str, method: &str) -> Result<Arc<dyn Module>, String> {
    let Some(module) = ch.manager().module(module_name) else {
        return Err("unknown module".into());
    };
    if !module_ui_enabled(&module) {
        return Err("webui extension disabled".into());
    }
    let mut route_exists = false;
    for route in module.ui_routes() {
        if route.path != wanted {
            continue;
        }
        route_exists = true;
        if route.method == method {
            return Ok(module);
        }
    }
    if !route_exists {
        return Err("route not found".into());
    }
    Err(format!("route only accepts {}", if method != "GET" { "GET" } else { "POST" }))
}

/// `dict(request.query_params)`: the last value of a repeated key wins
fn query_map(raw: Option<&str>) -> Map<String, Value> {
    let mut map = Map::new();
    if let Some(raw) = raw {
        for (k, v) in url::form_urlencoded::parse(raw.as_bytes()) {
            map.insert(k.into_owned(), Value::String(v.into_owned()));
        }
    }
    map
}

async fn ext_api(
    State(app): State<Arc<Shared>>,
    Path((module_name, route_path)): Path<(String, String)>,
    method: Method,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response {
    dispatch(app, module_name, route_path, method, raw_query, body).await
}

/// `/api/ext/<module>/` (empty route_path, as in python)
async fn ext_api_root(
    State(app): State<Arc<Shared>>,
    Path(module_name): Path<String>,
    method: Method,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Response {
    dispatch(app, module_name, String::new(), method, raw_query, body).await
}

async fn dispatch(
    app: Arc<Shared>,
    module_name: String,
    route_path: String,
    method: Method,
    raw_query: Option<String>,
    body: Bytes,
) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let wanted = py_strip_lower(&route_path);
    let method_name = method.as_str();

    let module = match find_route(&ch, &module_name, &wanted, method_name) {
        Ok(m) => m,
        Err(reason) => {
            let status = if reason.contains("accepts") { StatusCode::METHOD_NOT_ALLOWED } else { StatusCode::NOT_FOUND };
            return (status, Json(api_result(Value::String(reason), false))).into_response();
        }
    };

    // ext routes bypass send()/send_stream(), the only other caller of set_as_active_channel()
    ch.set_as_active_channel().await;

    // python: request.json() for every method but GET; a failed parse or a JSON null is None.
    // json.loads on bytes accepts a UTF-8 BOM (detect_encoding picks utf-8-sig), serde_json does not
    let json_body = if method == Method::GET {
        None
    } else {
        let bytes = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&body);
        serde_json::from_slice::<Value>(bytes).ok().filter(|v| !v.is_null())
    };

    match module.ui_route(&wanted, method_name, json_body, query_map(raw_query.as_deref())).await {
        Ok(outcome) => {
            // a module's self.result() dict (status + content) is unwrapped into the envelope
            let envelope = match outcome.as_object() {
                Some(o) if o.contains_key("status") && o.contains_key("content") => {
                    let success = o.get("status").and_then(Value::as_str) == Some("success");
                    api_result(o.get("content").cloned().unwrap_or(Value::Null), success)
                }
                _ => api_result(outcome, true),
            };
            Json(envelope).into_response()
        }
        Err(e) => {
            let detail = detail_error(&e);
            ch.log("webui", &format!("ext api error {}/{}: {}", module_name, route_path, detail));
            Json(api_result(Value::String(detail), false)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_keeps_last_value_of_repeated_keys() {
        let q = query_map(Some("a=1&b=x+y&a=2"));
        assert_eq!(q.get("a"), Some(&Value::String("2".into())));
        assert_eq!(q.get("b"), Some(&Value::String("x y".into())));
        assert_eq!(q.keys().collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(query_map(None).is_empty());
    }

    #[test]
    fn route_paths_are_normalised_like_python() {
        assert_eq!(py_strip_lower("/Settings/Get/"), "settings/get");
        assert_eq!(py_strip_lower("plain"), "plain");
    }
}
