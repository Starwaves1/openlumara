//! Port of the `/api/system/*` routes in channels/webui/channel.py (`get_data`, `get_context_size`,
//! `get_logs`, `restart_server`). Each answer goes through `api_result`, the same envelope as the other
//! api routes; the GET routes are `@app.get` in python, so they answer HEAD and other methods with 405.

use crate::server::{api_result, get_only, internal_error, Shared};
use crate::util::ascii_decimal_digits;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Json;
use axum::Router;
use ol_core::pyjson::{py_repr, truthy};
use serde::ser::SerializeMap;
use serde::{ser, Serialize, Serializer};
use serde_json::value::RawValue;
use serde_json::{json, Map, Value};
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

/// a python int. python ints are unbounded, so it is kept as its exact decimal text (`str()` of the int)
#[derive(Debug, PartialEq)]
struct PyInt(String);

impl PyInt {
    /// a sign and the digits without leading zeros (python: `int("-0012")` is `-12`, `int("-0")` is `0`)
    fn from_digits(negative: bool, digits: &str) -> PyInt {
        let digits = digits.trim_start_matches('0');
        if digits.is_empty() {
            PyInt("0".into())
        } else if negative {
            PyInt(format!("-{digits}"))
        } else {
            PyInt(digits.to_string())
        }
    }

    fn is_zero(&self) -> bool {
        self.0 == "0"
    }

    /// the nearest float, which is what the percentage needs (python's int / int is correctly rounded too)
    fn as_f64(&self) -> f64 {
        self.0.parse().unwrap_or(f64::NAN)
    }
}

/// python `int(value)` for a config value. the error is python's `str(e)` for the TypeError or ValueError
fn py_int(v: &Value) -> Result<PyInt, String> {
    match v {
        Value::Bool(b) => Ok(PyInt::from_digits(false, if *b { "1" } else { "0" })),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => Ok(PyInt::from_digits(i < 0, &i.unsigned_abs().to_string())),
            (None, Some(u)) => Ok(PyInt::from_digits(false, &u.to_string())),
            (None, None) => py_int_from_float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => py_int_from_str(s).ok_or_else(|| format!("invalid literal for int() with base 10: {}", py_repr(v))),
        Value::Null => Err("int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".into()),
        Value::Array(_) => Err("int() argument must be a string, a bytes-like object or a real number, not 'list'".into()),
        Value::Object(_) => Err("int() argument must be a string, a bytes-like object or a real number, not 'dict'".into()),
    }
}

/// python `int(float)`: truncates toward zero, and nan / infinity raise
fn py_int_from_float(f: f64) -> Result<PyInt, String> {
    if f.is_nan() {
        return Err("cannot convert float NaN to integer".into());
    }
    if f.is_infinite() {
        return Err("cannot convert float infinity to integer".into());
    }
    let t = f.trunc();
    // `{:.0}` prints the float's exact value, so 1e30 gives every digit python's int(1e30) has
    Ok(PyInt::from_digits(t < 0.0, &format!("{:.0}", t.abs())))
}

