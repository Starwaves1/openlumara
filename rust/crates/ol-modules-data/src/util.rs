//! small python-compat helpers shared by the modules of this crate

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use ol_core::pyjson::{py_str, truthy};
use serde_json::{Map, Value};

/// python `datetime.datetime.now()` (naive local time)
pub fn now() -> NaiveDateTime {
    chrono::Local::now().naive_local()
}

/// python `datetime.isoformat()` of a naive datetime
pub fn iso(dt: &NaiveDateTime) -> String {
    ol_core::globals::naive_isoformat(dt)
}

/// python `datetime.datetime.fromisoformat()`; the error is python's ValueError message
pub fn fromisoformat(s: &str) -> Result<NaiveDateTime, String> {
    if let Some(dt) = ol_core::globals::parse_isoformat(s) {
        return Ok(dt);
    }
    for fmt in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H", "%Y-%m-%d %H", "%Y%m%dT%H%M%S", "%Y%m%d"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(dt);
        }
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Ok(d.and_hms_opt(0, 0, 0).unwrap());
        }
    }
    Err(format!("Invalid isoformat string: {}", py_str_repr(s)))
}

/// fromisoformat() of a json value (non-strings raise TypeError in python)
pub fn fromisoformat_value(v: Option<&Value>) -> Result<NaiveDateTime, String> {
    match v {
        Some(Value::String(s)) => fromisoformat(s),
        Some(other) => Err(format!("fromisoformat: argument must be str, not {}", py_type(other))),
        None => Err("fromisoformat: argument must be str".into()),
    }
}

/// python repr() of a str
pub fn py_str_repr(s: &str) -> String {
    ol_core::pyjson::py_repr(&Value::String(s.to_string()))
}

/// python type name of a json value
pub fn py_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// python `datetime.datetime(year, month, day, hour, minute)` with python's ValueErrors
pub fn py_datetime(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> Result<NaiveDateTime, String> {
    if !(1..=9999).contains(&year) {
        return Err(format!("year {} is out of range", year));
    }
    if !(1..=12).contains(&month) {
        return Err("month must be in 1..12".into());
    }
    let Some(date) = NaiveDate::from_ymd_opt(year as i32, month as u32, day.clamp(0, 99) as u32).filter(|_| day >= 1) else {
        return Err("day is out of range for month".into());
    };
    if !(0..=23).contains(&hour) {
        return Err("hour must be in 0..23".into());
    }
    if !(0..=59).contains(&minute) {
        return Err("minute must be in 0..59".into());
    }
    Ok(date.and_hms_opt(hour as u32, minute as u32, 0).unwrap())
}

/// python `dt.replace(hour=, minute=, second=, microsecond=0)`
pub fn replace_time(dt: &NaiveDateTime, hour: i64, minute: i64, second: i64) -> Result<NaiveDateTime, String> {
    if !(0..=23).contains(&hour) {
        return Err("hour must be in 0..23".into());
    }
    if !(0..=59).contains(&minute) {
        return Err("minute must be in 0..59".into());
    }
    if !(0..=59).contains(&second) {
        return Err("second must be in 0..59".into());
    }
    Ok(dt.date().and_hms_opt(hour as u32, minute as u32, second as u32).unwrap())
}

/// python `dt.weekday()` (monday = 0)
pub fn weekday(dt: &NaiveDateTime) -> i64 {
    dt.weekday().num_days_from_monday() as i64
}

/// `(a - b).total_seconds()`
pub fn total_seconds(a: &NaiveDateTime, b: &NaiveDateTime) -> f64 {
    let d = *a - *b;
    d.num_microseconds().map(|us| us as f64 / 1e6).unwrap_or(d.num_seconds() as f64)
}

/// midnight of the given datetime (`dt.replace(hour=0, minute=0, second=0, microsecond=0)`)
pub fn midnight(dt: &NaiveDateTime) -> NaiveDateTime {
    dt.date().and_hms_opt(0, 0, 0).unwrap()
}

/// python `int(x)` for json values (strings are parsed strictly, floats truncated)
pub fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::Bool(b) => Some(*b as i64),
        Value::String(s) => {
            let t = s.trim().replace('_', "");
            if t.is_empty() {
                return None;
            }
            t.parse::<i64>().ok()
        }
        _ => None,
    }
}

/// python `int(s)` for str
pub fn py_int_str(s: &str) -> Option<i64> {
    py_int(&Value::String(s.to_string()))
}

/// `int(query.get(key, default) or default)` with ValueError -> default
pub fn query_int(query: &Map<String, Value>, key: &str, default: i64) -> i64 {
    match query.get(key) {
        Some(v) if truthy(v) => py_int(v).unwrap_or(default),
        _ => default,
    }
}

