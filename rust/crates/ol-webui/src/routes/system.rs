//! Port of the `/api/system/*` routes in channels/webui/channel.py (`get_data`, `get_context_size`,
//! `get_logs`, `restart_server`). Each answer goes through `api_result`, the same envelope as the other
//! api routes; the GET routes are `@app.get` in python, so they answer HEAD and other methods with 405.

use crate::server::{api_result, get_only, internal_error, Shared};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Json;
use axum::Router;
use ol_core::pyjson::{py_repr, truthy};
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

/// python `round(x)` with no ndigits: nearest integer, exact ties go to the even integer (12.5 gives 12)
pub fn py_round0(x: f64) -> i64 {
    if (x - x.trunc()).abs() == 0.5 {
        let lo = x.floor() as i64;
        return if lo % 2 == 0 { lo } else { lo + 1 };
    }
    x.round() as i64
}

/// python `int(value)` for a config value. the error is python's `str(e)` for the TypeError or ValueError
fn py_int(v: &Value) -> Result<i64, String> {
    match v {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Ok(i),
            None => py_int_from_float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => py_int_from_str(s).ok_or_else(|| format!("invalid literal for int() with base 10: {}", py_repr(v))),
        Value::Null => Err("int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".into()),
        Value::Array(_) => Err("int() argument must be a string, a bytes-like object or a real number, not 'list'".into()),
        Value::Object(_) => Err("int() argument must be a string, a bytes-like object or a real number, not 'dict'".into()),
    }
}

/// python `int(float)`: truncates toward zero, and nan / infinity raise
fn py_int_from_float(f: f64) -> Result<i64, String> {
    if f.is_nan() {
        return Err("cannot convert float NaN to integer".into());
    }
    if f.is_infinite() {
        return Err("cannot convert float infinity to integer".into());
    }
    Ok(f.trunc() as i64)
}

/// python `int(str)`: surrounding whitespace, an optional sign, and digits with single underscores between them
fn py_int_from_str(s: &str) -> Option<i64> {
    let t = s.trim();
    let (negative, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let parts: Vec<&str> = body.split('_').collect();
    if parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let n: i64 = parts.concat().parse().ok()?;
    if negative {
        n.checked_neg()
    } else {
        Some(n)
    }
}

/// python `float(value)` for a truthy config value; None where python raises
fn py_float(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// `api_result(data)` as a json response
fn reply(data: Value) -> Response {
    Json(api_result(data, true)).into_response()
}

/// `api_result(message, success=False)` as a json response
fn failure(message: String) -> Response {
    Json(api_result(json!(message), false)).into_response()
}

/// python `get_data`: `{"max_context": core.config.get("api", "max_context")}`
async fn system_data(State(app): State<Arc<Shared>>) -> Response {
    let Some(_ch) = app.ch() else { return internal_error() };
    let max_context = ol_core::config::get(&["api", "max_context"]).unwrap_or(Value::Null);
    reply(json!({"max_context": max_context}))
}

/// python `get_context_size`: the context breakdown plus `compression_threshold` (percent, one decimal)
/// and `auto_compress`, appended after the breakdown keys in that order.
/// python's `get_size()` raises on a bad `api.max_context` (int() of it, then division by it), which
/// python answers as `{"data": <error text>, "success": false}`; the ol-core breakdown would silently
/// default instead, so the value is parsed here first and the percentage is computed with python's rounding.
async fn system_context_size(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let max_raw = ol_core::config::get(&["api", "max_context"]).unwrap_or(Value::Null);
    let max_context = match py_int(&max_raw) {
        Ok(0) => return failure("division by zero".into()),
        Ok(n) => n,
        Err(message) => return failure(message),
    };
    let mut size = ch.context.get_size().await;
    let total = size["total_tokens"].as_i64().unwrap_or(0);
    size["max_context"] = json!(max_context);
    size["percent_full"] = json!(py_round0((total as f64 / max_context as f64) * 100.0));

    // python: `threshold = config.get(...) or 0`, then `round(float(threshold) * 100, 1)` outside any try,
    // so a non-numeric stored value is an unhandled error (500)
    let raw_threshold = ol_core::config::get(&["model", "context_compression_threshold"]).unwrap_or(Value::Null);
    let threshold = if truthy(&raw_threshold) {
        match py_float(&raw_threshold) {
            Some(t) => t,
            None => return internal_error(),
        }
    } else {
        0.0
    };
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

/// python `restart_server`: `await channel.manager.restart()`, which logs, sets the restart flag and then
/// awaits `shutdown()` before the handler returns None, so FastAPI answers `null` only after the shutdown
async fn system_restart(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let manager = ch.manager();
    manager.restart();
    manager.shutdown().await;
    Json(Value::Null).into_response()
}

#[cfg(test)]
mod tests {
    use super::{py_float, py_int, py_round0, py_round1};
    use serde_json::{json, Value};

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

    #[test]
    fn py_round0_sends_exact_ties_to_the_even_integer() {
        // python: round(12.5) == 12, round(13.5) == 14, round(-12.5) == -12, round(12.4) == 12, round(12.6) == 13
        assert_eq!(py_round0(12.5), 12);
        assert_eq!(py_round0(13.5), 14);
        assert_eq!(py_round0(-12.5), -12);
        assert_eq!(py_round0(12.4), 12);
        assert_eq!(py_round0(12.6), 13);
        assert_eq!(py_round0(0.0), 0);
        assert_eq!(py_round0(100.0), 100);
    }

    #[test]
    fn py_int_accepts_what_python_int_accepts_and_reports_its_errors() {
        assert_eq!(py_int(&json!("16768")), Ok(16768));
        assert_eq!(py_int(&json!(" 1_000 ")), Ok(1000));
        assert_eq!(py_int(&json!(16768.9)), Ok(16768));
        assert_eq!(py_int(&json!(true)), Ok(1));
        assert_eq!(py_int(&json!("1__0")), Err("invalid literal for int() with base 10: '1__0'".to_string()));
        assert_eq!(py_int(&Value::Null), Err("int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".to_string()));
    }

    #[test]
    fn py_float_follows_python_float_for_truthy_values() {
        assert_eq!(py_float(&json!("0.25")), Some(0.25));
        assert_eq!(py_float(&json!(true)), Some(1.0));
        assert_eq!(py_float(&json!("abc")), None);
        assert_eq!(py_float(&json!([1])), None);
    }
}
