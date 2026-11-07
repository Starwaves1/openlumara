//! small python-semantics helpers used by the webui port

use ol_core::chat::Chat;
use ol_core::lenient_json::repair_loads;
use ol_core::pyjson::{char_len, py_str, py_str_opt, take_chars, truthy};
use ol_core::turns::group_history;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};

/// why a request body is not a value python's `json.loads` would hand the handler
#[derive(Debug, PartialEq)]
pub enum BodyJsonError {
    /// python's `JSONDecodeError` text, e.g. `Expecting value: line 1 column 1 (char 0)`
    Decode(String),
    /// python parses the body (NaN, Infinity, an over-range number, very deep nesting) but
    /// `serde_json::Value` cannot hold it
    Unrepresentable,
}

/// python's json decoder recurses; serde_json stops at 128 levels, so deeper bodies are left to serde
const MAX_JSON_DEPTH: usize = 128;

/// python `json.loads` on a request body (starlette's `request.json()`): the same accepted documents,
/// the same error positions (in characters) and the same messages as python's C scanner
pub fn py_json_loads(body: &[u8]) -> Result<Value, BodyJsonError> {
    parse_document(body, None)
}

/// `py_json_loads` for a handler that only reads keys whose non-string values it treats as falsy or
/// as a failure. python's NaN, Infinity and out-of-range numbers are read as null here, so the body
/// is accepted as python accepts it. (python's `nan` is truthy, so a handler that fails on a
/// non-string value gives the same result for `nan` and for null.)
pub fn py_json_loads_lenient(body: &[u8]) -> Result<Value, BodyJsonError> {
    parse_document(body, Some(Value::Null))
}

/// `py_json_loads` for a handler that reads a key with `value.strip()` after `data.get(key) or ""`.
/// python's NaN, Infinity and out-of-range numbers are truthy and not strings, so `.strip()` fails on
/// them. they are read as `true` here: a truthy non-string that `or_strip` rejects, as python rejects it.
pub fn py_json_loads_python_only_true(body: &[u8]) -> Result<Value, BodyJsonError> {
    parse_document(body, Some(Value::Bool(true)))
}

/// `python_only_as` is the value a python-only number reads as; None when `Value` cannot hold it
fn parse_document(body: &[u8], python_only_as: Option<Value>) -> Result<Value, BodyJsonError> {
    // json.loads on bytes decodes with utf-8-sig, which drops a leading byte order mark
    let Ok(text) = std::str::from_utf8(body) else { return Err(BodyJsonError::Unrepresentable) };
    let doc: Vec<char> = text.strip_prefix('\u{feff}').unwrap_or(text).chars().collect();
    let scanner = JsonScanner { doc: &doc, python_only_as };
    let start = scanner.ws(0);
    let (value, end) = scanner.value(start, 0)?;
    let end = scanner.ws(end);
    if end != doc.len() {
        return Err(decode_error(&doc, "Extra data", end));
    }
    Ok(value)
}

/// the value python's `json.loads` gives a request body, or the text python's `str(e)` gives when it
/// raises. for the rare bodies python accepts but `Value` cannot hold, the text is serde_json's
/// (a known deviation: python would go on to fail later in the handler with its own message)
pub fn parse_body(body: &[u8]) -> Result<Value, String> {
    match py_json_loads(body) {
        Ok(value) => Ok(value),
        Err(BodyJsonError::Decode(message)) => Err(message),
        Err(BodyJsonError::Unrepresentable) => {
            Err(serde_json::from_slice::<Value>(body).err().map(|e| e.to_string()).unwrap_or_default())
        }
    }
}

/// python `JSONDecodeError(msg, doc, pos)` rendered through `str()`
fn decode_error(doc: &[char], msg: &str, pos: usize) -> BodyJsonError {
    let lineno = doc[..pos].iter().filter(|&&c| c == '\n').count() + 1;
    let colno = match doc[..pos].iter().rposition(|&c| c == '\n') {
        Some(newline) => pos - newline,
        None => pos + 1,
    };
    BodyJsonError::Decode(format!("{msg}: line {lineno} column {colno} (char {pos})"))
}

/// recursive-descent mirror of CPython's `_json` scanner. every method takes the index of the
/// first character it may consume and returns the index just past what it consumed
struct JsonScanner<'a> {
    doc: &'a [char],
    /// what python-only values (NaN, Infinity, out-of-range numbers) read as; None fails the body
    python_only_as: Option<Value>,
}

