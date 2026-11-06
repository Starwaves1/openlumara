//! Python-compatible helpers for the http module: `urllib.parse.urljoin`, and `json.loads` /
//! `json.dumps(ensure_ascii=False)` with python's results (NaN and Infinity literals, integers of any
//! size kept exact, python's float repr).

use crate::pytext::urlsplit;
use crate::sanitize::ContentSanitizer;
use ol_core::pyjson::float_repr;
use std::collections::HashMap;

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
