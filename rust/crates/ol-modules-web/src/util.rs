//! Python-compatible helpers for the http module: `urllib.parse.urljoin`, and `json.loads` /
//! `json.dumps(ensure_ascii=False)` with python's results (NaN and Infinity literals, integers of any
//! size kept exact, python's float repr).

use crate::pytext::urlsplit;
use crate::sanitize::ContentSanitizer;
use ol_core::pyjson::float_repr;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// python `urllib.parse.uses_relative` / `uses_netloc` (python 3.14)
const USES_RELATIVE: &[&str] = &[
    "", "ftp", "http", "gopher", "nntp", "imap", "wais", "file", "https", "shttp", "mms", "prospero", "rtsp", "rtsps", "rtspu", "sftp", "svn", "svn+ssh", "ws", "wss",
];
const USES_NETLOC: &[&str] = &[
    "", "ftp", "http", "gopher", "nntp", "telnet", "imap", "wais", "file", "mms", "https", "shttp", "snews", "prospero", "rtsp", "rtsps", "rtspu", "rsync", "svn",
    "svn+ssh", "sftp", "nfs", "git", "git+ssh", "ws", "wss", "itms-services",
];

/// python `urlunsplit`
fn urlunsplit(scheme: &str, netloc: &str, url: &str, query: &str, fragment: &str) -> String {
    let mut url = url.to_string();
    if !netloc.is_empty() || (!scheme.is_empty() && USES_NETLOC.contains(&scheme) && !url.starts_with("//")) {
        if !url.is_empty() && !url.starts_with('/') {
            url = format!("/{}", url);
        }
        url = format!("//{}{}", netloc, url);
    }
    if !scheme.is_empty() {
        url = format!("{}:{}", scheme, url);
    }
    if !query.is_empty() {
        url = format!("{}?{}", url, query);
    }
    if !fragment.is_empty() {
        url = format!("{}#{}", url, fragment);
    }
    url
}

/// python `urllib.parse.urljoin(base, url)`. `;params` stay in the path (joining them back gives the
/// same string). Err = the ValueError that `urlsplit` raises for a malformed IPv6 netloc.
pub fn urljoin(base: &str, url: &str) -> Result<String, String> {
    if base.is_empty() {
        return Ok(url.to_string());
    }
    if url.is_empty() {
        return Ok(base.to_string());
    }
    let b = urlsplit(base)?;
    let mut u = urlsplit(url)?;
    if u.scheme.is_empty() {
        u.scheme = b.scheme.clone();
    }
    if u.scheme != b.scheme || !USES_RELATIVE.contains(&u.scheme.as_str()) {
        return Ok(url.to_string());
    }
    let mut netloc = u.netloc.clone();
    if USES_NETLOC.contains(&u.scheme.as_str()) {
        if !u.netloc.is_empty() {
            return Ok(urlunsplit(&u.scheme, &u.netloc, &u.path, &u.query, &u.fragment));
        }
        netloc = b.netloc.clone();
    }
    if u.path.is_empty() {
        let query = if u.query.is_empty() { b.query.as_str() } else { u.query.as_str() };
        return Ok(urlunsplit(&u.scheme, &netloc, &b.path, query, &u.fragment));
    }
    let mut base_parts: Vec<&str> = b.path.split('/').collect();
    if base_parts.last() != Some(&"") {
        base_parts.pop();
    }
    let segments: Vec<&str> = if u.path.starts_with('/') {
        u.path.split('/').collect()
    } else {
        let mut segs = base_parts;
        segs.extend(u.path.split('/'));
        let n = segs.len();
        if n > 2 {
            let mut out = vec![segs[0]];
            out.extend(segs[1..n - 1].iter().copied().filter(|s| !s.is_empty()));
            out.push(segs[n - 1]);
            out
        } else {
            segs
        }
    };
    let mut resolved: Vec<&str> = Vec::new();
    for seg in &segments {
        match *seg {
            ".." => {
                resolved.pop();
            }
            "." => {}
            other => resolved.push(other),
        }
    }
    if matches!(segments.last(), Some(&".") | Some(&"..")) {
        resolved.push("");
    }
    let path = resolved.join("/");
    let path = if path.is_empty() { "/" } else { path.as_str() };
    Ok(urlunsplit(&u.scheme, &netloc, path, &u.query, &u.fragment))
}

