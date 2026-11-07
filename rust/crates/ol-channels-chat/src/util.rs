//! small python-semantics helpers shared by the chat channels

/// python `s[:n]` (code points)
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// python `len(s)` (code points)
pub fn clen(s: &str) -> usize {
    s.chars().count()
}

/// python `s[a:b]` (code points, clamped)
pub fn slice(s: &str, a: usize, b: usize) -> String {
    s.chars().skip(a).take(b.saturating_sub(a)).collect()
}

/// python `s.strip(chars)`: strip any of `chars` from both ends
pub fn strip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    if chars.is_empty() {
        return s;
    }
    s.trim_matches(|c: char| chars.contains(c))
}

/// python `s.lstrip(chars)`
pub fn lstrip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    if chars.is_empty() {
        return s;
    }
    s.trim_start_matches(|c: char| chars.contains(c))
}

/// python `[s[i:i+n] for i in range(0, len(s), n)]` (the discord fixed-size chunking)
pub fn fixed_chunks(s: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars.chunks(n.max(1)).map(|c| c.iter().collect()).collect()
}

/// the configured command prefix (core.cmd_prefix, default "/")
pub fn cmd_prefix() -> String {
    ol_core::config::get_str(&["core", "cmd_prefix"]).unwrap_or_else(|| "/".into())
}

/// python `str(value)` for a config value that may be None
pub fn cfg_string(v: Option<serde_json::Value>) -> Option<String> {
    match v {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(ol_core::pyjson::py_str(&v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slicing() {
        assert_eq!(head("héllo", 2), "hé");
        assert_eq!(slice("abcdef", 2, 4), "cd");
        assert_eq!(slice("abc", 2, 10), "c");
        assert_eq!(fixed_chunks("abcde", 2), vec!["ab", "cd", "e"]);
        assert!(fixed_chunks("", 2).is_empty());
        assert_eq!(strip_chars("//bob/", "/"), "bob");
        assert_eq!(lstrip_chars("//bob/", "/"), "bob/");
    }
}
