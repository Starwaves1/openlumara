//! small python-semantics helpers used by the webui port

use ol_core::pyjson::truthy;
use once_cell::sync::Lazy;
use regex::Regex;
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

/// python `str.strip()` with no arguments: removes every char for which `str.isspace()` holds.
/// rust's `trim()` (White_Space) misses U+001C-U+001F, which python treats as whitespace.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// python `(value or default).strip()`: falsy values become `default`, truthy
/// non-strings raise AttributeError (returned as the python error message)
pub fn or_strip(v: Option<&Value>, default: &str) -> Result<String, String> {
    match v {
        Some(v) if truthy(v) => match v {
            Value::String(s) => Ok(py_strip(s).to_string()),
            other => Err(format!("'{}' object has no attribute 'strip'", py_type_name(other))),
        },
        _ => Ok(py_strip(default).to_string()),
    }
}

/// python `value.strip()` for `data.get(key, default)` (no `or` fallback)
pub fn get_strip(v: Option<&Value>, default: &str) -> Result<String, String> {
    match v {
        None => Ok(py_strip(default).to_string()),
        Some(Value::String(s)) => Ok(py_strip(s).to_string()),
        Some(other) => Err(format!("'{}' object has no attribute 'strip'", py_type_name(other))),
    }
}

static DECIMAL_DIGIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\p{Nd}$").unwrap());

/// value of a unicode decimal digit (category Nd), as python's int() accepts them.
/// unicode guarantees each Nd run is made of blocks of ten digits that start at zero,
/// so the offset from the start of the contiguous run gives the digit value
fn decimal_value(c: char) -> Option<u32> {
    if c.is_ascii_digit() {
        return Some(c as u32 - '0' as u32);
    }
    let is_nd = |c: char| DECIMAL_DIGIT.is_match(c.encode_utf8(&mut [0; 4]));
    if !is_nd(c) {
        return None;
    }
    let mut start = c as u32;
    while start > 0 && char::from_u32(start - 1).map(is_nd).unwrap_or(false) {
        start -= 1;
    }
    Some((c as u32 - start) % 10)
}

/// python `int(str)`: surrounding whitespace (python's definition) allowed, optional sign,
/// unicode decimal digits, single underscores between digits. Values beyond the i64 range
/// saturate: every caller clamps (list slicing) or overflows the same way python does.
pub fn py_int_str(s: &str) -> Option<i64> {
    const BIG: i128 = 1 << 70;
    let t = py_strip(s);
    let (negative, body) = match t.chars().next()? {
        '+' => (false, &t[1..]),
        '-' => (true, &t[1..]),
        _ => (false, t),
    };
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return None;
    }
    let mut value: i128 = 0;
    for c in body.chars().filter(|c| *c != '_') {
        value = (value * 10 + decimal_value(c)? as i128).min(BIG);
    }
    let value = if negative { -value } else { value };
    Some(value.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
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

/// python `str.title()`: the first cased char of each run of cased chars is upper-cased,
/// the rest lower-cased. uncased chars (CJK, Hebrew, digits, punctuation) only separate words.
/// (rust has no titlecase mapping in std, so ǆ-style digraphs become their uppercase form)
pub fn py_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut previous_cased = false;
    for c in s.chars() {
        if previous_cased {
            out.extend(c.to_lowercase());
        } else {
            out.extend(c.to_uppercase());
        }
        previous_cased = c.is_uppercase() || c.is_lowercase();
    }
    out
}

/// markupsafe `escape` (what jinja2 autoescape applies to plain str values):
/// `& < > " '` become `&amp; &lt; &gt; &#34; &#39;`, nothing else is touched
pub fn markup_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
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

/// python `Webui` start-up: the bind host for `network_mode` (`local` 127.0.0.1, `internet`
/// 0.0.0.0, `custom` the configured `custom_host`, anything else 127.0.0.1). python's f-string
/// renders a missing custom host as "None"
pub fn bind_host(network_mode: Option<&str>, custom_host: Option<&str>) -> String {
    match network_mode {
        Some("local") => "127.0.0.1".to_string(),
        Some("internet") => "0.0.0.0".to_string(),
        Some("custom") => custom_host.unwrap_or("None").to_string(),
        _ => "127.0.0.1".to_string(),
    }
}

/// python `Webui.url`: `http://{host}:{port}`
pub fn channel_url(host: &str, port: impl std::fmt::Display) -> String {
    format!("http://{}:{}", host, port)
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
        assert_eq!(markup_escape("/assets/a&b'<\"x\">"), "/assets/a&amp;b&#39;&lt;&#34;x&#34;&gt;");
    }

    #[test]
    fn python_strip_and_int_semantics() {
        // python str.isspace() includes the file/group/record/unit separators
        assert_eq!(py_strip("\u{1c}x\u{1f}"), "x");
        assert_eq!(or_strip(Some(&json!("\u{1d}title\u{1e}")), "general").unwrap(), "title");
        assert_eq!(py_int_str("\u{1c}7"), Some(7));
        // unicode decimal digits (Nd) are accepted by python int()
        assert_eq!(py_int_str("\u{0665}\u{0664}"), Some(54));
        assert_eq!(py_int_str("\u{1d7db}"), Some(3));
        assert_eq!(py_int_str("\u{00b2}"), None);
        // arbitrary-size values saturate: callers clamp, so the result matches python's
        assert_eq!(py_int_str("99999999999999999999999"), Some(i64::MAX));
        assert_eq!(py_int_str("-99999999999999999999999"), Some(i64::MIN));
        assert_eq!(py_int_str("1__0"), None);
    }

    #[test]
    fn python_title() {
        assert_eq!(py_title("hello world-foo"), "Hello World-Foo");
        // python title() only counts cased characters as word material
        assert_eq!(py_title("\u{4e2d}a"), "\u{4e2d}A");
        assert_eq!(py_title("don't"), "Don'T");
    }

    #[test]
    fn bind_host_and_url() {
        assert_eq!(bind_host(Some("local"), Some("10.0.0.5")), "127.0.0.1");
        assert_eq!(bind_host(Some("internet"), None), "0.0.0.0");
        assert_eq!(bind_host(Some("custom"), Some("10.0.0.5")), "10.0.0.5");
        // python: an unset custom_host becomes the string "None" in the url
        assert_eq!(bind_host(Some("custom"), None), "None");
        // unknown or missing mode falls back to loopback
        assert_eq!(bind_host(Some("lan"), Some("10.0.0.5")), "127.0.0.1");
        assert_eq!(bind_host(None, None), "127.0.0.1");
        assert_eq!(channel_url(&bind_host(Some("local"), None), 8080), "http://127.0.0.1:8080");
    }
}
