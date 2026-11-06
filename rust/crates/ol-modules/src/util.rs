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
            // python reports the lead byte of the broken sequence and its offset in the file
            let err = e.utf8_error();
            let pos = err.valid_up_to();
            let b = e.as_bytes()[pos];
            let reason = if (0x80..=0xc1).contains(&b) || b >= 0xf5 {
                "invalid start byte"
            } else if err.error_len().is_none() {
                "unexpected end of data"
            } else {
                "invalid continuation byte"
            };
            return Err(format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: {}", b, pos, reason));
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
    fn os_error_format() {
        let e = std::fs::read("/nonexistent/x").unwrap_err();
        assert_eq!(os_error(&e, "/nonexistent/x"), "[Errno 2] No such file or directory: '/nonexistent/x'");
    }

    #[test]
    fn expanduser_and_basename_like_python() {
        assert_eq!(expanduser("/abs/x"), "/abs/x");
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(expanduser("~/notes.md"), format!("{}/notes.md", home.trim_end_matches('/')));
            assert_eq!(expanduser("~"), home);
        }
        assert_eq!(expanduser("~user/x"), "~user/x");
        assert_eq!(basename("a/b/c.txt"), "c.txt");
        assert_eq!(basename("a/b/"), "");
        assert_eq!(basename("plain"), "plain");
    }

    #[test]
    fn read_text_normalises_newlines_and_reports_decode_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok.txt");
        std::fs::write(&ok, b"one\r\ntwo\rthree\n").unwrap();
        assert_eq!(read_text(ok.to_str().unwrap()).unwrap(), "one\ntwo\nthree\n");

        let bad = dir.path().join("bad.txt");
        std::fs::write(&bad, b"ab\xff").unwrap();
        assert_eq!(
            read_text(bad.to_str().unwrap()).unwrap_err(),
            "'utf-8' codec can't decode byte 0xff in position 2: invalid start byte"
        );

        let trunc = dir.path().join("trunc.txt");
        std::fs::write(&trunc, b"x\xe4\xb8").unwrap();
        assert_eq!(
            read_text(trunc.to_str().unwrap()).unwrap_err(),
            "'utf-8' codec can't decode byte 0xe4 in position 1: unexpected end of data"
        );
    }

    #[test]
    fn py_type_name_matches_python_type_names() {
        assert_eq!(py_type_name(&Value::Null), "NoneType");
        assert_eq!(py_type_name(&serde_json::json!(true)), "bool");
        assert_eq!(py_type_name(&serde_json::json!(3)), "int");
        assert_eq!(py_type_name(&serde_json::json!(1.5)), "float");
        assert_eq!(py_type_name(&serde_json::json!("s")), "str");
        assert_eq!(py_type_name(&serde_json::json!([1])), "list");
        assert_eq!(py_type_name(&serde_json::json!({})), "dict");
    }
}