/// `str(query.get(key, default))`
pub fn query_str(query: &Map<String, Value>, key: &str, default: &str) -> String {
    query.get(key).map(py_str).unwrap_or_else(|| default.to_string())
}

/// `(body or {}).get(key)` -> owned value (None when missing)
pub fn body_get(body: &Option<Value>, key: &str) -> Option<Value> {
    body.as_ref().and_then(|b| b.as_object()).and_then(|b| b.get(key)).cloned()
}

/// `str(body.get(key, default))`
pub fn body_str(body: &Option<Value>, key: &str, default: &str) -> String {
    body_get(body, key).map(|v| py_str(&v)).unwrap_or_else(|| default.to_string())
}

/// python truthiness of an optional value
pub fn truthy_opt(v: Option<&Value>) -> bool {
    v.map(truthy).unwrap_or(false)
}

/// python `round(x, 2)` (correctly rounded, like python's float rounding)
pub fn round2(x: f64) -> f64 {
    format!("{:.2}", x).parse().unwrap_or(x)
}

/// python `os.path.expanduser`
pub fn expanduser(p: &str) -> String {
    if p == "~" || p.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home, &p[1..]);
        }
    }
    p.to_string()
}

/// lenient base64 decode like python's `base64.b64decode` (non-alphabet characters are discarded)
pub fn b64decode(s: &[u8]) -> Result<Vec<u8>, String> {
    use base64::engine::{general_purpose::GeneralPurpose, DecodePaddingMode, GeneralPurposeConfig};
    use base64::Engine;
    let filtered: Vec<u8> = s.iter().copied().filter(|c| c.is_ascii_alphanumeric() || *c == b'+' || *c == b'/' || *c == b'=').collect();
    // python stops at the first padding group; strip everything after the padding
    let cut = match filtered.iter().position(|c| *c == b'=') {
        Some(p) => {
            let mut end = p;
            while end < filtered.len() && filtered[end] == b'=' {
                end += 1;
            }
            &filtered[..end]
        }
        None => &filtered[..],
    };
    if cut.iter().filter(|c| **c != b'=').count() % 4 == 1 {
        return Err("Invalid base64-encoded string: number of data characters (5) cannot be 1 more than a multiple of 4".into());
    }
    let engine = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true).with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    engine.decode(cut).map_err(|_| "Incorrect padding".to_string())
}

/// python `str.lower()` + `str.strip()` equality used by several lookups
pub fn eq_ci_strip(a: &str, b: &str) -> bool {
    a.to_lowercase().trim() == b.to_lowercase().trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn isoformat_roundtrip() {
        let dt = fromisoformat("2024-01-15T10:30:00").unwrap();
        assert_eq!(iso(&dt), "2024-01-15T10:30:00");
        assert_eq!(iso(&fromisoformat("2024-01-15").unwrap()), "2024-01-15T00:00:00");
        assert_eq!(iso(&fromisoformat("2024-01-15 10:30").unwrap()), "2024-01-15T10:30:00");
        assert_eq!(iso(&fromisoformat("2024-01-15T10:30:00.123456").unwrap()), "2024-01-15T10:30:00.123456");
        assert_eq!(fromisoformat("nope").unwrap_err(), "Invalid isoformat string: 'nope'");
    }

    #[test]
    fn datetime_errors() {
        assert_eq!(py_datetime(2024, 13, 1, 0, 0).unwrap_err(), "month must be in 1..12");
        assert_eq!(py_datetime(2024, 2, 30, 0, 0).unwrap_err(), "day is out of range for month");
        assert_eq!(py_datetime(2024, 2, 29, 24, 0).unwrap_err(), "hour must be in 0..23");
        assert!(py_datetime(2024, 2, 29, 23, 59).is_ok());
    }

    #[test]
    fn ints_and_rounding() {
        assert_eq!(py_int(&json!(" 14 ")), Some(14));
        assert_eq!(py_int(&json!("1.5")), None);
        assert_eq!(py_int(&json!(2.9)), Some(2));
        let q = json!({"limit": "", "x": "abc", "y": "7"}).as_object().unwrap().clone();
        assert_eq!(query_int(&q, "limit", 100), 100);
        assert_eq!(query_int(&q, "x", 100), 100);
        assert_eq!(query_int(&q, "y", 100), 7);
        assert_eq!(round2(0.125), 0.12);
        assert_eq!(round2(1.0 / 3.0), 0.33);
    }

    #[test]
    fn base64_lenient() {
        assert_eq!(b64decode(b"aGVs\nbG8=").unwrap(), b"hello");
        assert_eq!(b64decode(b"aGVsbG8").unwrap(), b"hello");
    }
}
