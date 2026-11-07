//! Port of the `/api/system/*` routes in channels/webui/channel.py (`get_data`, `get_context_size`,
//! `get_logs`, `restart_server`). Each answer goes through `api_result`, the same envelope as the other
//! api routes; the GET routes are `@app.get` in python, so they answer HEAD and other methods with 405.

use crate::server::{api_result, get_only, internal_error, Shared};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Json;
use axum::Router;
use ol_core::pyjson::truthy;
use serde_json::{json, Value};
use std::sync::Arc;

/// registers `/api/system/data`, `/api/system/context_size`, `/api/system/logs` and `/api/system/restart`
pub fn add_routes(router: Router<Arc<Shared>>) -> Router<Arc<Shared>> {
    router
        .route("/api/system/data", get_only(system_data))
        .route("/api/system/context_size", get_only(system_context_size))
        .route("/api/system/logs", get_only(system_logs))
        .route("/api/system/restart", post(system_restart))
}

/// python `round(x, 1)`: rounds the exact binary value half-to-even, like python does, so 0.35 gives 0.3
pub fn py_round1(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.1}").parse().unwrap_or(x)
}

/// `api_result(data)` as a json response
fn reply(data: Value) -> Response {
    Json(api_result(data, true)).into_response()
}

/// python `get_data`: `{"max_context": core.config.get("api", "max_context")}`
async fn system_data(State(app): State<Arc<Shared>>) -> Response {
    let Some(_ch) = app.ch() else { return internal_error() };
    let max_context = ol_core::config::get(&["api", "max_context"]).unwrap_or(Value::Null);
    reply(json!({"max_context": max_context}))
}

/// python `get_context_size`: the context breakdown plus `compression_threshold` (percent, one decimal)
/// and `auto_compress`, appended after the breakdown keys in that order
async fn system_context_size(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let mut size = ch.context.get_size().await;
    let threshold = ol_core::config::get_f64(&["model", "context_compression_threshold"], 0.0);
    size["compression_threshold"] = json!(py_round1(threshold * 100.0));
    let auto = ol_core::config::get(&["model", "automatically_compress_context"]).unwrap_or(Value::Null);
    size["auto_compress"] = json!(truthy(&auto));
    reply(size)
}

/// python `get_logs`: `api_result(channel.logs)`, each entry `{"category", "message"}`
async fn system_logs(State(app): State<Arc<Shared>>) -> Response {
    let logs: Vec<Value> = app.logs.lock().iter().map(|(category, message)| json!({"category": category, "message": message})).collect();
    reply(json!(logs))
}

/// python `restart_server`: awaits `manager.restart()` and returns None, so FastAPI answers `null`
async fn system_restart(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    ch.manager().restart();
    Json(Value::Null).into_response()
}

#[cfg(test)]
mod tests {
    use super::py_round1;

    #[test]
    fn py_round1_rounds_the_exact_binary_value_like_python() {
        // python: round(0.35, 1) == 0.3 (0.35 is stored just below the tie), round(0.25, 1) == 0.2 (exact tie to even)
        assert_eq!(py_round1(0.35), 0.3);
        assert_eq!(py_round1(0.25), 0.2);
        assert_eq!(py_round1(12.345), 12.3);
        assert_eq!(py_round1(0.05), 0.1);
        assert_eq!(py_round1(25.0), 25.0);
    }

    #[test]
    fn py_round1_keeps_non_finite_values() {
        assert!(py_round1(f64::NAN).is_nan());
        assert_eq!(py_round1(f64::INFINITY), f64::INFINITY);
    }
}