impl JsonScanner<'_> {
    fn ws(&self, mut i: usize) -> usize {
        while matches!(self.doc.get(i), Some(' ' | '\t' | '\n' | '\r')) {
            i += 1;
        }
        i
    }

    fn word(&self, i: usize, word: &str) -> bool {
        word.chars().enumerate().all(|(k, c)| self.doc.get(i + k) == Some(&c))
    }

    /// a value python parses but `Value` cannot hold: a failure, or its stand-in, ending at `end`
    fn python_only(&self, end: usize) -> Result<(Value, usize), BodyJsonError> {
        match &self.python_only_as {
            Some(stand_in) => Ok((stand_in.clone(), end)),
            None => Err(BodyJsonError::Unrepresentable),
        }
    }

    fn value(&self, i: usize, depth: usize) -> Result<(Value, usize), BodyJsonError> {
        match self.doc.get(i) {
            Some('"') => {
                let (text, end) = self.string(i + 1)?;
                Ok((Value::String(text), end))
            }
            Some('{') => self.object(i + 1, depth + 1),
            Some('[') => self.array(i + 1, depth + 1),
            Some('n') if self.word(i, "null") => Ok((Value::Null, i + 4)),
            Some('t') if self.word(i, "true") => Ok((Value::Bool(true), i + 4)),
            Some('f') if self.word(i, "false") => Ok((Value::Bool(false), i + 5)),
            Some('N') if self.word(i, "NaN") => self.python_only(i + 3),
            Some('I') if self.word(i, "Infinity") => self.python_only(i + 8),
            Some('-') if self.word(i, "-Infinity") => self.python_only(i + 9),
            _ => self.number(i),
        }
    }

    fn number(&self, start: usize) -> Result<(Value, usize), BodyJsonError> {
        let d = self.doc;
        let digits = |mut j: usize| {
            while matches!(d.get(j), Some(c) if c.is_ascii_digit()) {
                j += 1;
            }
            j
        };
        let mut j = start;
        if d.get(j) == Some(&'-') {
            j += 1;
        }
        match d.get(j) {
            Some('0') => j += 1,
            Some(c) if c.is_ascii_digit() => j = digits(j),
            _ => return Err(decode_error(d, "Expecting value", start)),
        }
        let mut is_float = false;
        if d.get(j) == Some(&'.') && matches!(d.get(j + 1), Some(c) if c.is_ascii_digit()) {
            j = digits(j + 1);
            is_float = true;
        }
        if matches!(d.get(j), Some('e' | 'E')) {
            let mut k = j + 1;
            if matches!(d.get(k), Some('+' | '-')) {
                k += 1;
            }
            if matches!(d.get(k), Some(c) if c.is_ascii_digit()) {
                j = digits(k);
                is_float = true;
            }
        }
        let text: String = d[start..j].iter().collect();
        if is_float {
            return match text.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
                Some(n) => Ok((Value::Number(n), j)),
                None => self.python_only(j),
            };
        }
        match text.parse::<i64>().map(Value::from).or_else(|_| text.parse::<u64>().map(Value::from)) {
            Ok(v) => Ok((v, j)),
            Err(_) => self.python_only(j),
        }
    }

    /// `start` is the index just past the opening quote
    fn string(&self, start: usize) -> Result<(String, usize), BodyJsonError> {
        let d = self.doc;
        let begin = start - 1;
        let mut out = String::new();
        let mut i = start;
        loop {
            let mut j = i;
            while let Some(&c) = d.get(j) {
                if c == '"' || c == '\\' || (c as u32) < 0x20 {
                    break;
                }
                j += 1;
            }
            out.extend(&d[i..j]);
            match d.get(j) {
                None => return Err(decode_error(d, "Unterminated string starting at", begin)),
                Some('"') => return Ok((out, j + 1)),
                Some('\\') => {}
                Some(_) => return Err(decode_error(d, "Invalid control character at", j)),
            }
            // j is the backslash
            let Some(&esc) = d.get(j + 1) else {
                return Err(decode_error(d, "Unterminated string starting at", begin));
            };
            let plain = match esc {
                '"' => '"',
                '\\' => '\\',
                '/' => '/',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'u' => {
                    let (unit, next) = self.hex4(j + 1)?;
                    i = next;
                    let mut code = unit;
                    // a high surrogate followed by an escaped low surrogate is one character
                    if (0xD800..0xDC00).contains(&unit) && d.get(next) == Some(&'\\') && d.get(next + 1) == Some(&'u') {
                        let (low, after) = self.hex4(next + 1)?;
                        if (0xDC00..0xE000).contains(&low) {
                            code = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                            i = after;
                        }
                    }
                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                    continue;
                }
                _ => return Err(decode_error(d, "Invalid \\escape", j)),
            };
            out.push(plain);
            i = j + 2;
        }
    }

    /// the four hex digits of a `\u` escape whose `u` is at `u_at`; returns the code unit and the
    /// index just past the digits
    fn hex4(&self, u_at: usize) -> Result<(u32, usize), BodyJsonError> {
        let unit = (1..=4).try_fold(0u32, |acc, k| {
            self.doc.get(u_at + k).and_then(|c| c.to_digit(16)).map(|v| acc * 16 + v)
        });
        match unit {
            Some(unit) => Ok((unit, u_at + 5)),
            None => Err(decode_error(self.doc, "Invalid \\uXXXX escape", u_at)),
        }
    }

    /// `start` is the index just past the `{`
    fn object(&self, start: usize, depth: usize) -> Result<(Value, usize), BodyJsonError> {
        if depth > MAX_JSON_DEPTH {
            return Err(BodyJsonError::Unrepresentable);
        }
        let d = self.doc;
        let mut map = Map::new();
        let mut i = self.ws(start);
        if d.get(i) == Some(&'}') {
            return Ok((Value::Object(map), i + 1));
        }
        loop {
            if d.get(i) != Some(&'"') {
                return Err(decode_error(d, "Expecting property name enclosed in double quotes", i));
            }
            let (key, after_key) = self.string(i + 1)?;
            let colon = self.ws(after_key);
            if d.get(colon) != Some(&':') {
                return Err(decode_error(d, "Expecting ':' delimiter", colon));
            }
            let (value, after_value) = self.value(self.ws(colon + 1), depth)?;
            map.insert(key, value);
            let next = self.ws(after_value);
            match d.get(next) {
                Some('}') => return Ok((Value::Object(map), next + 1)),
                Some(',') => i = self.ws(next + 1),
                _ => return Err(decode_error(d, "Expecting ',' delimiter", next)),
            }
        }
    }

    /// `start` is the index just past the `[`
    fn array(&self, start: usize, depth: usize) -> Result<(Value, usize), BodyJsonError> {
        if depth > MAX_JSON_DEPTH {
            return Err(BodyJsonError::Unrepresentable);
        }
        let d = self.doc;
        let mut items = Vec::new();
        let mut i = self.ws(start);
        if d.get(i) == Some(&']') {
            return Ok((Value::Array(items), i + 1));
        }
        loop {
            let (item, after_item) = self.value(i, depth)?;
            items.push(item);
            let next = self.ws(after_item);
            match d.get(next) {
                Some(']') => return Ok((Value::Array(items), next + 1)),
                Some(',') => i = self.ws(next + 1),
                _ => return Err(decode_error(d, "Expecting ',' delimiter", next)),
            }
        }
    }
}