/// The text python's `json.loads(text)` raises with, for the ddgs engines that parse json.
/// serde_json decides whether the text parses; when it does not, this reproduces CPython's
/// `JSONDecodeError` message (`"Expecting value: line 1 column 1 (char 0)"`). A text CPython
/// accepts but serde rejects (NaN, Infinity, lone surrogates) keeps serde's own text.
pub fn py_json_loads(text: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(text).map_err(|e| py_json_error(text).unwrap_or_else(|| e.to_string()))
}

/// CPython's `JSONDecodeError` text for `text`, or None when CPython's decoder accepts it.
fn py_json_error(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let scan = JsonScan { c: &chars };
    let start = scan.ws(0);
    let (msg, pos) = match scan.value(start) {
        Err(e) => e,
        Ok(end) => {
            let end = scan.ws(end);
            if end == chars.len() {
                return None;
            }
            ("Extra data", end)
        }
    };
    // lineno/colno as JSONDecodeError computes them; pos counts code points, as python's str does
    let before = &chars[..pos];
    let lineno = before.iter().filter(|c| **c == '\n').count() + 1;
    let colno = match before.iter().rposition(|c| *c == '\n') {
        Some(nl) => pos - nl,
        None => pos + 1,
    };
    Some(format!("{msg}: line {lineno} column {colno} (char {pos})"))
}

/// a failure position as `(message, index into the code points)`
type JsonErr = (&'static str, usize);

/// CPython's `json` scanner (C flavour) walking code points; only the error positions matter here
struct JsonScan<'a> {
    c: &'a [char],
}

impl JsonScan<'_> {
    fn at(&self, i: usize) -> Option<char> {
        self.c.get(i).copied()
    }

    fn ws(&self, mut i: usize) -> usize {
        while matches!(self.at(i), Some(' ' | '\t' | '\n' | '\r')) {
            i += 1;
        }
        i
    }

    fn starts_with(&self, i: usize, word: &str) -> bool {
        word.chars().enumerate().all(|(k, ch)| self.at(i + k) == Some(ch))
    }

    /// the value starting at `i`; returns the index just past it
    fn value(&self, i: usize) -> Result<usize, JsonErr> {
        match self.at(i) {
            Some('"') => self.string(i),
            Some('{') => self.object(i),
            Some('[') => self.array(i),
            Some('n') => self.literal(i, "null"),
            Some('t') => self.literal(i, "true"),
            Some('f') => self.literal(i, "false"),
            Some('N') => self.literal(i, "NaN"),
            Some('I') => self.literal(i, "Infinity"),
            Some('-') if self.starts_with(i, "-Infinity") => Ok(i + "-Infinity".len()),
            _ => self.number(i),
        }
    }

    fn literal(&self, i: usize, word: &str) -> Result<usize, JsonErr> {
        if self.starts_with(i, word) {
            Ok(i + word.len())
        } else {
            Err(("Expecting value", i))
        }
    }

    /// `(-?(?:0|[1-9]\d*))(\.\d+)?([eE][-+]?\d+)?`
    fn number(&self, i: usize) -> Result<usize, JsonErr> {
        let digit = |j: usize| matches!(self.at(j), Some('0'..='9'));
        let mut j = i;
        if self.at(j) == Some('-') {
            j += 1;
        }
        match self.at(j) {
            Some('0') => j += 1,
            Some('1'..='9') => {
                while digit(j) {
                    j += 1;
                }
            }
            _ => return Err(("Expecting value", i)),
        }
        if self.at(j) == Some('.') && digit(j + 1) {
            j += 1;
            while digit(j) {
                j += 1;
            }
        }
        if matches!(self.at(j), Some('e' | 'E')) {
            let mut k = j + 1;
            if matches!(self.at(k), Some('+' | '-')) {
                k += 1;
            }
            if digit(k) {
                while digit(k) {
                    k += 1;
                }
                j = k;
            }
        }
        Ok(j)
    }

    /// `begin` is the opening quote; strict mode rejects raw control characters
    fn string(&self, begin: usize) -> Result<usize, JsonErr> {
        let unterminated = ("Unterminated string starting at", begin);
        let mut j = begin + 1;
        loop {
            match self.at(j) {
                None => return Err(unterminated),
                Some('"') => return Ok(j + 1),
                Some('\\') => match self.at(j + 1) {
                    None => return Err(unterminated),
                    Some('u') => {
                        if !(0..4).all(|k| matches!(self.at(j + 2 + k), Some(h) if h.is_ascii_hexdigit())) {
                            return Err(("Invalid \\uXXXX escape", j + 1));
                        }
                        j += 6;
                    }
                    Some('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') => j += 2,
                    Some(_) => return Err(("Invalid \\escape", j)),
                },
                Some(ch) if (ch as u32) < 0x20 => return Err(("Invalid control character at", j)),
                Some(_) => j += 1,
            }
        }
    }

    fn object(&self, i: usize) -> Result<usize, JsonErr> {
        let mut j = self.ws(i + 1);
        if self.at(j) == Some('}') {
            return Ok(j + 1);
        }
        loop {
            if self.at(j) != Some('"') {
                return Err(("Expecting property name enclosed in double quotes", j));
            }
            j = self.ws(self.string(j)?);
            if self.at(j) != Some(':') {
                return Err(("Expecting ':' delimiter", j));
            }
            j = self.value(self.ws(j + 1))?;
            j = self.ws(j);
            match self.at(j) {
                Some('}') => return Ok(j + 1),
                Some(',') => j = self.ws(j + 1),
                _ => return Err(("Expecting ',' delimiter", j)),
            }
        }
    }

    fn array(&self, i: usize) -> Result<usize, JsonErr> {
        let mut j = self.ws(i + 1);
        if self.at(j) == Some(']') {
            return Ok(j + 1);
        }
        loop {
            j = self.ws(self.value(j)?);
            match self.at(j) {
                Some(']') => return Ok(j + 1),
                Some(',') => j = self.ws(j + 1),
                _ => return Err(("Expecting ',' delimiter", j)),
            }
        }
    }
}

