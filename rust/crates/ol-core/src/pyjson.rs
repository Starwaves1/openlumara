//! Python-compatible serialization helpers.
//!
//! The Python code base relies on `json.dumps` output in a few places that are observable
//! (token estimation = `len(json.dumps(x)) // 4`, tool results sent back to the model,
//! files written to disk) and on `str()`/`repr()` of dicts in others (word counts,
//! f-string formatting). These helpers reproduce CPython's formatting exactly.

use serde_json::{Map, Value};
use std::fmt::Write;

/// Python truthiness of a JSON-ish value
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub fn truthy_opt(v: Option<&Value>) -> bool {
    v.map(truthy).unwrap_or(false)
}

/// CPython float repr()
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sci = format!("{:e}", f); // shortest roundtrip, e.g. "1.5e-7"
    let (mant, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-4..16).contains(&exp) {
        let s = format!("{}", f);
        if s.contains('.') {
            s
        } else {
            format!("{}.0", s)
        }
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{}{:02}", mant, sign, exp.abs())
    }
}

/// float repr() used by json.dumps (same as repr but nan/inf spelled the json way)
fn json_number(n: &serde_json::Number, out: &mut String) {
    if n.is_f64() {
        out.push_str(&float_repr(n.as_f64().unwrap()));
    } else {
        out.push_str(&n.to_string());
    }
}

fn json_string(s: &str, ensure_ascii: bool, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if ensure_ascii && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{:04x}", unit);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[derive(Clone, Copy, Debug)]
pub struct DumpOpts {
    pub indent: Option<usize>,
    pub ensure_ascii: bool,
    pub sort_keys: bool,
}

impl Default for DumpOpts {
    fn default() -> Self {
        Self { indent: None, ensure_ascii: true, sort_keys: false }
    }
}

fn dump_value(v: &Value, opts: &DumpOpts, level: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => json_number(n, out),
        Value::String(s) => json_string(s, opts.ensure_ascii, out),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if opts.indent.is_none() {
                        out.push(' ');
                    }
                }
                if let Some(ind) = opts.indent {
                    out.push('\n');
                    out.push_str(&" ".repeat(ind * (level + 1)));
                }
                dump_value(item, opts, level + 1, out);
            }
            if let Some(ind) = opts.indent {
                out.push('\n');
                out.push_str(&" ".repeat(ind * level));
            }
            out.push(']');
        }
        Value::Object(o) => {
            if o.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            let mut keys: Vec<&String> = o.keys().collect();
            if opts.sort_keys {
                keys.sort();
            }
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if opts.indent.is_none() {
                        out.push(' ');
                    }
                }
                if let Some(ind) = opts.indent {
                    out.push('\n');
                    out.push_str(&" ".repeat(ind * (level + 1)));
                }
                json_string(k, opts.ensure_ascii, out);
                out.push_str(": ");
                dump_value(&o[k.as_str()], opts, level + 1, out);
            }
            if let Some(ind) = opts.indent {
                out.push('\n');
                out.push_str(&" ".repeat(ind * level));
            }
            out.push('}');
        }
    }
}

/// json.dumps(v) with Python defaults (ensure_ascii=True, separators (', ', ': '))
pub fn dumps(v: &Value) -> String {
    dumps_opts(v, DumpOpts::default())
}

/// json.dumps(v, indent=2)
pub fn dumps_indent(v: &Value, indent: usize) -> String {
    dumps_opts(v, DumpOpts { indent: Some(indent), ..Default::default() })
}

pub fn dumps_opts(v: &Value, opts: DumpOpts) -> String {
    let mut out = String::new();
    dump_value(v, &opts, 0, &mut out);
    out
}

fn repr_str(s: &str, out: &mut String) {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
}

fn repr_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("None"),
        Value::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
        Value::Number(n) => {
            if n.is_f64() {
                let f = n.as_f64().unwrap();
                if f.is_nan() {
                    out.push_str("nan")
                } else if f.is_infinite() {
                    out.push_str(if f > 0.0 { "inf" } else { "-inf" })
                } else {
                    out.push_str(&float_repr(f))
                }
            } else {
                out.push_str(&n.to_string())
            }
        }
        Value::String(s) => repr_str(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                repr_value(item, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            for (i, (k, val)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                repr_str(k, out);
                out.push_str(": ");
                repr_value(val, out);
            }
            out.push('}');
        }
    }
}

/// Python repr() of a JSON-ish value
pub fn py_repr(v: &Value) -> String {
    let mut out = String::new();
    repr_value(v, &mut out);
    out
}

/// Python str() of a JSON-ish value (strings are returned raw)
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// Python str() of an optional value (None -> "None")
pub fn py_str_opt(v: Option<&Value>) -> String {
    match v {
        Some(v) => py_str(v),
        None => "None".into(),
    }
}

/// Python `.split()` word count
pub fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

/// get a nested value: get_path(v, &["a","b"])
pub fn get_in<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for k in keys {
        cur = cur.as_object()?.get(*k)?;
    }
    Some(cur)
}

pub fn obj_get_str<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    o.get(key).and_then(|v| v.as_str())
}

/// Python-ish conversion of a Value to i64 (bools/floats/numeric strings accepted)
pub fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::Bool(b) => Some(*b as i64),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

pub fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Python string `.capitalize()`
pub fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

/// Python string `.title()`
pub fn title(s: &str) -> String {
    let mut out = String::new();
    let mut prev_alpha = false;
    for c in s.chars() {
        if c.is_alphabetic() {
            if prev_alpha {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            prev_alpha = true;
        } else {
            out.push(c);
            prev_alpha = false;
        }
    }
    out
}

/// slice a string by character count like Python's s[:n]
pub fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dumps_matches_python() {
        let v = json!({"a": 1, "b": [1.5, true, null], "c": "h\u{e9}llo\n", "d": {}});
        assert_eq!(dumps(&v), "{\"a\": 1, \"b\": [1.5, true, null], \"c\": \"h\\u00e9llo\\n\", \"d\": {}}");
        assert_eq!(
            dumps_indent(&json!({"a": [1, 2], "b": {}}), 2),
            "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {}\n}"
        );
        assert_eq!(dumps(&json!("\u{1F600}")), "\"\\ud83d\\ude00\"");
    }

    #[test]
    fn float_repr_matches_python() {
        assert_eq!(float_repr(0.7), "0.7");
        assert_eq!(float_repr(1.0), "1.0");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1e-5), "1e-05");
        assert_eq!(float_repr(0.0001), "0.0001");
        assert_eq!(float_repr(123456789.5), "123456789.5");
    }

    #[test]
    fn repr_matches_python() {
        let v = json!([{"role": "system", "content": "it's\nok"}, true, null, 2.0]);
        assert_eq!(py_repr(&v), r#"[{'role': 'system', 'content': "it's\nok"}, True, None, 2.0]"#);
    }

    #[test]
    fn title_and_capitalize() {
        assert_eq!(title("hello world_x"), "Hello World_X");
        assert_eq!(capitalize("hELLO"), "Hello");
    }
}