/// python `type(x).__name__` for json values (used in python-style error strings)
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

/// python `str.strip()` with no arguments: removes every char for which `str.isspace()` holds.
/// rust's `trim()` (White_Space) misses U+001C-U+001F, which python treats as whitespace.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// python `(value or default).strip()`: falsy values become `default`, truthy
/// non-strings raise AttributeError (returned as the python error message)
pub fn or_strip(v: Option<&Value>, default: &str) -> Result<String, String> {
    match v {
        Some(v) if truthy(v) => match v {
            Value::String(s) => Ok(py_strip(s).to_string()),
            other => Err(format!("'{}' object has no attribute 'strip'", py_type_name(other))),
        },
        _ => Ok(py_strip(default).to_string()),
    }
}

/// python `value.strip()` for `data.get(key, default)` (no `or` fallback)
pub fn get_strip(v: Option<&Value>, default: &str) -> Result<String, String> {
    match v {
        None => Ok(py_strip(default).to_string()),
        Some(Value::String(s)) => Ok(py_strip(s).to_string()),
        Some(other) => Err(format!("'{}' object has no attribute 'strip'", py_type_name(other))),
    }
}

static DECIMAL_DIGIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\p{Nd}$").unwrap());

/// value of a unicode decimal digit (category Nd), as python's int() accepts them.
/// unicode guarantees each Nd run is made of blocks of ten digits that start at zero,
/// so the offset from the start of the contiguous run gives the digit value
fn decimal_value(c: char) -> Option<u32> {
    if c.is_ascii_digit() {
        return Some(c as u32 - '0' as u32);
    }
    let is_nd = |c: char| DECIMAL_DIGIT.is_match(c.encode_utf8(&mut [0; 4]));
    if !is_nd(c) {
        return None;
    }
    let mut start = c as u32;
    while start > 0 && char::from_u32(start - 1).map(is_nd).unwrap_or(false) {
        start -= 1;
    }
    Some((c as u32 - start) % 10)
}

/// python `int(str)`: surrounding whitespace (python's definition) allowed, optional sign,
/// unicode decimal digits, single underscores between digits. Values beyond the i64 range
/// saturate: every caller clamps (list slicing) or overflows the same way python does.
pub fn py_int_str(s: &str) -> Option<i64> {
    const BIG: i128 = 1 << 70;
    let t = py_strip(s);
    let (negative, body) = match t.chars().next()? {
        '+' => (false, &t[1..]),
        '-' => (true, &t[1..]),
        _ => (false, t),
    };
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return None;
    }
    let mut value: i128 = 0;
    for c in body.chars().filter(|c| *c != '_') {
        value = (value * 10 + decimal_value(c)? as i128).min(BIG);
    }
    let value = if negative { -value } else { value };
    Some(value.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

/// python `int(value)` for json values (floats truncate, bools are 0/1, strings are parsed)
pub fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::String(s) => py_int_str(s),
        _ => None,
    }
}

/// python list slicing `lst[start:stop]` (negative indices count from the end)
pub fn py_slice<T: Clone>(items: &[T], start: i64, stop: i64) -> Vec<T> {
    let len = items.len() as i64;
    let norm = |i: i64| -> i64 {
        if i < 0 {
            (i + len).max(0)
        } else {
            i.min(len)
        }
    };
    let (s, e) = (norm(start), norm(stop));
    if s >= e {
        return Vec::new();
    }
    items[s as usize..e as usize].to_vec()
}

/// python `str.title()`: a run of cased characters starts with its titlecase mapping and the
/// rest of the run is lowercased (capital sigma takes python's final-sigma form when it ends a
/// word). uncased characters (CJK, digits, punctuation) only separate words.
/// std has no titlecase table, so `push_titlecase` lists the mappings that differ from uppercase.
/// known gap: the multi-char Greek/Armenian titlecase specials (e.g. U+0390) stay uppercase.
pub fn py_title(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut previous_cased = false;
    for (i, &c) in chars.iter().enumerate() {
        if !previous_cased {
            push_titlecase(c, &mut out);
        } else if c == '\u{3a3}' {
            let next_cased = chars.get(i + 1).map(|&n| is_cased(n)).unwrap_or(false);
            out.push(if next_cased { '\u{3c3}' } else { '\u{3c2}' });
        } else {
            out.extend(c.to_lowercase());
        }
        previous_cased = is_cased(c);
    }
    out
}