/// A value as python's `json.loads` returns it. Integers keep every digit, floats are python floats.
#[derive(Debug, Clone, PartialEq)]
pub enum PyJson {
    Null,
    Bool(bool),
    /// python's `str(int)` of the parsed integer
    Int(String),
    Float(f64),
    Str(String),
    List(Vec<PyJson>),
    /// insertion-ordered dict; a repeated key keeps its first position and takes the last value
    Dict(Vec<(String, PyJson)>),
}

/// python 3.14's `sys.get_int_max_str_digits()` default: a longer integer literal raises ValueError
const INT_MAX_STR_DIGITS: usize = 4300;
/// about python's recursion limit: deeper nesting raises RecursionError
const MAX_DEPTH: usize = 1000;

/// python `json.loads(s)` (strict). `None` where python raises. Lone surrogate escapes are also `None`
/// (python keeps them, which a Rust string cannot hold).
pub fn json_loads(s: &str) -> Option<PyJson> {
    if s.starts_with('\u{feff}') {
        return None; // "Unexpected UTF-8 BOM"
    }
    let mut p = Parser { b: s.as_bytes(), i: 0 };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.i != p.b.len() {
        return None; // "Extra data"
    }
    Some(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn lit(&mut self, word: &[u8]) -> bool {
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Option<PyJson> {
        match *self.b.get(self.i)? {
            b'"' => {
                self.i += 1;
                self.string().map(PyJson::Str)
            }
            b'{' => self.object(depth + 1),
            b'[' => self.array(depth + 1),
            b'n' if self.lit(b"null") => Some(PyJson::Null),
            b't' if self.lit(b"true") => Some(PyJson::Bool(true)),
            b'f' if self.lit(b"false") => Some(PyJson::Bool(false)),
            b'N' if self.lit(b"NaN") => Some(PyJson::Float(f64::NAN)),
            b'I' if self.lit(b"Infinity") => Some(PyJson::Float(f64::INFINITY)),
            b'-' if self.lit(b"-Infinity") => Some(PyJson::Float(f64::NEG_INFINITY)),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    /// `(-?(?:0|[1-9]\d*))(\.\d+)?([eE][-+]?\d+)?`; a float when a fraction or exponent is present
    fn number(&mut self) -> Option<PyJson> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        match self.b.get(self.i)? {
            b'0' => self.i += 1,
            b'1'..=b'9' => self.skip_digits(),
            _ => {
                self.i = start;
                return None;
            }
        }
        let mut is_float = false;
        if self.b.get(self.i) == Some(&b'.') && self.b.get(self.i + 1).is_some_and(u8::is_ascii_digit) {
            is_float = true;
            self.i += 1;
            self.skip_digits();
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            let mut j = self.i + 1;
            if matches!(self.b.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if self.b.get(j).is_some_and(u8::is_ascii_digit) {
                is_float = true;
                self.i = j;
                self.skip_digits();
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        if is_float {
            return text.parse::<f64>().ok().map(PyJson::Float);
        }
        if text.trim_start_matches('-').len() > INT_MAX_STR_DIGITS {
            return None;
        }
        // int("-0") is 0
        Some(PyJson::Int(if text == "-0" { "0".to_string() } else { text.to_string() }))
    }

    fn skip_digits(&mut self) {
        while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
            self.i += 1;
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let h = self.b.get(self.i..self.i + 4)?;
        if !h.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        self.i += 4;
        u32::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()
    }

    /// the body of a string, after its opening quote, through the closing quote (strict: no raw control
    /// characters)
    fn string(&mut self) -> Option<String> {
        let mut out = String::new();
        let mut run = self.i;
        loop {
            match *self.b.get(self.i)? {
                b'"' => {
                    out.push_str(std::str::from_utf8(&self.b[run..self.i]).ok()?);
                    self.i += 1;
                    return Some(out);
                }
                b'\\' => {
                    out.push_str(std::str::from_utf8(&self.b[run..self.i]).ok()?);
                    let e = *self.b.get(self.i + 1)?;
                    self.i += 2;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) && self.b[self.i..].starts_with(b"\\u") {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    self.i = save;
                                    return None;
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp)?);
                        }
                        _ => return None,
                    }
                    run = self.i;
                }
                0..=0x1f => return None,
                _ => self.i += 1,
            }
        }
    }

    fn object(&mut self, depth: usize) -> Option<PyJson> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.i += 1;
        let mut entries: Vec<(String, PyJson)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        self.skip_ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Some(PyJson::Dict(entries));
        }
        loop {
            self.skip_ws();
            if self.b.get(self.i) != Some(&b'"') {
                return None;
            }
            self.i += 1;
            let key = self.string()?;
            self.skip_ws();
            if self.b.get(self.i) != Some(&b':') {
                return None;
            }
            self.i += 1;
            self.skip_ws();
            let v = self.value(depth)?;
            match index.get(&key) {
                Some(&at) => entries[at].1 = v,
                None => {
                    index.insert(key.clone(), entries.len());
                    entries.push((key, v));
                }
            }
            self.skip_ws();
            match *self.b.get(self.i)? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Some(PyJson::Dict(entries));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<PyJson> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.i += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Some(PyJson::List(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth)?);
            self.skip_ws();
            match *self.b.get(self.i)? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(PyJson::List(items));
                }
                _ => return None,
            }
        }
    }
}

