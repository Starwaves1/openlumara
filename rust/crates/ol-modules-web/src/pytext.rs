//! Python stdlib text helpers whose exact behaviour the web modules depend on:
//! `html.unescape`, `urllib.parse.unquote` / `quote_plus` / `urlsplit`, and
//! python's notion of whitespace (`str.split()` / `str.strip()`).

use crate::entities;
use once_cell::sync::Lazy;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// whitespace
// ---------------------------------------------------------------------------

/// python `str.isspace()` for a single char
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// python `str.split()` (no arguments)
pub fn py_split(s: &str) -> Vec<&str> {
    s.split(py_isspace).filter(|p| !p.is_empty()).collect()
}

/// python `str.strip()` (no arguments)
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// python `len(str)` (code points)
pub fn py_len(s: &str) -> usize {
    s.chars().count()
}

/// python `s[:n]` on code points
pub fn py_prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

// ---------------------------------------------------------------------------
// html.unescape
// ---------------------------------------------------------------------------

static HTML5: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| entities::HTML5.iter().copied().collect());
static INVALID_CHARREFS: Lazy<HashMap<u32, &'static str>> = Lazy::new(|| entities::INVALID_CHARREFS.iter().copied().collect());
static INVALID_CODEPOINTS: Lazy<HashSet<u32>> = Lazy::new(|| entities::INVALID_CODEPOINTS.iter().copied().collect());
static CHARREF: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"&(#[0-9]+;?|#[xX][0-9a-fA-F]+;?|[^\t\n\x0c <&#;]{1,32};?)").unwrap());

fn replace_charref(s: &str) -> String {
    if let Some(num) = s.strip_prefix('#') {
        let (digits, radix) = match num.strip_prefix(['x', 'X']) {
            Some(h) => (h, 16),
            None => (num, 10),
        };
        let digits = digits.trim_end_matches(';');
        // python ints are unbounded: anything that doesn't fit is > 0x10FFFF
        let n = u64::from_str_radix(digits, radix).unwrap_or(u64::MAX);
        if n <= u32::MAX as u64 {
            if let Some(r) = INVALID_CHARREFS.get(&(n as u32)) {
                return r.to_string();
            }
        }
        if (0xD800..=0xDFFF).contains(&n) || n > 0x10FFFF {
            return "\u{FFFD}".into();
        }
        if INVALID_CODEPOINTS.contains(&(n as u32)) {
            return String::new();
        }
        return char::from_u32(n as u32).map(|c| c.to_string()).unwrap_or_else(|| "\u{FFFD}".into());
    }
    if let Some(v) = HTML5.get(s) {
        return v.to_string();
    }
    // find the longest matching name (as defined by the standard)
    let idx: Vec<usize> = s.char_indices().map(|(i, _)| i).chain(std::iter::once(s.len())).collect();
    // python: for x in range(len(s)-1, 1, -1): s[:x]
    let n = idx.len() - 1;
    let mut x = n.saturating_sub(1);
    while x > 1 {
        let (a, b) = s.split_at(idx[x]);
        if let Some(v) = HTML5.get(a) {
            return format!("{}{}", v, b);
        }
        x -= 1;
    }
    format!("&{}", s)
}

/// python `html.unescape`
pub fn html_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    CHARREF.replace_all(s, |c: &regex::Captures| replace_charref(&c[1])).into_owned()
}

// ---------------------------------------------------------------------------
// urllib.parse.unquote / quote_plus / urlencode
// ---------------------------------------------------------------------------

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn unquote_to_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hexval(b[i + 1]), hexval(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// python `urllib.parse.unquote(s)` (utf-8, errors='replace')
pub fn url_unquote(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    // python splits into ascii / non-ascii runs and only decodes the ascii runs
    let mut out = String::with_capacity(s.len());
    let mut run_start = 0;
    let mut in_ascii = true;
    let flush = |out: &mut String, part: &str, ascii: bool| {
        if part.is_empty() {
            return;
        }
        if ascii {
            out.push_str(&String::from_utf8_lossy(&unquote_to_bytes(part)));
        } else {
            out.push_str(part);
        }
    };
    for (i, c) in s.char_indices() {
        let a = c.is_ascii();
        if a != in_ascii {
            flush(&mut out, &s[run_start..i], in_ascii);
            run_start = i;
            in_ascii = a;
        }
    }
    flush(&mut out, &s[run_start..], in_ascii);
    out
}

/// python `urllib.parse.quote(s, safe)`
pub fn quote(s: &str, safe: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let c = b as char;
        if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) || (b.is_ascii() && safe.contains(c)) {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// python `urllib.parse.quote_plus(s)`
pub fn quote_plus(s: &str) -> String {
    if s.contains(' ') {
        quote(s, " ").replace(' ', "+")
    } else {
        quote(s, "")
    }
}

/// python `urllib.parse.urlencode(pairs)`
pub fn urlencode(pairs: &[(String, String)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v))).collect::<Vec<_>>().join("&")
}

// ---------------------------------------------------------------------------
// urllib.parse.urlsplit
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SplitResult {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
    pub fragment: String,
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'
}

/// python `ipaddress.ip_address` validity (used for bracketed hosts)
fn check_bracketed_host(host: &str) -> Result<(), String> {
    if host.starts_with('v') {
        let re = regex::Regex::new(r"(?i)\Av[a-fA-F0-9]+\..+\z").unwrap();
        if !re.is_match(host) {
            return Err("IPvFuture address is invalid".into());
        }
        return Ok(());
    }
    // python's ipaddress accepts scope ids (%zone) on IPv6
    let bare = host.split('%').next().unwrap_or("");
    if bare.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok(());
    }
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return Err("An IPv4 address cannot be in brackets".into());
    }
    Err(format!("'{}' does not appear to be an IPv4 or IPv6 address", host))
}

