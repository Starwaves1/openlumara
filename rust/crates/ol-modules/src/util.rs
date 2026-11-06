//! small python-compatibility helpers shared by the modules of this crate

use ol_core::channel::Channel;
use ol_core::module::CommandSpec;
use ol_core::pyjson::truthy;
use serde_json::Value;
use std::ffi::{CStr, CString, OsString};
use std::sync::Arc;
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

/// os.path.expanduser
pub fn expanduser(p: &str) -> String {
    expanduser_with(p, std::env::var_os("HOME"), passwd_home)
}

/// os.path.expanduser with the environment and the passwd database injected.
/// `passwd_home(None)` is the home of the current uid, `passwd_home(Some(name))` the home of `name`.
fn expanduser_with(p: &str, home_env: Option<OsString>, passwd_home: impl Fn(Option<&str>) -> Option<String>) -> String {
    let Some(rest) = p.strip_prefix('~') else {
        return p.to_string();
    };
    // python: i = path.find(sep, 1); the user name is path[1:i], the remainder is path[i:]
    let i = rest.find('/').unwrap_or(rest.len());
    let userhome = if i == 0 {
        match home_env {
            Some(h) => h.to_string_lossy().into_owned(),
            None => match passwd_home(None) {
                Some(h) => h,
                None => return p.to_string(),
            },
        }
    } else {
        match passwd_home(Some(&rest[..i])) {
            Some(h) => h,
            None => return p.to_string(),
        }
    };
    let joined = format!("{}{}", userhome.trim_end_matches('/'), &rest[i..]);
    // python: (userhome + path[i:]) or sep
    if joined.is_empty() {
        "/".to_string()
    } else {
        joined
    }
}

/// home directory from the passwd database: of the user `name`, or of the current uid when `name` is None
fn passwd_home(name: Option<&str>) -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16384];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = match name {
        Some(n) => {
            let cname = CString::new(n).ok()?;
            unsafe { libc::getpwnam_r(cname.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) }
        }
        None => unsafe { libc::getpwuid_r(libc::getuid(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) },
    };
    if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(pwd.pw_dir) }.to_string_lossy().into_owned())
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

/// python repr() of a string (CPython's rules: quote choice, escapes, and \xNN/\uNNNN/\UNNNNNNNN
/// for every code point that str.isprintable() rejects)
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            ' ' => out.push(' '),
            c if is_printable(c) => out.push(c),
            c => {
                let cp = c as u32;
                if cp <= 0xff {
                    out.push_str(&format!("\\x{:02x}", cp));
                } else if cp <= 0xffff {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    out.push_str(&format!("\\U{:08x}", cp));
                }
            }
        }
    }
    out.push(quote);
    out
}

