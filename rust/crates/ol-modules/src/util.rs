//! small python-compatibility helpers shared by the modules of this crate

use ol_core::channel::Channel;
use ol_core::module::CommandSpec;
use ol_core::pyjson::{py_repr, truthy};
use serde_json::Value;
use std::sync::Arc;

/// os.path.expanduser
pub fn expanduser(p: &str) -> String {
    if p == "~" || p.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home.trim_end_matches('/'), &p[1..]);
        }
    }
    p.to_string()
}

/// str.rstrip(os.path.sep)
pub fn rstrip_sep(p: &str) -> String {
    p.trim_end_matches(std::path::MAIN_SEPARATOR).to_string()
}

/// os.path.basename
pub fn basename(p: &str) -> String {
    match p.rfind('/') {
        Some(i) => p[i + 1..].to_string(),
        None => p.to_string(),
    }
}

/// python repr() of a string
pub fn repr_str(s: &str) -> String {
    py_repr(&Value::String(s.to_string()))
}

/// str() of a python OSError raised for `path`: "[Errno 2] No such file or directory: 'path'"
pub fn os_error(e: &std::io::Error, path: &str) -> String {
    match e.raw_os_error() {
        Some(code) => {
            let msg = e.to_string();
            let msg = match msg.rfind(" (os error ") {
                Some(i) => msg[..i].to_string(),
                None => msg,
            };
            format!("[Errno {}] {}: {}", code, msg, repr_str(path))
        }
        None => e.to_string(),
    }
}

/// python `open(path, 'r', encoding='utf-8').read()`: utf-8 decoding + universal newlines.
/// errors are formatted like str(exception).
pub fn read_text(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| os_error(&e, path))?;
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => return Err(utf8_decode_error(e.as_bytes()).unwrap_or_else(|| "'utf-8' codec can't decode bytes".into())),
    };
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// str(UnicodeDecodeError) for a utf-8 decode of `data` (CPython's utf-8 decoder rules: the
/// error span, the reason, and the single-byte or byte-range message form). None when `data` is valid.
pub fn utf8_decode_error(data: &[u8]) -> Option<String> {
    let end = data.len();
    let cont = |b: u8| (0x80..=0xbf).contains(&b);
    let mut s = 0usize;
    // (start, end, reason) of the first bad sequence; the scan stops at a truncated tail
    let failure: Option<(usize, usize, &str)> = loop {
        if s >= end {
            break None;
        }
        let ch = data[s];
        if ch < 0x80 {
            s += 1;
        } else if ch < 0xe0 {
            if ch < 0xc2 {
                break Some((s, s + 1, "invalid start byte"));
            }
            if end - s < 2 {
                break Some((s, end, "unexpected end of data"));
            }
            if !cont(data[s + 1]) {
                break Some((s, s + 1, "invalid continuation byte"));
            }
            s += 2;
        } else if ch < 0xf0 {
            let ch2 = data.get(s + 1).copied();
            let bad2 = ch2.is_some_and(|c| !cont(c) || (if c < 0xa0 { ch == 0xe0 } else { ch == 0xed }));
            if end - s < 3 {
                if end - s < 2 {
                    break Some((s, end, "unexpected end of data"));
                }
                if bad2 {
                    break Some((s, s + 1, "invalid continuation byte"));
                }
                break Some((s, end, "unexpected end of data"));
            }
            if bad2 {
                break Some((s, s + 1, "invalid continuation byte"));
            }
            if !cont(data[s + 2]) {
                break Some((s, s + 2, "invalid continuation byte"));
            }
            s += 3;
        } else if ch < 0xf5 {
            let ch2 = data.get(s + 1).copied();
            let bad2 = ch2.is_some_and(|c| !cont(c) || (if c < 0x90 { ch == 0xf0 } else { ch == 0xf4 }));
            if end - s < 4 {
                if end - s < 2 {
                    break Some((s, end, "unexpected end of data"));
                }
                if bad2 {
                    break Some((s, s + 1, "invalid continuation byte"));
                }
                if end - s < 3 {
                    break Some((s, end, "unexpected end of data"));
                }
                if !cont(data[s + 2]) {
                    break Some((s, s + 2, "invalid continuation byte"));
                }
                break Some((s, end, "unexpected end of data"));
            }
            if bad2 {
                break Some((s, s + 1, "invalid continuation byte"));
            }
            if !cont(data[s + 2]) {
                break Some((s, s + 2, "invalid continuation byte"));
            }
            if !cont(data[s + 3]) {
                break Some((s, s + 3, "invalid continuation byte"));
            }
            s += 4;
        } else {
            break Some((s, s + 1, "invalid start byte"));
        }
    };
    let (start, stop, reason) = failure?;
    Some(if stop == start + 1 {
        format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: {}", data[start], start, reason)
    } else {
        format!("'utf-8' codec can't decode bytes in position {}-{}: {}", start, stop - 1, reason)
    })
}

