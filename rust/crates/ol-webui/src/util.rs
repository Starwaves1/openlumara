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

/// python `str.title()`: a run of cased characters starts with its titlecase mapping and the
/// rest of the run is lowercased (capital sigma takes python's final-sigma form when it ends a
/// word). uncased characters (CJK, digits, punctuation) only separate words.
/// std has no titlecase table, so `push_titlecase` lists the mappings that differ from uppercase.
/// known gap: the multi-char Greek/Armenian titlecase specials (e.g. U+0390) stay uppercase.
pub fn py_title(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut previous_cased = false;
    for (i, &c) in chars.iter().enumerate() {
        if !previous_cased {
            push_titlecase(c, &mut out);
        } else if c == '\u{3a3}' {
            let next_cased = chars.get(i + 1).map(|&n| is_cased(n)).unwrap_or(false);
            out.push(if next_cased { '\u{3c3}' } else { '\u{3c2}' });
        } else {
            out.extend(c.to_lowercase());
        }
        previous_cased = is_cased(c);
    }
    out
}

/// unicode `Cased` property (python `str.title` word material): Lowercase, Uppercase and Lt
fn is_cased(c: char) -> bool {
    c.is_lowercase() || c.is_uppercase() || is_titlecase_letter(c)
}

/// the Lt (titlecase letter) general category: the digraphs and the Greek prosgegrammeni forms
fn is_titlecase_letter(c: char) -> bool {
    matches!(c, '\u{1c5}' | '\u{1c8}' | '\u{1cb}' | '\u{1f2}' | '\u{1f88}'..='\u{1f8f}' | '\u{1f98}'..='\u{1f9f}' | '\u{1fa8}'..='\u{1faf}' | '\u{1fbc}' | '\u{1fcc}' | '\u{1ffc}')
}

/// titlecase mapping of one char (SpecialCasing / UnicodeData titlecase entries that differ from
/// uppercase), falling back to uppercase
fn push_titlecase(c: char, out: &mut String) {
    let mapped: &str = match c {
        '\u{df}' => "Ss",
        '\u{1c4}'..='\u{1c6}' => "\u{1c5}",
        '\u{1c7}'..='\u{1c9}' => "\u{1c8}",
        '\u{1ca}'..='\u{1cc}' => "\u{1cb}",
        '\u{1f1}'..='\u{1f3}' => "\u{1f2}",
        '\u{149}' => "\u{2bc}N",
        '\u{1f0}' => "J\u{30c}",
        '\u{1e96}' => "H\u{331}",
        '\u{1e97}' => "T\u{308}",
        '\u{1e98}' => "W\u{30a}",
        '\u{1e99}' => "Y\u{30a}",
        '\u{1e9a}' => "A\u{2be}",
        '\u{fb00}' => "Ff",
        '\u{fb01}' => "Fi",
        '\u{fb02}' => "Fl",
        '\u{fb03}' => "Ffi",
        '\u{fb04}' => "Ffl",
        '\u{fb05}' | '\u{fb06}' => "St",
        '\u{1f80}'..='\u{1f87}' | '\u{1f90}'..='\u{1f97}' | '\u{1fa0}'..='\u{1fa7}' => {
            let base = match c as u32 {
                x if x < 0x1f90 => 0x1f88,
                x if x < 0x1fa0 => 0x1f98,
                _ => 0x1fa8,
            };
            out.push(char::from_u32(base + (c as u32 & 7)).unwrap_or(c));
            return;
        }
        '\u{1fb3}' => {
            out.push('\u{1fbc}');
            return;
        }
        '\u{1fc3}' => {
            out.push('\u{1fcc}');
            return;
        }
        '\u{1ff3}' => {
            out.push('\u{1ffc}');
            return;
        }
        c => {
            out.extend(c.to_uppercase());
            return;
        }
    };
    out.push_str(mapped);
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

/// python `Webui.host` for `network_mode` (`local` 127.0.0.1, `internet` 0.0.0.0, `custom` the
/// configured `custom_host`, anything else 127.0.0.1). `None` when custom mode has no custom_host:
/// uvicorn passes that None to asyncio.create_server, which binds every interface
pub fn bind_host(network_mode: Option<&str>, custom_host: Option<&str>) -> Option<String> {
    match network_mode {
        Some("local") => Some("127.0.0.1".to_string()),
        Some("internet") => Some("0.0.0.0".to_string()),
        Some("custom") => custom_host.map(str::to_string),
        _ => Some("127.0.0.1".to_string()),
    }
}

/// python `Webui.url`: `http://{self.host}:{self.port}`, where an unset (None) host renders as "None"
pub fn channel_url(host: Option<&str>, port: impl std::fmt::Display) -> String {
    format!("http://{}:{}", host.unwrap_or("None"), port)
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
        // titlecase differs from uppercase for these (python: 'ß'.title() == 'Ss')
        assert_eq!(py_title("\u{df}a"), "Ssa");
        assert_eq!(py_title("\u{1c6}a b"), "\u{1c5}a B");
        assert_eq!(py_title("\u{fb01}x"), "Fix");
        assert_eq!(py_title("\u{149}a"), "\u{2bc}Na");
        // capital sigma at the end of a word is the final form, as in python
        assert_eq!(py_title("\u{391}\u{394}\u{39f}\u{3a3}"), "\u{391}\u{3b4}\u{3bf}\u{3c2}");
        assert_eq!(py_title("a\u{130}y"), "A\u{69}\u{307}y");
    }

    #[test]
    fn bind_host_and_url() {
        assert_eq!(bind_host(Some("local"), Some("10.0.0.5")), Some("127.0.0.1".to_string()));
        assert_eq!(bind_host(Some("internet"), None), Some("0.0.0.0".to_string()));
        assert_eq!(bind_host(Some("custom"), Some("10.0.0.5")), Some("10.0.0.5".to_string()));
        // python: custom mode without custom_host has host None (every interface is bound)...
        assert_eq!(bind_host(Some("custom"), None), None);
        // ...while the url f-string renders that None as "None"
        assert_eq!(channel_url(bind_host(Some("custom"), None).as_deref(), 8080), "http://None:8080");
        // unknown or missing mode falls back to loopback
        assert_eq!(bind_host(Some("lan"), Some("10.0.0.5")), Some("127.0.0.1".to_string()));
        assert_eq!(bind_host(None, None), Some("127.0.0.1".to_string()));
        assert_eq!(channel_url(bind_host(Some("local"), None).as_deref(), 8080), "http://127.0.0.1:8080");
    }
}