/// unicode `Cased` property (python `str.title` word material): Lowercase, Uppercase and Lt
fn is_cased(c: char) -> bool {
    c.is_lowercase() || c.is_uppercase() || is_titlecase_letter(c)
}

/// the Lt (titlecase letter) general category: the digraphs and the Greek prosgegrammeni forms
fn is_titlecase_letter(c: char) -> bool {
    matches!(c, '\u{1c5}' | '\u{1c8}' | '\u{1cb}' | '\u{1f2}' | '\u{1f88}'..='\u{1f8f}' | '\u{1f98}'..='\u{1f9f}' | '\u{1fa8}'..='\u{1faf}' | '\u{1fbc}' | '\u{1fcc}' | '\u{1ffc}')
}

/// titlecase mapping of one char (SpecialCasing / UnicodeData titlecase entries that differ from
/// uppercase), falling back to uppercase
fn push_titlecase(c: char, out: &mut String) {
    let mapped: &str = match c {
        '\u{df}' => "Ss",
        '\u{1c4}'..='\u{1c6}' => "\u{1c5}",
        '\u{1c7}'..='\u{1c9}' => "\u{1c8}",
        '\u{1ca}'..='\u{1cc}' => "\u{1cb}",
        '\u{1f1}'..='\u{1f3}' => "\u{1f2}",
        '\u{149}' => "\u{2bc}N",
        '\u{1f0}' => "J\u{30c}",
        '\u{1e96}' => "H\u{331}",
        '\u{1e97}' => "T\u{308}",
        '\u{1e98}' => "W\u{30a}",
        '\u{1e99}' => "Y\u{30a}",
        '\u{1e9a}' => "A\u{2be}",
        '\u{fb00}' => "Ff",
        '\u{fb01}' => "Fi",
        '\u{fb02}' => "Fl",
        '\u{fb03}' => "Ffi",
        '\u{fb04}' => "Ffl",
        '\u{fb05}' | '\u{fb06}' => "St",
        '\u{1f80}'..='\u{1f87}' | '\u{1f90}'..='\u{1f97}' | '\u{1fa0}'..='\u{1fa7}' => {
            let base = match c as u32 {
                x if x < 0x1f90 => 0x1f88,
                x if x < 0x1fa0 => 0x1f98,
                _ => 0x1fa8,
            };
            out.push(char::from_u32(base + (c as u32 & 7)).unwrap_or(c));
            return;
        }
        '\u{1fb3}' => {
            out.push('\u{1fbc}');
            return;
        }
        '\u{1fc3}' => {
            out.push('\u{1fcc}');
            return;
        }
        '\u{1ff3}' => {
            out.push('\u{1ffc}');
            return;
        }
        c => {
            out.extend(c.to_uppercase());
            return;
        }
    };
    out.push_str(mapped);
}

/// markupsafe `escape` (what jinja2 autoescape applies to plain str values):
/// `& < > " '` become `&amp; &lt; &gt; &#34; &#39;`, nothing else is touched
pub fn markup_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// python `html.escape(text, quote=True)`
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// python `==` for json-like values (1 == 1.0, dict order doesn't matter)
pub fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(i), Some(j)) => i == j,
            _ => x.as_f64() == y.as_f64(),
        },
        (Value::Bool(x), Value::Number(y)) | (Value::Number(y), Value::Bool(x)) => y.as_f64() == Some(*x as i64 as f64),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).map(|w| py_eq(v, w)).unwrap_or(false))
        }
        _ => a == b,
    }
}

/// python `for x in value`: lists yield their items, strings their characters, dicts their keys;
/// Err where python raises TypeError (numbers, bools, null)
pub fn py_iter(v: &Value) -> Result<Vec<Value>, String> {
    match v {
        Value::Array(items) => Ok(items.clone()),
        Value::String(s) => Ok(s.chars().map(|c| Value::String(c.to_string())).collect()),
        Value::Object(m) => Ok(m.keys().map(|k| Value::String(k.clone())).collect()),
        other => Err(format!("'{}' object is not iterable", py_type_name(other))),
    }
}

/// python `item in container`; Err where python raises TypeError
pub fn py_contains(container: &Value, item: &Value) -> Result<bool, String> {
    match container {
        Value::Array(items) => Ok(items.iter().any(|x| py_eq(x, item))),
        Value::String(s) => match item {
            Value::String(needle) => Ok(s.contains(needle.as_str())),
            other => Err(format!("'in <string>' requires string as left operand, not {}", py_type_name(other))),
        },
        Value::Object(m) => match item {
            Value::Array(_) | Value::Object(_) => Err(format!("unhashable type: '{}'", py_type_name(item))),
            Value::String(key) => Ok(m.contains_key(key)),
            _ => Ok(false),
        },
        other => Err(format!("argument of type '{}' is not iterable", py_type_name(other))),
    }
}