/// python str.splitlines()
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut iter = s.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        let is_break = matches!(
            c,
            '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        );
        if is_break {
            out.push(&s[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(j, '\n')) = iter.peek() {
                    end = j + 1;
                    iter.next();
                }
            }
            start = end;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// command specs out of the generated (spec, python method name) list
pub fn command_specs(v: Vec<(CommandSpec, &'static str)>) -> Vec<CommandSpec> {
    v.into_iter().map(|(c, _)| c).collect()
}

/// python: AttributeError raised when a module touches `self.channel.context` while there is no channel
pub fn no_channel() -> anyhow::Error {
    anyhow::anyhow!("'NoneType' object has no attribute 'context'")
}

/// `self.channel.context.chat.get("metadata").get("character")` (truthy?)
pub fn chat_has_character(ch: &Arc<Channel>) -> anyhow::Result<bool> {
    match ch.chat.get("metadata") {
        Some(Value::Object(m)) => Ok(m.get("character").map(truthy).unwrap_or(false)),
        Some(Value::Null) | None => Err(anyhow::anyhow!("'NoneType' object has no attribute 'get'")),
        Some(other) => Err(anyhow::anyhow!("'{}' object has no attribute 'get'", py_type_name(&other))),
    }
}

/// python type name of a JSON value
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

/// directory listing like os.listdir (names only)
pub fn listdir(path: &str) -> Result<Vec<String>, String> {
    let rd = std::fs::read_dir(path).map_err(|e| os_error(&e, path))?;
    Ok(rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitlines_like_python() {
        assert_eq!(splitlines("a\nb\r\nc\rd"), vec!["a", "b", "c", "d"]);
        assert_eq!(splitlines("a\n"), vec!["a"]);
        assert_eq!(splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert!(splitlines("").is_empty());
    }

    #[test]
    fn utf8_decode_errors_match_python_str() {
        // expected strings are python's str(UnicodeDecodeError) for the same bytes
        let msg = |b: &[u8]| utf8_decode_error(b).expect("invalid input");
        assert_eq!(msg(b"\xff"), "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte");
        assert_eq!(msg(b"ab\xc3"), "'utf-8' codec can't decode byte 0xc3 in position 2: unexpected end of data");
        assert_eq!(msg(b"\xe4\xb8"), "'utf-8' codec can't decode bytes in position 0-1: unexpected end of data");
        assert_eq!(msg(b"\xf0\x9f\x98"), "'utf-8' codec can't decode bytes in position 0-2: unexpected end of data");
        assert_eq!(msg(b"\xc3\x28"), "'utf-8' codec can't decode byte 0xc3 in position 0: invalid continuation byte");
        assert_eq!(msg(b"\xe4\xb8\xe4"), "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte");
        assert_eq!(msg(b"\xc0\x80"), "'utf-8' codec can't decode byte 0xc0 in position 0: invalid start byte");
        assert_eq!(msg(b"\xf5\x80\x80\x80"), "'utf-8' codec can't decode byte 0xf5 in position 0: invalid start byte");
        assert_eq!(msg(b"\xed\xa0\x80"), "'utf-8' codec can't decode byte 0xed in position 0: invalid continuation byte");
        assert!(utf8_decode_error("héllo ✓ 😀".as_bytes()).is_none());
    }

    #[test]
    fn os_error_format() {
        let e = std::fs::read("/nonexistent/x").unwrap_err();
        assert_eq!(os_error(&e, "/nonexistent/x"), "[Errno 2] No such file or directory: '/nonexistent/x'");
    }
}