/// `json.dumps(v, ensure_ascii=False)`
pub fn json_dumps(v: &PyJson) -> String {
    let mut out = String::new();
    dump(v, &mut out);
    out
}

fn dump(v: &PyJson, out: &mut String) {
    match v {
        PyJson::Null => out.push_str("null"),
        PyJson::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        PyJson::Int(s) => out.push_str(s),
        PyJson::Float(f) => out.push_str(&float_repr(*f)),
        PyJson::Str(s) => dump_str(s, out),
        PyJson::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump(item, out);
            }
            out.push(']');
        }
        PyJson::Dict(entries) => {
            out.push('{');
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump_str(k, out);
                out.push_str(": ");
                dump(val, out);
            }
            out.push('}');
        }
    }
}

/// python's ESCAPE set (`[\x00-\x1f\\"\b\f\n\r\t]`) with ensure_ascii off
fn dump_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `_sanitize_json_recursive`: string values are sanitized, keys and other values are kept
pub fn sanitize_json(v: &PyJson) -> PyJson {
    match v {
        PyJson::Str(s) => PyJson::Str(ContentSanitizer::sanitize(s)),
        PyJson::Dict(entries) => PyJson::Dict(entries.iter().map(|(k, val)| (k.clone(), sanitize_json(val))).collect()),
        PyJson::List(items) => PyJson::List(items.iter().map(sanitize_json).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(s: &str) -> Option<String> {
        json_loads(s).map(|v| json_dumps(&v))
    }

    #[test]
    fn json_keeps_python_literals_and_integers() {
        assert_eq!(roundtrip("[NaN, Infinity, -Infinity]").unwrap(), "[NaN, Infinity, -Infinity]");
        assert_eq!(roundtrip("18446744073709551616").unwrap(), "18446744073709551616");
        assert_eq!(roundtrip("[-0, 1e400, 1.0, 2.5e-3]").unwrap(), "[0, Infinity, 1.0, 0.0025]");
        assert_eq!(roundtrip("{\"a\": 1, \"a\": 2, \"b\": null}").unwrap(), "{\"a\": 2, \"b\": null}");
    }

    #[test]
    fn json_rejects_what_python_rejects() {
        for bad in ["", "01", "1.", "[1,]", "{\"a\" 1}", "\"a\u{1}\"", "\"\\x\"", "\"\\ud800\"", "\u{feff}1", "1 2", "[1"] {
            assert!(json_loads(bad).is_none(), "{:?} should not parse", bad);
        }
        assert_eq!(roundtrip("\"\\ud83d\\ude00\\n\\u00e9\""), Some("\"\u{1f600}\\n\u{e9}\"".to_string()));
    }

    #[test]
    fn py_json_loads_reports_cpython_decode_error_text() {
        let msg = |t: &str| py_json_loads(t).unwrap_err();
        assert_eq!(msg(""), "Expecting value: line 1 column 1 (char 0)");
        assert_eq!(msg("{\"a\" 1}"), "Expecting ':' delimiter: line 1 column 6 (char 5)");
        assert_eq!(msg("[1 2]"), "Expecting ',' delimiter: line 1 column 4 (char 3)");
        assert_eq!(msg("[1,]"), "Expecting value: line 1 column 4 (char 3)");
        assert_eq!(msg("{\"a\":1} x"), "Extra data: line 1 column 9 (char 8)");
        assert_eq!(msg("\"abc"), "Unterminated string starting at: line 1 column 1 (char 0)");
        assert_eq!(msg("{\n\"a\" 1}"), "Expecting ':' delimiter: line 2 column 5 (char 6)");
        assert_eq!(msg("{1:2}"), "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)");
        assert_eq!(py_json_loads("[1, {\"k\": null}]").unwrap()[1]["k"], serde_json::Value::Null);
    }

    #[test]
    fn json_dumps_escapes_like_python_without_ensure_ascii() {
        let v = json_loads("{\"k\\u0001\": \"q\\\"\\\\\\t\\u007f\u{e9}\"}").unwrap();
        assert_eq!(json_dumps(&v), "{\"k\\u0001\": \"q\\\"\\\\\\t\u{7f}\u{e9}\"}");
    }

    #[test]
    fn urljoin_is_python_urljoin() {
        assert_eq!(urljoin("https://a.com/b/c/", "../../../d?q#f").unwrap(), "https://a.com/d?q#f");
        assert_eq!(urljoin("http://a.com/x", "//b.com/y").unwrap(), "http://b.com/y");
    }
}

// ==================== python os / pathlib / shutil helpers (file_manager) ====================

/// python `str(OSError)` for an error on one path: `[Errno N] strerror: 'path'`
pub fn os_err<P: AsRef<OsStr>>(e: &std::io::Error, path: P) -> String {
    match e.raw_os_error() {
        Some(code) => format!("[Errno {}] {}: {}", code, strerror(code), py_repr_os(path)),
        None => e.to_string(),
    }
}

/// python `str(OSError)` for a two-path error: `[Errno N] strerror: 'src' -> 'dst'`
pub fn os_err2<P: AsRef<OsStr>, Q: AsRef<OsStr>>(e: &std::io::Error, src: P, dst: Q) -> String {
    match e.raw_os_error() {
        Some(code) => format!("[Errno {}] {}: {} -> {}", code, strerror(code), py_repr_os(src), py_repr_os(dst)),
        None => e.to_string(),
    }
}

/// one unit of a path as python sees it after `os.fsdecode`: a decoded character, or a byte that
/// did not decode and became a `surrogateescape` lone surrogate (U+DC80..U+DCFF)
enum OsPiece {
    Char(char),
    Escaped(u8),
}

fn os_pieces(mut bytes: &[u8]) -> Vec<OsPiece> {
    let mut out = Vec::new();
    loop {
        match std::str::from_utf8(bytes) {
            Ok(s) => {
                out.extend(s.chars().map(OsPiece::Char));
                return out;
            }
            Err(e) => {
                let (good, rest) = bytes.split_at(e.valid_up_to());
                out.extend(std::str::from_utf8(good).unwrap_or("").chars().map(OsPiece::Char));
                let bad = e.error_len().unwrap_or(rest.len());
                out.extend(rest[..bad].iter().map(|b| OsPiece::Escaped(*b)));
                bytes = &rest[bad..];
            }
        }
    }
}

/// python `repr()` of `os.fsdecode(path)`: undecodable bytes show as `\udcXX`, as CPython's repr does
pub fn py_repr_os<P: AsRef<OsStr>>(path: P) -> String {
    let path = path.as_ref();
    if let Some(s) = path.to_str() {
        return py_quote(s);
    }
    let pieces = os_pieces(path.as_bytes());
    let has_sq = pieces.iter().any(|p| matches!(p, OsPiece::Char('\'')));
    let has_dq = pieces.iter().any(|p| matches!(p, OsPiece::Char('"')));
    let quote = if has_sq && !has_dq { '"' } else { '\'' };
    let mut out = String::new();
    out.push(quote);
    for piece in pieces {
        match piece {
            OsPiece::Escaped(b) => out.push_str(&format!("\\udc{:02x}", b)),
            OsPiece::Char('\\') => out.push_str("\\\\"),
            OsPiece::Char('\t') => out.push_str("\\t"),
            OsPiece::Char('\n') => out.push_str("\\n"),
            OsPiece::Char('\r') => out.push_str("\\r"),
            OsPiece::Char(c) if c == quote => {
                out.push('\\');
                out.push(c);
            }
            OsPiece::Char(c) if c.is_ascii() && (c as u32) >= 0x20 && c as u32 != 0x7f => out.push(c),
            OsPiece::Char(c) if c.is_ascii() => out.push_str(&format!("\\x{:02x}", c as u32)),
            OsPiece::Char(c) => {
                let q = py_quote(&c.to_string());
                out.push_str(&q[1..q.len() - 1]);
            }
        }
    }
    out.push(quote);
    out
}

/// python `str()` of `os.fsdecode(path)` without quotes (a plain message such as a SpecialFileError)
pub fn display_os<P: AsRef<OsStr>>(path: P) -> String {
    let path = path.as_ref();
    if let Some(s) = path.to_str() {
        return s.to_string();
    }
    os_pieces(path.as_bytes())
        .into_iter()
        .map(|p| match p {
            OsPiece::Char(c) => c.to_string(),
            OsPiece::Escaped(b) => format!("\\udc{:02x}", b),
        })
        .collect()
}

/// python `os.path.basename(path.rstrip('/'))` on the raw bytes of a path
pub fn py_basename_os(path: &OsStr) -> OsString {
    let b = path.as_bytes();
    let mut end = b.len();
    while end > 0 && b[end - 1] == b'/' {
        end -= 1;
    }
    let trimmed = &b[..end];
    let start = trimmed.iter().rposition(|c| *c == b'/').map_or(0, |i| i + 1);
    OsStr::from_bytes(&trimmed[start..]).to_os_string()
}

/// python `str(OSError)` for an error without a filename: `[Errno N] strerror`
pub fn os_err_bare(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => format!("[Errno {}] {}", code, strerror(code)),
        None => e.to_string(),
    }
}

/// libc strerror text (what python puts into `OSError.strerror`)
pub fn strerror(code: i32) -> String {
    let s = std::io::Error::from_raw_os_error(code).to_string();
    match s.rfind(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// python repr() of a str
pub fn py_quote(s: &str) -> String {
    ol_core::pyjson::py_repr(&serde_json::Value::String(s.to_string()))
}

/// python type name used in TypeError messages ("int", "NoneType", "list", ...)
pub fn py_type_name(v: &serde_json::Value) -> &'static str {
    use serde_json::Value;
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

/// python `int(v)` for a tool argument or setting. A str is parsed like `int(str)`: surrounding whitespace,
/// an optional sign, and digits with single underscores between them. Other values use the numeric
/// conversion. Past i64 the value saturates, since python keeps the big int and callers only clamp it.
pub fn py_int(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::String(s) => py_int_str(s),
        other => ol_core::pyjson::as_i64(other),
    }
}

fn py_int_str(s: &str) -> Option<i64> {
    let t = s.trim();
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    // underscores only sit between two digits: no leading, trailing or doubled underscore
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    const LIMIT: i128 = i64::MAX as i128 + 1;
    let mut value: i128 = 0;
    for c in digits.chars().filter(|c| *c != '_') {
        // ASCII digits only; python also accepts other unicode decimal digits
        let d = c.to_digit(10)? as i128;
        value = (value * 10 + d).min(LIMIT);
    }
    let signed = if negative { -value } else { value };
    Some(signed.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

/// python `str(pathlib.Path(s))` on posix: repeated slashes, trailing slashes and `.` components
/// are dropped, a leading `//` is kept, and an empty path is `.`
pub fn py_path(s: &str) -> String {
    let abs = s.starts_with('/');
    let lead = if abs && s.starts_with("//") && !s.starts_with("///") {
        "//"
    } else if abs {
        "/"
    } else {
        ""
    };
    let body = s.split('/').filter(|c| !c.is_empty() && *c != ".").collect::<Vec<_>>().join("/");
    if abs {
        format!("{}{}", lead, body)
    } else if body.is_empty() {
        ".".to_string()
    } else {
        body
    }
}

/// python `str(Path(s).parent)`
pub fn py_parent(s: &str) -> String {
    let p = py_path(s);
    if p == "." || p == "/" || p == "//" {
        return p;
    }
    match p.rfind('/') {
        None => ".".to_string(),
        Some(i) => {
            let par = &p[..i];
            if par.is_empty() {
                "/".to_string()
            } else if par == "/" && p.starts_with("//") {
                "//".to_string()
            } else {
                par.to_string()
            }
        }
    }
}

/// python `Path(s).name`
pub fn py_name(s: &str) -> String {
    let p = py_path(s);
    if p == "." {
        return String::new();
    }
    p.rsplit('/').next().unwrap_or("").to_string()
}

/// python `Path(s).absolute()` as a string (no symlink resolution, no `..` normalisation)
pub fn py_absolute(s: &str) -> String {
    let p = py_path(s);
    if p.starts_with('/') {
        return p;
    }
    let cwd = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
    if p == "." {
        return cwd;
    }
    py_join(&cwd, &p)
}

/// python `os.path.join(a, b)` for a relative `b`
pub fn py_join(a: &str, b: &str) -> String {
    if a.ends_with('/') {
        format!("{}{}", a, b)
    } else {
        format!("{}/{}", a, b)
    }
}

/// python `posixpath.normpath`
pub fn py_normpath(s: &str) -> String {
    let (lead, rest) = if s.starts_with("//") && !s.starts_with("///") {
        ("//", &s[2..])
    } else if s.starts_with('/') {
        ("/", &s[1..])
    } else {
        ("", s)
    };
    let mut stack: Vec<&str> = Vec::new();
    for comp in rest.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if matches!(stack.last(), Some(l) if *l != "..") {
                    stack.pop();
                } else if lead.is_empty() {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }
    let body = stack.join("/");
    if lead.is_empty() {
        if body.is_empty() {
            ".".to_string()
        } else {
            body
        }
    } else {
        format!("{}{}", lead, body)
    }
}

/// python `os.path.abspath`
pub fn py_abspath(s: &str) -> String {
    if s.starts_with('/') {
        return py_normpath(s);
    }
    let cwd = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
    py_normpath(&py_join(&cwd, s))
}

/// python `os.path.samefile` (False when either stat fails)
pub fn same_file<P: AsRef<Path>, Q: AsRef<Path>>(a: P, b: Q) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// python text-mode newline handling on read: `\r\n` and lone `\r` become `\n`
pub fn universal_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}