/// python `sorted(set(items))`: duplicates removed by python equality (first kept), then ordered.
/// strings order among themselves, numbers and bools among themselves; a mix, or an unhashable
/// list/dict item, raises in python and gives Err here
pub fn py_sorted_unique(items: Vec<Value>) -> Result<Vec<Value>, String> {
    let mut unique: Vec<Value> = Vec::new();
    for item in items {
        if matches!(item, Value::Array(_) | Value::Object(_)) {
            return Err(format!("unhashable type: '{}'", py_type_name(&item)));
        }
        if !unique.iter().any(|u| py_eq(u, &item)) {
            unique.push(item);
        }
    }
    if unique.iter().all(Value::is_string) {
        unique.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    } else if unique.iter().all(|v| matches!(v, Value::Number(_) | Value::Bool(_))) {
        let num = |v: &Value| match v {
            Value::Bool(b) => *b as i64 as f64,
            other => other.as_f64().unwrap_or(0.0),
        };
        unique.sort_by(|a, b| num(a).partial_cmp(&num(b)).unwrap_or(std::cmp::Ordering::Equal));
    } else {
        return Err("'<' not supported between instances of the mixed types in the set".into());
    }
    Ok(unique)
}

/// python `bool(value)`; missing values are falsy
pub fn truthy_opt(v: Option<&Value>) -> bool {
    v.map(truthy).unwrap_or(false)
}

/// python `Webui.host` for `network_mode` (`local` 127.0.0.1, `internet` 0.0.0.0, `custom` the
/// configured `custom_host`, anything else 127.0.0.1). `None` when custom mode has no custom_host:
/// uvicorn passes that None to asyncio.create_server, which binds every interface
pub fn bind_host(network_mode: Option<&str>, custom_host: Option<&str>) -> Option<String> {
    match network_mode {
        Some("local") => Some("127.0.0.1".to_string()),
        Some("internet") => Some("0.0.0.0".to_string()),
        Some("custom") => custom_host.map(str::to_string),
        _ => Some("127.0.0.1".to_string()),
    }
}

/// python `str.strip("/").lower()`: the canonical form of a module extension route path
pub fn py_strip_lower(s: &str) -> String {
    s.trim_matches('/').to_lowercase()
}

/// python `Webui.url`: `http://{self.host}:{self.port}`, where an unset (None) host renders as "None"
pub fn channel_url(host: Option<&str>, port: impl std::fmt::Display) -> String {
    format!("http://{}:{}", host.unwrap_or("None"), port)
}

const TOOL_CALL_FALLBACK: &str = "🔧 Calling tool...";

