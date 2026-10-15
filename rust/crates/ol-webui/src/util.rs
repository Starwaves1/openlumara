//! small python-semantics helpers used by the webui port

use ol_core::pyjson::truthy;
use serde_json::Value;

/// python `type(x).__name__` for json values (used in python-style error strings)
pub fn py_type_name(v: &Value) -> &'static str {
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

/// python `(value or default).strip()`: falsy values become `default`, truthy
/// non-strings raise AttributeError (returned as the python error message)
pub fn or_strip(v: Option<&Value>, default: &str) -> Result<String, String> {
    match v {
        Some(v) if truthy(v) => match v {
            Value::String(s) => Ok(s.trim().to_string()),
            other => Err(format!("'{}' object has no attribute 'strip'", py_type_name(other))),
        },
        _ => Ok(default.trim().to_string()),
    }
}

/// python `value.strip()` for `data.get(key, default)` (no `or` fallback)
pub fn get_strip(v: Option<&Value>, default: &str) -> Result<String, String> {
    match v {
        None => Ok(default.trim().to_string()),
        Some(Value::String(s)) => Ok(s.trim().to_string()),
        Some(other) => Err(format!("'{}' object has no attribute 'strip'", py_type_name(other))),
    }
}

/// python `int(str)` (surrounding whitespace allowed, optional sign, underscores between digits)
pub fn py_int_str(s: &str) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() || t.starts_with('_') || t.ends_with('_') || t.contains("__") {
        return None;
    }
    let (sign, digits) = match t.as_bytes()[0] {
        b'+' => (1, &t[1..]),
        b'-' => (-1, &t[1..]),
        _ => (1, t),
    };
    if digits.is_empty() || digits.starts_with('_') {
        return None;
    }
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    if !cleaned.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    cleaned.parse::<i64>().ok().map(|n| n * sign)
}

/// python `int(value)` for json values (floats truncate, bools are 0/1, strings are parsed)
pub fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::String(s) => py_int_str(s),
        _ => None,
    }
}

/// python list slicing `lst[start:stop]` (negative indices count from the end)
pub fn py_slice<T: Clone>(items: &[T], start: i64, stop: i64) -> Vec<T> {
    let len = items.len() as i64;
    let norm = |i: i64| -> i64 {
        if i < 0 {
            (i + len).max(0)
        } else {
            i.min(len)
        }
    };
    let (s, e) = (norm(start), norm(stop));
    if s >= e {
        return Vec::new();
    }
    items[s as usize..e as usize].to_vec()
}

/// python `html.escape(text, quote=True)`
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// python `==` for json-like values (1 == 1.0, dict order doesn't matter)
pub fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(i), Some(j)) => i == j,
            _ => x.as_f64() == y.as_f64(),
        },
        (Value::Bool(x), Value::Number(y)) | (Value::Number(y), Value::Bool(x)) => y.as_f64() == Some(*x as i64 as f64),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).map(|w| py_eq(v, w)).unwrap_or(false))
        }
        _ => a == b,
    }
}

/// python `bool(value)`; missing values are falsy
pub fn truthy_opt(v: Option<&Value>) -> bool {
    v.map(truthy).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ints() {
        assert_eq!(py_int_str(" 5 "), Some(5));
        assert_eq!(py_int_str("-60"), Some(-60));
        assert_eq!(py_int_str("1_000"), Some(1000));
        assert_eq!(py_int_str("abc"), None);
        assert_eq!(py_int_str(""), None);
        assert_eq!(py_int(&json!(60.9)), Some(60));
        assert_eq!(py_int(&json!(true)), Some(1));
    }

    #[test]
    fn slicing() {
        let v = vec![1, 2, 3, 4, 5];
        assert_eq!(py_slice(&v, 0, 2), vec![1, 2]);
        assert_eq!(py_slice(&v, 3, 50), vec![4, 5]);
        assert_eq!(py_slice(&v, -2, 48), vec![4, 5]);
        assert_eq!(py_slice(&v, -10, 1), vec![1]);
        assert!(py_slice(&v, 9, 20).is_empty());
    }

    #[test]
    fn escaping_and_strip() {
        assert_eq!(html_escape(r#"<a href="x">'&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;&#x27;&lt;/a&gt;");
        assert_eq!(or_strip(Some(&json!("  hi ")), "general").unwrap(), "hi");
        assert_eq!(or_strip(Some(&json!(null)), "general").unwrap(), "general");
        assert_eq!(or_strip(Some(&json!(0)), "").unwrap(), "");
        assert_eq!(or_strip(Some(&json!(5)), "").unwrap_err(), "'int' object has no attribute 'strip'");
        assert!(py_eq(&json!({"a": [1, 2.0]}), &json!({"a": [1.0, 2]})));
    }
}