/// python `int(str)`: surrounding whitespace, an optional sign, and digits with single underscores between them
fn py_int_from_str(s: &str) -> Option<PyInt> {
    let digits = ascii_decimal_digits(s.trim());
    let t = digits.as_str();
    let (negative, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let parts: Vec<&str> = body.split('_').collect();
    if parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    Some(PyInt::from_digits(negative, &parts.concat()))
}

/// the context breakdown as python's `api_result(size)` writes it: `max_context` first, as python's exact int
/// text (which can be wider than i64), then the other keys in the order they were added
struct ContextBody {
    max_context: PyInt,
    breakdown: Map<String, Value>,
}

impl Serialize for ContextBody {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let max_context = RawValue::from_string(self.max_context.0.clone()).map_err(<S::Error as ser::Error>::custom)?;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("max_context", &*max_context)?;
        for (key, value) in self.breakdown.iter().filter(|(key, _)| key.as_str() != "max_context") {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

/// `api_result(size)` for the context route
#[derive(Serialize)]
struct ContextEnvelope {
    data: ContextBody,
    success: bool,
}

/// python `float(value)` for a truthy config value; None where python raises
fn py_float(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => py_float_str(s),
        _ => None,
    }
}

/// python `float(str)`: surrounding whitespace, and single underscores only between two digits (so '1_0' is 10.0)
fn py_float_str(s: &str) -> Option<f64> {
    let digits = ascii_decimal_digits(s.trim());
    let t = digits.as_str();
    let bytes = t.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'_' {
            let digit_before = i > 0 && bytes[i - 1].is_ascii_digit();
            let digit_after = bytes.get(i + 1).is_some_and(|c| c.is_ascii_digit());
            if !(digit_before && digit_after) {
                return None;
            }
        }
    }
    t.replace('_', "").parse::<f64>().ok()
}

/// `round(float(threshold) * 100, 1)` for the stored `model.context_compression_threshold`, or None where
/// python would fail to answer: a value python's `float()` rejects, or a non-finite result. python's
/// `round()` keeps inf/nan and starlette's JSONResponse (allow_nan=False) then raises, so the answer is a 500
fn compression_threshold_percent(raw: &Value) -> Option<f64> {
    // python: `threshold = config.get(...) or 0`
    let threshold = if truthy(raw) { py_float(raw)? } else { 0.0 };
    let pct = py_round1(threshold * 100.0);
    pct.is_finite().then_some(pct)
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
        Ok(n) if n.is_zero() => return failure("division by zero".into()),
        Ok(n) => n,
        Err(message) => return failure(message),
    };
    let mut size = ch.context.get_size().await;
    let total = size["total_tokens"].as_i64().unwrap_or(0);
    size["percent_full"] = json!(py_round0((total as f64 / max_context.as_f64()) * 100.0));

    // python: `round(float(threshold) * 100, 1)` outside any try, so a non-numeric or non-finite stored value is an unhandled error (500)
    let raw_threshold = ol_core::config::get(&["model", "context_compression_threshold"]).unwrap_or(Value::Null);
    let Some(percent) = compression_threshold_percent(&raw_threshold) else { return internal_error() };
    size["compression_threshold"] = json!(percent);
    let auto = ol_core::config::get(&["model", "automatically_compress_context"]).unwrap_or(Value::Null);
    size["auto_compress"] = json!(truthy(&auto));
    let Value::Object(breakdown) = size else { return internal_error() };
    Json(ContextEnvelope { data: ContextBody { max_context, breakdown }, success: true }).into_response()
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
    use super::{compression_threshold_percent, py_float, py_int, py_round0, py_round1, ContextBody, ContextEnvelope, PyInt};
    use serde_json::{json, Map, Value};

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
        assert_eq!(py_int(&json!("16768")), Ok(PyInt("16768".into())));
        assert_eq!(py_int(&json!(" 1_000 ")), Ok(PyInt("1000".into())));
        assert_eq!(py_int(&json!(16768.9)), Ok(PyInt("16768".into())));
        assert_eq!(py_int(&json!(true)), Ok(PyInt("1".into())));
        assert_eq!(py_int(&json!("1__0")), Err("invalid literal for int() with base 10: '1__0'".to_string()));
        assert_eq!(py_int(&Value::Null), Err("int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".to_string()));
    }

    #[test]
    fn py_int_keeps_every_digit_python_would_keep() {
        // python: int("99999999999999999999999") is exact, and int("-0012") is -12
        assert_eq!(py_int(&json!("99999999999999999999999")), Ok(PyInt("99999999999999999999999".into())));
        assert_eq!(py_int(&json!("-0012")), Ok(PyInt("-12".into())));
        assert_eq!(py_int(&json!("-0")), Ok(PyInt("0".into())));
        assert!(py_int(&json!("-0")).unwrap().is_zero());
        // python: int(1e30) is the float's exact value, 1000000000000000019884624838656
        assert_eq!(py_int(&json!(1e30)), Ok(PyInt("1000000000000000019884624838656".into())));
        // a JSON integer beyond i64 but within u64 is kept exactly as well
        assert_eq!(py_int(&json!(18446744073709551615u64)), Ok(PyInt("18446744073709551615".into())));
        assert_eq!(py_int(&json!(-9223372036854775808i64)), Ok(PyInt("-9223372036854775808".into())));
    }

    #[test]
    fn context_body_writes_python_exact_max_context_first() {
        // python: {"data": {"max_context": <int>, "total_tokens": 5, ...}, "success": true}
        let mut breakdown = Map::new();
        breakdown.insert("max_context".into(), json!(16768));
        breakdown.insert("total_tokens".into(), json!(5));
        breakdown.insert("percent_full".into(), json!(0));
        let envelope = ContextEnvelope {
            data: ContextBody { max_context: PyInt("99999999999999999999999".into()), breakdown },
            success: true,
        };
        assert_eq!(
            serde_json::to_string(&envelope).unwrap(),
            r#"{"data":{"max_context":99999999999999999999999,"total_tokens":5,"percent_full":0},"success":true}"#
        );
    }

    #[test]
    fn py_float_follows_python_float_for_truthy_values() {
        assert_eq!(py_float(&json!("0.25")), Some(0.25));
        assert_eq!(py_float(&json!(true)), Some(1.0));
        assert_eq!(py_float(&json!("abc")), None);
        assert_eq!(py_float(&json!([1])), None);
    }

    #[test]
    fn py_float_takes_underscores_only_between_digits() {
        // python: float('1_0') == 10.0, float('1__0'), float('_1') and float('1_') raise
        assert_eq!(py_float(&json!("1_0")), Some(10.0));
        assert_eq!(py_float(&json!(" 0.5 ")), Some(0.5));
        assert_eq!(py_float(&json!("1__0")), None);
        assert_eq!(py_float(&json!("_1")), None);
        assert_eq!(py_float(&json!("1_")), None);
    }

    #[test]
    fn py_int_and_py_float_take_unicode_decimal_digits_like_python() {
        // python: int('\u{661}\u{662}') == 12 (arabic-indic digits), int('\u{663}_\u{664}') == 34, float('\u{661}_\u{662}') == 12.0
        assert_eq!(py_int(&json!("\u{661}\u{662}")), Ok(PyInt("12".into())));
        assert_eq!(py_int(&json!("\u{663}_\u{664}")), Ok(PyInt("34".into())));
        assert_eq!(py_float(&json!("\u{661}\u{662}")), Some(12.0));
        assert_eq!(py_float(&json!("\u{661}_\u{662}")), Some(12.0));
        // a unicode digit does not make underscores legal where python rejects them
        assert_eq!(py_float(&json!("\u{661}__\u{662}")), None);
        assert!(py_int(&json!("\u{661}x")).is_err());
    }

    #[test]
    fn compression_threshold_percent_matches_python_round_and_refuses_non_finite() {
        // python: round(float(0.25) * 100, 1) == 25.0; a falsy stored value is 0
        assert_eq!(compression_threshold_percent(&json!(0.25)), Some(25.0));
        assert_eq!(compression_threshold_percent(&json!("0.125")), Some(12.5));
        assert_eq!(compression_threshold_percent(&json!(null)), Some(0.0));
        assert_eq!(compression_threshold_percent(&json!("")), Some(0.0));
        // python: float('nan') and float('inf') give non-finite results that starlette cannot serialise (500)
        assert_eq!(compression_threshold_percent(&json!("nan")), None);
        assert_eq!(compression_threshold_percent(&json!("inf")), None);
        assert_eq!(compression_threshold_percent(&json!("1e308")), None);
        assert_eq!(compression_threshold_percent(&json!("abc")), None);
    }
}
