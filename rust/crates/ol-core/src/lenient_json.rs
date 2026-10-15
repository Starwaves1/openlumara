//! Lenient JSON parsing, replacing the Python `json_repair` and `partial_json_parser`
//! packages for the uses OpenLumara has for them:
//! - repairing tool-call arguments emitted by models (trailing commas, single quotes,
//!   python literals, unquoted keys, missing closing brackets, code fences)
//! - parsing incomplete JSON while tool-call arguments are still streaming in
//!   (`partial_json_parser.loads(..., Allow.ALL)`)

use serde_json::{Map, Number, Value};

struct Parser<'a> {
    s: &'a [char],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }
    fn eof(&self) -> bool {
        self.i >= self.s.len()
    }
    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => self.i += 1,
                Some('/') if self.s.get(self.i + 1) == Some(&'/') => {
                    while let Some(c) = self.peek() {
                        self.i += 1;
                        if c == '\n' {
                            break;
                        }
                    }
                }
                Some('/') if self.s.get(self.i + 1) == Some(&'*') => {
                    self.i += 2;
                    while !self.eof() {
                        if self.peek() == Some('*') && self.s.get(self.i + 1) == Some(&'/') {
                            self.i += 2;
                            break;
                        }
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn value(&mut self) -> Option<Value> {
        self.skip_ws();
        match self.peek()? {
            '{' => Some(self.object()),
            '[' => Some(self.array()),
            '"' | '\'' => Some(Value::String(self.string())),
            c if c == '-' || c.is_ascii_digit() => self.number(),
            _ => self.literal_or_bare(),
        }
    }

    fn object(&mut self) -> Value {
        self.i += 1; // {
        let mut map = Map::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => break,
                Some('}') => {
                    self.i += 1;
                    break;
                }
                Some(',') => {
                    self.i += 1;
                    continue;
                }
                _ => {}
            }
            let key = match self.peek() {
                Some('"') | Some('\'') => {
                    let start = self.i;
                    let k = self.string();
                    // an unterminated key at EOF is dropped
                    if self.eof() && !self.closed_string_at(start) {
                        break;
                    }
                    k
                }
                _ => {
                    let mut k = String::new();
                    while let Some(c) = self.peek() {
                        if c == ':' || c == '}' || c == ',' || c.is_whitespace() {
                            break;
                        }
                        k.push(c);
                        self.i += 1;
                    }
                    if k.is_empty() {
                        // garbage char, skip it
                        self.i += 1;
                        continue;
                    }
                    k
                }
            };
            self.skip_ws();
            if self.peek() == Some(':') {
                self.i += 1;
            } else if self.eof() {
                break;
            }
            self.skip_ws();
            if self.eof() {
                break;
            }
            if matches!(self.peek(), Some('}') | Some(',')) {
                map.insert(key, Value::String(String::new()));
                continue;
            }
            match self.value() {
                Some(v) => {
                    map.insert(key, v);
                }
                None => break,
            }
        }
        Value::Object(map)
    }

    fn closed_string_at(&self, start: usize) -> bool {
        // true if the string starting at `start` was terminated before the current position
        let quote = self.s[start];
        let mut j = start + 1;
        while j < self.s.len() {
            if self.s[j] == '\\' {
                j += 2;
                continue;
            }
            if self.s[j] == quote {
                return true;
            }
            j += 1;
        }
        false
    }

    fn array(&mut self) -> Value {
        self.i += 1; // [
        let mut arr = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => break,
                Some(']') => {
                    self.i += 1;
                    break;
                }
                Some(',') => {
                    self.i += 1;
                    continue;
                }
                _ => {}
            }
            let before = self.i;
            match self.value() {
                Some(v) => arr.push(v),
                None => break,
            }
            if self.i == before {
                self.i += 1;
            }
        }
        Value::Array(arr)
    }

    fn string(&mut self) -> String {
        let quote = self.peek().unwrap();
        self.i += 1;
        let mut out = String::new();
        while let Some(c) = self.peek() {
            self.i += 1;
            if c == quote {
                return out;
            }
            if c == '\\' {
                let Some(e) = self.peek() else { break };
                self.i += 1;
                match e {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{08}'),
                    'f' => out.push('\u{0c}'),
                    '/' => out.push('/'),
                    'u' => {
                        let hex: String = self.s[self.i..(self.i + 4).min(self.s.len())].iter().collect();
                        if hex.len() == 4 {
                            if let Ok(code) = u32::from_str_radix(&hex, 16) {
                                self.i += 4;
                                // surrogate pair handling
                                if (0xD800..0xDC00).contains(&code)
                                    && self.s.get(self.i) == Some(&'\\')
                                    && self.s.get(self.i + 1) == Some(&'u')
                                {
                                    let lo: String =
                                        self.s[self.i + 2..(self.i + 6).min(self.s.len())].iter().collect();
                                    if let Ok(lo) = u32::from_str_radix(&lo, 16) {
                                        if (0xDC00..0xE000).contains(&lo) {
                                            self.i += 6;
                                            let c = 0x10000 + ((code - 0xD800) << 10) + (lo - 0xDC00);
                                            if let Some(ch) = char::from_u32(c) {
                                                out.push(ch);
                                            }
                                            continue;
                                        }
                                    }
                                }
                                if let Some(ch) = char::from_u32(code) {
                                    out.push(ch);
                                }
                                continue;
                            }
                        }
                        // incomplete escape at EOF: stop
                        self.i = self.s.len();
                        break;
                    }
                    other => out.push(other),
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E') {
                self.i += 1;
            } else {
                break;
            }
        }
        let mut text: String = self.s[start..self.i].iter().collect();
        // partial numbers like "1." or "1e" or "-": trim the dangling part
        while !text.is_empty() && !text.ends_with(|c: char| c.is_ascii_digit()) {
            text.pop();
        }
        if text.is_empty() || text == "-" {
            return Some(Value::Null);
        }
        if let Ok(i) = text.parse::<i64>() {
            return Some(Value::Number(i.into()));
        }
        if let Ok(f) = text.parse::<f64>() {
            return Number::from_f64(f).map(Value::Number);
        }
        Some(Value::String(text))
    }

    fn literal_or_bare(&mut self) -> Option<Value> {
        let rest: String = self.s[self.i..].iter().take(5).collect();
        for (lit, val) in [
            ("true", Value::Bool(true)),
            ("True", Value::Bool(true)),
            ("false", Value::Bool(false)),
            ("False", Value::Bool(false)),
            ("null", Value::Null),
            ("None", Value::Null),
        ] {
            if rest.starts_with(lit) {
                self.i += lit.len();
                return Some(val);
            }
            // partial literal at EOF ("tr", "nul")
            if self.s.len() - self.i < lit.len() && lit.starts_with(rest.as_str()) && !rest.is_empty() {
                self.i = self.s.len();
                return Some(val);
            }
        }
        // bare word: read until a structural char
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if matches!(c, ',' | '}' | ']' | '\n') {
                break;
            }
            out.push(c);
            self.i += 1;
        }
        let out = out.trim().to_string();
        if out.is_empty() {
            return None;
        }
        Some(Value::String(out))
    }
}

fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.trim_start_matches(|c: char| c.is_alphanumeric());
        let rest = rest.strip_suffix("```").unwrap_or(rest);
        return rest.trim();
    }
    t
}

/// json_repair.loads(): parse JSON, repairing common mistakes. Returns `Value::String("")`
/// for empty/unparseable input (mirroring json_repair's empty-string return).
pub fn repair_loads(s: &str) -> Value {
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        return v;
    }
    let t = strip_fences(s);
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return v;
    }
    let chars: Vec<char> = t.chars().collect();
    // json_repair skips leading garbage until the first { or [
    let start = chars.iter().position(|c| *c == '{' || *c == '[');
    let mut p = Parser { s: &chars, i: start.unwrap_or(0) };
    match p.value() {
        Some(v) => v,
        None => Value::String(String::new()),
    }
}

/// partial_json_parser.loads(s, allow_partial=Allow.ALL)
pub fn partial_loads(s: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        return Some(v);
    }
    let chars: Vec<char> = s.chars().collect();
    let mut p = Parser { s: &chars, i: 0 };
    p.value()
}

/// parse a (possibly incomplete) JSON object string into a plain dict.
/// never fails: falls back to {'_raw': text} for non-JSON input. (core/turns.py parse_lenient_json)
pub fn parse_lenient_json(raw: Option<&Value>) -> Map<String, Value> {
    let Some(Value::String(raw)) = raw else { return Map::new() };
    if raw.trim().is_empty() {
        return Map::new();
    }
    let trimmed = raw.trim_start();
    if trimmed.starts_with('{') {
        if let Some(Value::Object(m)) = partial_loads(raw) {
            return m;
        }
    }
    let mut m = Map::new();
    m.insert("_raw".into(), Value::String(raw.clone()));
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repairs() {
        assert_eq!(repair_loads(r#"{"a": 1,}"#), json!({"a": 1}));
        assert_eq!(repair_loads(r#"{'a': 'b'}"#), json!({"a": "b"}));
        assert_eq!(repair_loads(r#"{"a": True, "b": None}"#), json!({"a": true, "b": null}));
        assert_eq!(repair_loads(r#"{"a": [1, 2"#), json!({"a": [1, 2]}));
        assert_eq!(repair_loads("```json\n{\"a\": 1}\n```"), json!({"a": 1}));
        assert_eq!(repair_loads(""), json!(""));
    }

    #[test]
    fn partial() {
        assert_eq!(partial_loads(r#"{"path": "/tmp/fo"#), Some(json!({"path": "/tmp/fo"})));
        assert_eq!(partial_loads(r#"{"a": 1, "b"#), Some(json!({"a": 1})));
        assert_eq!(partial_loads(r#"{"a": 1, "b": "#), Some(json!({"a": 1})));
        assert_eq!(partial_loads(r#"{"a": [1, {"x": tr"#), Some(json!({"a": [1, {"x": true}]})));
        assert_eq!(partial_loads(r#"{"n": 12"#), Some(json!({"n": 12})));
    }

    #[test]
    fn lenient() {
        assert_eq!(parse_lenient_json(Some(&json!("not json"))), json!({"_raw": "not json"}).as_object().unwrap().clone());
        assert!(parse_lenient_json(Some(&json!(""))).is_empty());
        assert_eq!(parse_lenient_json(Some(&json!("{\"q\": \"x"))), json!({"q": "x"}).as_object().unwrap().clone());
    }
}
