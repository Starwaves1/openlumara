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
        Err(e) => {
            let err = e.utf8_error();
            let pos = err.valid_up_to();
            let bad = &e.as_bytes()[pos..];
            // python's utf-8 decoder: lead bytes 0x80-0xc1 and 0xf5-0xff can never start a sequence
            let (reason, span) = match err.error_len() {
                Some(n) if matches!(bad[0], 0x80..=0xc1 | 0xf5..=0xff) => ("invalid start byte", n),
                Some(n) => ("invalid continuation byte", n),
                // the input ends inside a sequence, so python reports every remaining byte
                None => ("unexpected end of data", bad.len()),
            };
            return Err(if span == 1 {
                format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: {}", bad[0], pos, reason)
            } else {
                format!("'utf-8' codec can't decode bytes in position {}-{}: {}", pos, pos + span - 1, reason)
            });
        }
    };
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
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
    fn read_text_decode_errors_match_python() {
        let path = std::env::temp_dir().join(format!("read_text_errors_{}.txt", std::process::id()));
        let check = |bytes: &[u8], expected: &str| {
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(read_text(path.to_str().unwrap()).unwrap_err(), expected);
        };
        check(b"\xc0x", "'utf-8' codec can't decode byte 0xc0 in position 0: invalid start byte");
        check(b"ab\xf5", "'utf-8' codec can't decode byte 0xf5 in position 2: invalid start byte");
        check(b"\xe2\x82", "'utf-8' codec can't decode bytes in position 0-1: unexpected end of data");
        check(b"\xe2\x28\xa1", "'utf-8' codec can't decode byte 0xe2 in position 0: invalid continuation byte");
        check(b"\xf0\x9f\x28", "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte");
        std::fs::write(&path, "a\r\nb\rc".as_bytes()).unwrap();
        assert_eq!(read_text(path.to_str().unwrap()).unwrap(), "a\nb\nc");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn os_error_format() {
        let e = std::fs::read("/nonexistent/x").unwrap_err();
        assert_eq!(os_error(&e, "/nonexistent/x"), "[Errno 2] No such file or directory: '/nonexistent/x'");
    }
}