/// python `ToolcallManager.display_call` (core/toolcalls.py). Everything python raises inside its
/// try block (tool data that is not a dict, a `function` that is not a dict, arguments that repair
/// to a non-dict) returns the generic fallback string. ol-core's display_call differs on those
/// inputs, so the chat export formats tool calls through this function.
pub fn display_call(tool_data: &Value) -> String {
    let Some(function) = tool_data.get("function") else {
        return TOOL_CALL_FALLBACK.into();
    };
    let Some(function) = function.as_object() else {
        return TOOL_CALL_FALLBACK.into();
    };
    let func_name = function.get("name").map(py_str).unwrap_or_else(|| "unknown".into());
    let raw_args = function.get("arguments").cloned().unwrap_or_else(|| json!("{}"));
    let args: Map<String, Value> = match raw_args {
        Value::String(s) => match repair_loads(&s) {
            Value::Object(m) => m,
            _ => return TOOL_CALL_FALLBACK.into(),
        },
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let arg_strs: Vec<String> = args
        .iter()
        .map(|(key, value)| {
            let mut value_str = py_str(value);
            if char_len(&value_str) > 30 {
                value_str = format!("{}..", take_chars(&value_str, 30));
            }
            format!("{}=\"{}\"", key, value_str.replace('"', "'"))
        })
        .collect();
    format!("🔧 {}({})", func_name, arg_strs.join(", "))
}

/// python `ChatStore.export` (core/chat.py) over the current chat. The Err string is the python
/// exception message; the route turns any Err into a 500, as starlette does for the raised error.
pub fn export_chat_text(chat: &Chat) -> Result<String, String> {
    if chat.current().is_none() {
        return Err("No chat is currently loaded!".into());
    }
    export_turns_text(&group_history(&chat.messages_get()))
}

/// the formatting half of python `ChatStore.export`, over already grouped turns
pub fn export_turns_text(turns: &[Value]) -> Result<String, String> {
    let mut turn_export: Vec<String> = Vec::new();
    for turn in turns {
        let mut turn_lines = vec![format!("--- {} ---", py_str_opt(turn.get("role")))];
        let messages = turn
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| "turn messages is not a list".to_string())?;

        // (kind, text) pairs in order; kind is "content" or "tool"
        let mut items: Vec<(&str, String)> = Vec::new();
        for message in messages {
            let Some(message) = message.as_object() else {
                return Err("message object has no attribute 'get'".into());
            };
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            let toolcalls = message.get("tool_calls").filter(|t| truthy(t));

            if let Some(content) = message.get("content").filter(|c| truthy(c)) {
                if role != "tool" && toolcalls.is_none() {
                    match content {
                        Value::String(s) => items.push(("content", py_strip(s).to_string())),
                        Value::Array(parts) => {
                            for part in parts {
                                let Some(part) = part.as_object() else {
                                    return Err("content part object has no attribute 'get'".into());
                                };
                                if part.get("type").and_then(Value::as_str) == Some("text") {
                                    // python `part.get('text').strip()`: a missing or non-str text raises
                                    let text = part
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .ok_or_else(|| "'NoneType' object has no attribute 'strip'".to_string())?;
                                    items.push(("content", py_strip(text).to_string()));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            if let Some(toolcalls) = toolcalls {
                match toolcalls {
                    Value::Array(calls) => items.extend(calls.iter().map(|tc| ("tool", display_call(tc)))),
                    // python iterates a truthy dict (its keys) or string (its characters); each item is a str,
                    // which display_call never reads as a tool call
                    Value::Object(map) => items.extend(map.keys().map(|_| ("tool", TOOL_CALL_FALLBACK.to_string()))),
                    Value::String(s) => items.extend(s.chars().map(|_| ("tool", TOOL_CALL_FALLBACK.to_string()))),
                    _ => return Err("tool_calls value is not iterable".into()),
                }
            }
        }

        // group consecutive items of one kind into blocks
        let mut blocks: Vec<Vec<String>> = Vec::new();
        let mut current_kind: Option<&str> = None;
        for (kind, text) in items {
            if current_kind != Some(kind) {
                blocks.push(Vec::new());
                current_kind = Some(kind);
            }
            if let Some(block) = blocks.last_mut() {
                block.push(text);
            }
        }

        // blocks join with a blank line, items within a block with a single newline
        let formatted: Vec<String> = blocks.iter().map(|b| b.join("\n")).collect();
        turn_lines.push(formatted.join("\n\n"));
        turn_export.push(turn_lines.join("\n"));
    }
    Ok(turn_export.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn py_iteration_and_membership_follow_python() {
        assert_eq!(py_iter(&json!("ab")).unwrap(), vec![json!("a"), json!("b")]);
        assert_eq!(py_iter(&json!({"k": 1})).unwrap(), vec![json!("k")]);
        assert!(py_iter(&json!(5)).is_err());
        assert!(py_contains(&json!("money"), &json!("one")).unwrap());
        assert!(py_contains(&json!("money"), &json!(1)).is_err());
        assert!(py_contains(&json!(["a", 1]), &json!(1.0)).unwrap());
        assert!(!py_contains(&json!({"k": 1}), &json!(1)).unwrap());
        assert!(py_contains(&json!({"k": 1}), &json!([])).is_err());
    }

    #[test]
    fn sorted_unique_orders_one_kind_and_rejects_mixes() {
        assert_eq!(py_sorted_unique(vec![json!("b"), json!("a"), json!("b")]).unwrap(), vec![json!("a"), json!("b")]);
        assert_eq!(py_sorted_unique(vec![json!(2), json!(1.0), json!(1)]).unwrap(), vec![json!(1.0), json!(2)]);
        assert!(py_sorted_unique(vec![json!("a"), json!(1)]).is_err());
        assert!(py_sorted_unique(vec![json!(["x"])]).is_err());
    }

    #[test]
    fn display_call_matches_python_format() {
        let call = json!({"function": {"name": "web_search", "arguments": "{\"q\": \"rust \\\"nightly\\\"\", \"n\": 3}"}});
        assert_eq!(display_call(&call), "🔧 web_search(q=\"rust 'nightly'\", n=\"3\")");
        // arguments longer than 30 chars are cut to 30 and suffixed with '..'
        let long = json!({"function": {"name": "f", "arguments": {"text": "x".repeat(40)}}});
        assert_eq!(display_call(&long), format!("🔧 f(text=\"{}..\")", "x".repeat(30)));
        // missing name is 'unknown', a null name renders as python None
        assert_eq!(display_call(&json!({"function": {}})), "🔧 unknown()");
        assert_eq!(display_call(&json!({"function": {"name": null}})), "🔧 None()");
    }

    #[test]
    fn display_call_falls_back_where_python_raises() {
        // python: tool_data['function'].get raises on null, lists, strings
        assert_eq!(display_call(&json!({"function": null})), "🔧 Calling tool...");
        assert_eq!(display_call(&json!({"function": ["x"]})), "🔧 Calling tool...");
        assert_eq!(display_call(&json!({"name": "no function key"})), "🔧 Calling tool...");
        assert_eq!(display_call(&json!("plain string")), "🔧 Calling tool...");
        // python: a string argument that repairs to a non-dict hits args_dict.items()
        assert_eq!(display_call(&json!({"function": {"name": "f", "arguments": "[1, 2]"}})), "🔧 Calling tool...");
        assert_eq!(display_call(&json!({"function": {"name": "f", "arguments": "\"s\""}})), "🔧 Calling tool...");
        // non-string, non-dict arguments are an empty dict, not a failure
        assert_eq!(display_call(&json!({"function": {"name": "f", "arguments": null}})), "🔧 f()");
    }

    #[test]
    fn export_strips_like_python_including_separators() {
        let turns = vec![json!({
            "role": "assistant",
            "messages": [
                {"role": "assistant", "content": "\u{1c}hi there\u{1f}"},
                {"role": "assistant", "content": [{"type": "text", "text": "\u{1d}part\u{1e}"}]}
            ]
        })];
        assert_eq!(export_turns_text(&turns).unwrap(), "--- assistant ---\nhi there\npart");
    }

    #[test]
    fn export_text_part_without_text_is_an_error() {
        let missing = vec![json!({"role": "assistant", "messages": [
            {"role": "assistant", "content": [{"type": "text"}]}
        ]})];
        assert!(export_turns_text(&missing).is_err());
        let null_text = vec![json!({"role": "assistant", "messages": [
            {"role": "assistant", "content": [{"type": "text", "text": null}]}
        ]})];
        assert!(export_turns_text(&null_text).is_err());
    }

    #[test]
    fn export_tool_calls_follow_python_iteration() {
        // a list is one display line per call
        let list = vec![json!({"role": "assistant", "messages": [
            {"role": "assistant", "tool_calls": [{"function": {"name": "f", "arguments": {"a": 1}}}]}
        ]})];
        assert_eq!(export_turns_text(&list).unwrap(), "--- assistant ---\n🔧 f(a=\"1\")");
        // a truthy dict or string is iterated: one generic line per key or character, and it
        // suppresses the message content as a tool call does
        let dict = vec![json!({"role": "assistant", "messages": [
            {"role": "assistant", "content": "ignored", "tool_calls": {"a": 1, "b": 2}}
        ]})];
        assert_eq!(export_turns_text(&dict).unwrap(), "--- assistant ---\n🔧 Calling tool...\n🔧 Calling tool...");
        let string = vec![json!({"role": "assistant", "messages": [
            {"role": "assistant", "tool_calls": "ab"}
        ]})];
        assert_eq!(export_turns_text(&string).unwrap(), "--- assistant ---\n🔧 Calling tool...\n🔧 Calling tool...");
        // a truthy number is not iterable: python raises
        let number = vec![json!({"role": "assistant", "messages": [
            {"role": "assistant", "tool_calls": 5}
        ]})];
        assert!(export_turns_text(&number).is_err());
    }

    #[test]
    fn ints() {
        assert_eq!(py_int_str(" 5 "), Some(5));
        assert_eq!(py_int_str("-60"), Some(-60));
        assert_eq!(py_int_str("1_000"), Some(1000));
        assert_eq!(py_int_str("abc"), None);
        assert_eq!(py_int_str(""), None);
        assert_eq!(py_int(&json!(60.9)), Some(60));
        assert_eq!(py_int(&json!(true)), Some(1));
    }

    #[test]
    fn slicing() {
        let v = vec![1, 2, 3, 4, 5];
        assert_eq!(py_slice(&v, 0, 2), vec![1, 2]);
        assert_eq!(py_slice(&v, 3, 50), vec![4, 5]);
        assert_eq!(py_slice(&v, -2, 48), vec![4, 5]);
        assert_eq!(py_slice(&v, -10, 1), vec![1]);
        assert!(py_slice(&v, 9, 20).is_empty());
    }

    #[test]
    fn escaping_and_strip() {
        assert_eq!(html_escape(r#"<a href="x">'&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;&#x27;&lt;/a&gt;");
        assert_eq!(or_strip(Some(&json!("  hi ")), "general").unwrap(), "hi");
        assert_eq!(or_strip(Some(&json!(null)), "general").unwrap(), "general");
        assert_eq!(or_strip(Some(&json!(0)), "").unwrap(), "");
        assert_eq!(or_strip(Some(&json!(5)), "").unwrap_err(), "'int' object has no attribute 'strip'");
        assert!(py_eq(&json!({"a": [1, 2.0]}), &json!({"a": [1.0, 2]})));
        assert_eq!(markup_escape("/assets/a&b'<\"x\">"), "/assets/a&amp;b&#39;&lt;&#34;x&#34;&gt;");
    }

    #[test]
    fn python_strip_and_int_semantics() {
        // python str.isspace() includes the file/group/record/unit separators
        assert_eq!(py_strip("\u{1c}x\u{1f}"), "x");
        assert_eq!(or_strip(Some(&json!("\u{1d}title\u{1e}")), "general").unwrap(), "title");
        assert_eq!(py_int_str("\u{1c}7"), Some(7));
        // unicode decimal digits (Nd) are accepted by python int()
        assert_eq!(py_int_str("\u{0665}\u{0664}"), Some(54));
        assert_eq!(py_int_str("\u{1d7db}"), Some(3));
        assert_eq!(py_int_str("\u{00b2}"), None);
        // arbitrary-size values saturate: callers clamp, so the result matches python's
        assert_eq!(py_int_str("99999999999999999999999"), Some(i64::MAX));
        assert_eq!(py_int_str("-99999999999999999999999"), Some(i64::MIN));
        assert_eq!(py_int_str("1__0"), None);
    }

    #[test]
    fn python_title() {
        assert_eq!(py_title("hello world-foo"), "Hello World-Foo");
        // python title() only counts cased characters as word material
        assert_eq!(py_title("\u{4e2d}a"), "\u{4e2d}A");
        assert_eq!(py_title("don't"), "Don'T");
        // titlecase differs from uppercase for these (python: 'ß'.title() == 'Ss')
        assert_eq!(py_title("\u{df}a"), "Ssa");
        assert_eq!(py_title("\u{1c6}a b"), "\u{1c5}a B");
        assert_eq!(py_title("\u{fb01}x"), "Fix");
        assert_eq!(py_title("\u{149}a"), "\u{2bc}Na");
        // capital sigma at the end of a word is the final form, as in python
        assert_eq!(py_title("\u{391}\u{394}\u{39f}\u{3a3}"), "\u{391}\u{3b4}\u{3bf}\u{3c2}");
        assert_eq!(py_title("a\u{130}y"), "A\u{69}\u{307}y");
    }

    #[test]
    fn bind_host_and_url() {
        assert_eq!(bind_host(Some("local"), Some("10.0.0.5")), Some("127.0.0.1".to_string()));
        assert_eq!(bind_host(Some("internet"), None), Some("0.0.0.0".to_string()));
        assert_eq!(bind_host(Some("custom"), Some("10.0.0.5")), Some("10.0.0.5".to_string()));
        // python: custom mode without custom_host has host None (every interface is bound)...
        assert_eq!(bind_host(Some("custom"), None), None);
        // ...while the url f-string renders that None as "None"
        assert_eq!(channel_url(bind_host(Some("custom"), None).as_deref(), 8080), "http://None:8080");
        // unknown or missing mode falls back to loopback
        assert_eq!(bind_host(Some("lan"), Some("10.0.0.5")), Some("127.0.0.1".to_string()));
        assert_eq!(bind_host(None, None), Some("127.0.0.1".to_string()));
        assert_eq!(channel_url(bind_host(Some("local"), None).as_deref(), 8080), "http://127.0.0.1:8080");
    }

    #[test]
    fn empty_bodies_get_pythons_expecting_value_position() {
        assert_eq!(parse_body(b"").unwrap_err(), "Expecting value: line 1 column 1 (char 0)");
        assert_eq!(parse_body(b"  ").unwrap_err(), "Expecting value: line 1 column 3 (char 2)");
        assert_eq!(parse_body(b"\n\n").unwrap_err(), "Expecting value: line 3 column 1 (char 2)");
        assert!(parse_body(b" \n a").unwrap_err().starts_with("Expecting value: line 2 column 2 (char 3)"));
    }

    #[test]
    fn malformed_bodies_get_the_python_json_decoder_messages() {
        let cases = [
            (&b"{a:1}"[..], "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)"),
            (&b"{\"a\" 1}"[..], "Expecting ':' delimiter: line 1 column 6 (char 5)"),
            (&b"[1 2]"[..], "Expecting ',' delimiter: line 1 column 4 (char 3)"),
            (&b"[1,]"[..], "Expecting value: line 1 column 4 (char 3)"),
            (&b"{\"a\":1,}"[..], "Expecting property name enclosed in double quotes: line 1 column 8 (char 7)"),
            (&b"1 2"[..], "Extra data: line 1 column 3 (char 2)"),
            (&b"\"\\x\""[..], "Invalid \\escape: line 1 column 2 (char 1)"),
            (&b"\"abc"[..], "Unterminated string starting at: line 1 column 1 (char 0)"),
            (&b"\"a\nb\""[..], "Invalid control character at: line 1 column 3 (char 2)"),
            (&b"\"\\u12\""[..], "Invalid \\uXXXX escape: line 1 column 3 (char 2)"),
        ];
        for (body, message) in cases {
            assert_eq!(parse_body(body).unwrap_err(), message, "body {:?}", String::from_utf8_lossy(body));
        }
    }

    #[test]
    fn valid_bodies_parse_like_python_json_loads() {
        assert_eq!(parse_body(b" {\"a\": [1, 2.5, true, null]} \n").unwrap(), json!({"a": [1, 2.5, true, null]}));
        assert_eq!(parse_body(b"\"\\u00e9\\ud83d\\ude00\"").unwrap(), json!("\u{e9}\u{1f600}"));
        assert_eq!(parse_body(b"-0").unwrap(), json!(0));
        // python accepts NaN; a value cannot hold it, so the text is serde's and the body is rejected
        assert!(parse_body(b"[NaN]").is_err());
    }

    #[test]
    fn lenient_bodies_read_python_only_numbers_as_null() {
        // python's json.loads accepts these; the lenient reader keeps every other key intact
        assert_eq!(
            py_json_loads_lenient(b"{\"category\": \"work\", \"x\": NaN, \"y\": [Infinity, -Infinity, 1e400, 123456789012345678901234567890]}")
                .unwrap(),
            json!({"category": "work", "x": null, "y": [null, null, null, null]})
        );
        assert_eq!(py_json_loads_lenient(b"NaN").unwrap(), Value::Null);
        // structural errors still fail with python's messages, and the strict reader still rejects NaN
        assert_eq!(
            py_json_loads_lenient(b"{\"a\": NaN,}").unwrap_err(),
            BodyJsonError::Decode("Expecting property name enclosed in double quotes: line 1 column 11 (char 10)".into())
        );
        assert_eq!(py_json_loads(b"[NaN]").unwrap_err(), BodyJsonError::Unrepresentable);
    }

    #[test]
    fn python_only_true_reader_makes_python_only_numbers_truthy_non_strings() {
        // a NaN message is truthy in python and has no .strip(); here it reads as true, which or_strip rejects
        assert_eq!(
            py_json_loads_python_only_true(b"{\"message\": NaN, \"x\": [Infinity], \"n\": 0}").unwrap(),
            json!({"message": true, "x": [true], "n": 0})
        );
        assert_eq!(py_json_loads_python_only_true(b"NaN").unwrap(), json!(true));
        assert!(py_json_loads_python_only_true(b"{\"a\": NaN,}").is_err());
    }
}