/// python `urllib.parse.urlsplit` (python 3.14 semantics). Errors carry the ValueError message.
pub fn urlsplit(url: &str) -> Result<SplitResult, String> {
    let url = url.trim_start_matches(|c: char| c <= ' ');
    let mut url: String = url.chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    let mut r = SplitResult::default();
    if let Some(i) = url.find(':') {
        let first = url.chars().next().unwrap();
        if i > 0 && first.is_ascii_alphabetic() && url[..i].chars().all(is_scheme_char) {
            r.scheme = url[..i].to_lowercase();
            url = url[i + 1..].to_string();
        }
    }
    if url.starts_with("//") {
        let rest = &url[2..];
        let delim = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        r.netloc = rest[..delim].to_string();
        url = rest[delim..].to_string();
        let (ob, cb) = (r.netloc.contains('['), r.netloc.contains(']'));
        if ob != cb {
            return Err("Invalid IPv6 URL".into());
        }
        if ob && cb {
            let hostport = r.netloc.rsplit_once('@').map(|(_, h)| h).unwrap_or(&r.netloc);
            let (before, bracketed) = hostport.split_once('[').unwrap_or((hostport, ""));
            if !before.is_empty() {
                return Err("Invalid IPv6 URL".into());
            }
            let (host, port) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            if !port.is_empty() && !port.starts_with(':') {
                return Err("Invalid IPv6 URL".into());
            }
            check_bracketed_host(host)?;
        }
    }
    if let Some((a, b)) = url.split_once('#') {
        r.fragment = b.to_string();
        url = a.to_string();
    }
    if let Some((a, b)) = url.split_once('?') {
        r.query = b.to_string();
        url = a.to_string();
    }
    r.path = url;
    Ok(r)
}

impl SplitResult {
    fn hostinfo(&self) -> (String, Option<String>) {
        let hostinfo = self.netloc.rsplit_once('@').map(|(_, h)| h).unwrap_or(&self.netloc);
        let (hostname, port) = match hostinfo.split_once('[') {
            Some((_, bracketed)) => {
                let (h, p) = bracketed.split_once(']').unwrap_or((bracketed, ""));
                let p = p.split_once(':').map(|(_, p)| p).unwrap_or("");
                (h, p)
            }
            None => hostinfo.split_once(':').unwrap_or((hostinfo, "")),
        };
        (hostname.to_string(), if port.is_empty() { None } else { Some(port.to_string()) })
    }

    /// python `.hostname` (lower-cased, zone id preserved)
    pub fn hostname(&self) -> Option<String> {
        let (h, _) = self.hostinfo();
        if h.is_empty() {
            return None;
        }
        Some(match h.split_once('%') {
            Some((a, z)) => format!("{}%{}", a.to_lowercase(), z),
            None => h.to_lowercase(),
        })
    }

    /// python `.port` (Err = the ValueError message)
    pub fn port(&self) -> Result<Option<u32>, String> {
        let (_, p) = self.hostinfo();
        let Some(p) = p else { return Ok(None) };
        if !p.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("Port could not be cast to integer value as {}", ol_core::pyjson::py_repr(&serde_json::Value::String(p))));
        }
        let trimmed = p.trim_start_matches('0');
        if trimmed.len() > 5 {
            return Err("Port out of range 0-65535".into());
        }
        let n: u32 = if trimmed.is_empty() { 0 } else { trimmed.parse().unwrap() };
        if n > 65535 {
            return Err("Port out of range 0-65535".into());
        }
        Ok(Some(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_matches_python() {
        assert_eq!(html_unescape("a &amp; b &lt;c&gt; &#39;x&#x27; &copy2024 &notit; &bogus;"), "a & b <c> 'x' ©2024 ¬it; &bogus;");
        assert_eq!(html_unescape("&#0; &#x80; &#xD800; &#1114112; &#11;"), "\u{FFFD} € \u{FFFD} \u{FFFD} ");
        assert_eq!(html_unescape("&amp"), "&");
    }

    #[test]
    fn unquote_matches_python() {
        assert_eq!(url_unquote("a%20b%zz%4"), "a b%zz%4");
        assert_eq!(url_unquote("%C3%A9%C3"), "é\u{FFFD}");
        assert_eq!(url_unquote("é%41"), "éA");
    }

    #[test]
    fn quote_plus_matches_python() {
        assert_eq!(quote_plus("a b/c~d*é"), "a+b%2Fc~d%2A%C3%A9");
    }

    #[test]
    fn urlsplit_matches_python() {
        let r = urlsplit("HTTPS://User@Example.COM:8080/p?q=1#f").unwrap();
        assert_eq!(r.scheme, "https");
        assert_eq!(r.netloc, "User@Example.COM:8080");
        assert_eq!(r.hostname().as_deref(), Some("example.com"));
        assert_eq!(r.port(), Ok(Some(8080)));
        assert_eq!(r.path, "/p");
        let r = urlsplit("http://[::1]:80/").unwrap();
        assert_eq!(r.hostname().as_deref(), Some("::1"));
        assert_eq!(urlsplit("http://[::1/").unwrap_err(), "Invalid IPv6 URL");
        assert_eq!(urlsplit("http://a:x/").unwrap().port().unwrap_err(), "Port could not be cast to integer value as 'x'");
        assert_eq!(urlsplit("http://a:99999/").unwrap().port().unwrap_err(), "Port out of range 0-65535");
        let r = urlsplit("/relative/path").unwrap();
        assert_eq!((r.scheme.as_str(), r.netloc.as_str()), ("", ""));
        let r = urlsplit("mailto:x@y").unwrap();
        assert_eq!((r.scheme.as_str(), r.netloc.as_str()), ("mailto", ""));
    }
}