/// python str.isprintable() for one character: every category except Cc, Cf, Cs, Co, Cn, Zl, Zp, Zs
fn is_printable(c: char) -> bool {
    !matches!(
        c.general_category(),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::Surrogate
            | GeneralCategory::PrivateUse
            | GeneralCategory::Unassigned
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
            | GeneralCategory::SpaceSeparator
    )
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
            return Err(utf8_decode_error(e.as_bytes(), &err));
        }
    };
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// str(UnicodeDecodeError) for a strict utf-8 decode of `bytes` that failed with `err`.
/// the span is [start, end) like python's: one byte gets the singular form, longer spans the plural.
fn utf8_decode_error(bytes: &[u8], err: &std::str::Utf8Error) -> String {
    let start = err.valid_up_to();
    // a sequence cut off by the end of the data runs to the end of the data
    let end = match err.error_len() {
        Some(n) => start + n,
        None => bytes.len(),
    };
    let b = bytes[start];
    let reason = if (0x80..=0xc1).contains(&b) || b >= 0xf5 {
        "invalid start byte"
    } else if err.error_len().is_none() {
        "unexpected end of data"
    } else {
        "invalid continuation byte"
    };
    if end == start + 1 {
        format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: {}", b, start, reason)
    } else {
        format!("'utf-8' codec can't decode bytes in position {}-{}: {}", start, end - 1, reason)
    }
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

/// os.listdir: names only. a read error part way through fails the whole listing, like python.
/// names that are not valid utf-8 are lossy (python keeps them via surrogateescape).
pub fn listdir(path: &str) -> Result<Vec<String>, String> {
    let rd = std::fs::read_dir(path).map_err(|e| os_error(&e, path))?;
    let mut names = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|e| os_error(&e, path))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// passwd lookup stub: a fixed home for the current uid and for "root" only
    fn fake_passwd(name: Option<&str>) -> Option<String> {
        match name {
            None => Some("/home/uid".to_string()),
            Some("root") => Some("/root/".to_string()),
            Some(_) => None,
        }
    }

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
    fn expanduser_follows_python_rules() {
        let home = |h: &str| Some(OsString::from(h));
        assert_eq!(expanduser_with("/abs/x", home("/h"), fake_passwd), "/abs/x");
        assert_eq!(expanduser_with("~/notes.md", home("/h/"), fake_passwd), "/h/notes.md");
        assert_eq!(expanduser_with("~", home("/h"), fake_passwd), "/h");
        // HOME='/' or '' with '~' gives '/', not '' (an empty path would mean the project root)
        assert_eq!(expanduser_with("~", home("/"), fake_passwd), "/");
        assert_eq!(expanduser_with("~", home(""), fake_passwd), "/");
        assert_eq!(expanduser_with("~/x", home("/"), fake_passwd), "/x");
        // HOME unset falls back to the passwd entry of the current uid
        assert_eq!(expanduser_with("~/docs", None, fake_passwd), "/home/uid/docs");
        assert_eq!(expanduser_with("~/docs", None, |_| None), "~/docs");
        // ~user expands through the passwd database; unknown users stay literal
        assert_eq!(expanduser_with("~root/x", home("/h"), fake_passwd), "/root/x");
        assert_eq!(expanduser_with("~nobody_here/x", home("/h"), fake_passwd), "~nobody_here/x");
    }

    #[test]
    fn expanduser_and_basename_against_real_environment() {
        assert_eq!(expanduser("/abs/x"), "/abs/x");
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(expanduser("~/notes.md"), format!("{}/notes.md", home.trim_end_matches('/')));
        }
        if let Some(root) = passwd_home(Some("root")) {
            assert_eq!(expanduser("~root/x"), format!("{}/x", root.trim_end_matches('/')));
        }
        assert_eq!(expanduser("~no_such_user_olmods/x"), "~no_such_user_olmods/x");
        assert_eq!(basename("a/b/c.txt"), "c.txt");
        assert_eq!(basename("a/b/"), "");
        assert_eq!(basename("plain"), "plain");
    }

    #[test]
    fn repr_str_matches_cpython_repr() {
        assert_eq!(repr_str("plain"), "'plain'");
        assert_eq!(repr_str("it's"), "\"it's\"");
        assert_eq!(repr_str("a\tb\n\\"), "'a\\tb\\n\\\\'");
        assert_eq!(repr_str("\u{1}\u{7f}"), "'\\x01\\x7f'");
        // C1 controls, no-break space, zero width space, and an unassigned-free emoji
        assert_eq!(repr_str("a\u{85}b\u{80}\u{200b}"), "'a\\x85b\\x80\\u200b'");
        assert_eq!(repr_str("x\u{a0}y"), "'x\\xa0y'");
        assert_eq!(repr_str("caf\u{e9} \u{1f600}"), "'caf\u{e9} \u{1f600}'");
        assert_eq!(repr_str("\u{e000}"), "'\\ue000'");
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
            "'utf-8' codec can't decode bytes in position 1-2: unexpected end of data"
        );
    }

    #[test]
    fn decode_error_spans_match_cpython() {
        let dir = tempfile::tempdir().unwrap();
        let cases: [(&[u8], &str); 6] = [
            (&b"ab\xe4"[..], "'utf-8' codec can't decode byte 0xe4 in position 2: unexpected end of data"),
            (&b"x\xe4\xb8A"[..], "'utf-8' codec can't decode bytes in position 1-2: invalid continuation byte"),
            (&b"x\xf0\x9f\x98A"[..], "'utf-8' codec can't decode bytes in position 1-3: invalid continuation byte"),
            (&b"\xf0\x9f("[..], "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte"),
            (&b"\xf0\x9f"[..], "'utf-8' codec can't decode bytes in position 0-1: unexpected end of data"),
            (&b"x\xe4A"[..], "'utf-8' codec can't decode byte 0xe4 in position 1: invalid continuation byte"),
        ];
        for (i, (bytes, want)) in cases.iter().enumerate() {
            let p = dir.path().join(format!("case{}.txt", i));
            std::fs::write(&p, bytes).unwrap();
            assert_eq!(read_text(p.to_str().unwrap()).unwrap_err(), *want);
        }
    }

    #[test]
    fn listdir_lists_names_and_reports_missing_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut names = listdir(dir.path().to_str().unwrap()).unwrap();
        names.sort();
        assert_eq!(names, vec!["a.md", "sub"]);

        let missing = dir.path().join("nope");
        assert_eq!(
            listdir(missing.to_str().unwrap()).unwrap_err(),
            format!("[Errno 2] No such file or directory: {}", repr_str(missing.to_str().unwrap()))
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
