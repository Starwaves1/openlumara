//! small python-compat helpers shared by the modules of this crate

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use ol_core::pyjson::{py_str, truthy};
use serde_json::{Map, Value};

/// python `datetime.datetime.now()` (naive local time)
pub fn now() -> NaiveDateTime {
    chrono::Local::now().naive_local()
}

/// python `datetime.isoformat()` of a naive datetime
pub fn iso(dt: &NaiveDateTime) -> String {
    ol_core::globals::naive_isoformat(dt)
}

/// python `datetime.datetime.fromisoformat()`; the error is python's ValueError message
pub fn fromisoformat(s: &str) -> Result<NaiveDateTime, String> {
    if let Some(dt) = ol_core::globals::parse_isoformat(s) {
        return Ok(dt);
    }
    for fmt in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H", "%Y-%m-%d %H", "%Y%m%dT%H%M%S", "%Y%m%d"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(dt);
        }
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Ok(d.and_hms_opt(0, 0, 0).unwrap());
        }
    }
    Err(format!("Invalid isoformat string: {}", py_str_repr(s)))
}

/// fromisoformat() of a json value (non-strings raise TypeError in python)
pub fn fromisoformat_value(v: Option<&Value>) -> Result<NaiveDateTime, String> {
    match v {
        Some(Value::String(s)) => fromisoformat(s),
        Some(other) => Err(format!("fromisoformat: argument must be str, not {}", py_type(other))),
        None => Err("fromisoformat: argument must be str".into()),
    }
}

/// python repr() of a str
pub fn py_str_repr(s: &str) -> String {
    ol_core::pyjson::py_repr(&Value::String(s.to_string()))
}

/// python type name of a json value
pub fn py_type(v: &Value) -> &'static str {
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

/// python `datetime.datetime(year, month, day, hour, minute)` with python's ValueErrors
pub fn py_datetime(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> Result<NaiveDateTime, String> {
    if !(1..=9999).contains(&year) {
        return Err(format!("year {} is out of range", year));
    }
    if !(1..=12).contains(&month) {
        return Err("month must be in 1..12".into());
    }
    let Some(date) = NaiveDate::from_ymd_opt(year as i32, month as u32, day.clamp(0, 99) as u32).filter(|_| day >= 1) else {
        return Err("day is out of range for month".into());
    };
    if !(0..=23).contains(&hour) {
        return Err("hour must be in 0..23".into());
    }
    if !(0..=59).contains(&minute) {
        return Err("minute must be in 0..59".into());
    }
    Ok(date.and_hms_opt(hour as u32, minute as u32, 0).unwrap())
}

/// python `dt.replace(hour=, minute=, second=, microsecond=0)`
pub fn replace_time(dt: &NaiveDateTime, hour: i64, minute: i64, second: i64) -> Result<NaiveDateTime, String> {
    if !(0..=23).contains(&hour) {
        return Err("hour must be in 0..23".into());
    }
    if !(0..=59).contains(&minute) {
        return Err("minute must be in 0..59".into());
    }
    if !(0..=59).contains(&second) {
        return Err("second must be in 0..59".into());
    }
    Ok(dt.date().and_hms_opt(hour as u32, minute as u32, second as u32).unwrap())
}

/// python `dt.weekday()` (monday = 0)
pub fn weekday(dt: &NaiveDateTime) -> i64 {
    dt.weekday().num_days_from_monday() as i64
}

/// `(a - b).total_seconds()`
pub fn total_seconds(a: &NaiveDateTime, b: &NaiveDateTime) -> f64 {
    let d = *a - *b;
    d.num_microseconds().map(|us| us as f64 / 1e6).unwrap_or(d.num_seconds() as f64)
}

/// midnight of the given datetime (`dt.replace(hour=0, minute=0, second=0, microsecond=0)`)
pub fn midnight(dt: &NaiveDateTime) -> NaiveDateTime {
    dt.date().and_hms_opt(0, 0, 0).unwrap()
}

/// python `int(x)` for json values (strings are parsed strictly, floats truncated)
pub fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::Bool(b) => Some(*b as i64),
        Value::String(s) => {
            let t = s.trim().replace('_', "");
            if t.is_empty() {
                return None;
            }
            t.parse::<i64>().ok()
        }
        _ => None,
    }
}

/// python `int(s)` for str
pub fn py_int_str(s: &str) -> Option<i64> {
    py_int(&Value::String(s.to_string()))
}

/// `int(query.get(key, default) or default)` with ValueError -> default
pub fn query_int(query: &Map<String, Value>, key: &str, default: i64) -> i64 {
    match query.get(key) {
        Some(v) if truthy(v) => py_int(v).unwrap_or(default),
        _ => default,
    }
}

/// `str(query.get(key, default))`
pub fn query_str(query: &Map<String, Value>, key: &str, default: &str) -> String {
    query.get(key).map(py_str).unwrap_or_else(|| default.to_string())
}

/// `(body or {}).get(key)` -> owned value (None when missing)
pub fn body_get(body: &Option<Value>, key: &str) -> Option<Value> {
    body.as_ref().and_then(|b| b.as_object()).and_then(|b| b.get(key)).cloned()
}

/// `str(body.get(key, default))`
pub fn body_str(body: &Option<Value>, key: &str, default: &str) -> String {
    body_get(body, key).map(|v| py_str(&v)).unwrap_or_else(|| default.to_string())
}

/// python `body = body or {}` followed by `.get(...)`: a truthy non-object body raises AttributeError
pub fn dict_body(body: &Option<Value>) -> anyhow::Result<()> {
    match body {
        Some(v) if truthy(v) && !v.is_object() => Err(anyhow::anyhow!("'{}' object has no attribute 'get'", py_type(v))),
        _ => Ok(()),
    }
}

/// python truthiness of an optional value
pub fn truthy_opt(v: Option<&Value>) -> bool {
    v.map(truthy).unwrap_or(false)
}

/// python `round(x, 2)` (correctly rounded, like python's float rounding)
pub fn round2(x: f64) -> f64 {
    format!("{:.2}", x).parse().unwrap_or(x)
}

/// python `os.path.expanduser`
pub fn expanduser(p: &str) -> String {
    if p == "~" || p.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home, &p[1..]);
        }
    }
    p.to_string()
}

/// python `base64.b64decode(s)` on bytes (binascii `a2b_base64`, non-strict, python 3.14). non-alphabet
/// bytes are skipped. a '=' only counts as padding after a partial quad, and the run of pads that directly
/// follows the last data character must complete the quad: a short run or a 1-character tail is an error
pub fn b64decode(s: &[u8]) -> Result<Vec<u8>, String> {
    use base64::engine::{general_purpose::GeneralPurpose, DecodePaddingMode, GeneralPurposeConfig};
    use base64::Engine;
    let mut data: Vec<u8> = Vec::new();
    let mut quad_pos = 0usize;
    let mut pad_run = 0usize;
    for &c in s {
        if c == b'=' {
            if quad_pos >= 2 {
                pad_run += 1;
            }
            continue;
        }
        if !(c.is_ascii_alphanumeric() || c == b'+' || c == b'/') {
            continue;
        }
        data.push(c);
        quad_pos = (quad_pos + 1) % 4;
        pad_run = 0;
    }
    match quad_pos {
        1 => {
            return Err(format!(
                "Invalid base64-encoded string: number of data characters ({}) cannot be 1 more than a multiple of 4",
                data.len()
            ))
        }
        2 if pad_run < 2 => return Err("Incorrect padding".into()),
        3 if pad_run < 1 => return Err("Incorrect padding".into()),
        _ => {}
    }
    let engine = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true).with_decode_padding_mode(DecodePaddingMode::RequireNone),
    );
    engine.decode(&data).map_err(|_| "Incorrect padding".to_string())
}

/// python `base64.b64decode(str)`: a str argument must be ASCII before it is decoded
pub fn b64decode_text(s: &str) -> Result<Vec<u8>, String> {
    if !s.is_ascii() {
        return Err("string argument should contain only ASCII characters".into());
    }
    b64decode(s.as_bytes())
}

/// python `bytes.decode("utf-8")`, with CPython's UnicodeDecodeError text
pub fn py_utf8_decode(bytes: &[u8]) -> Result<String, String> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.to_string()),
        Err(e) => {
            let start = e.valid_up_to();
            let (end, reason) = match e.error_len() {
                Some(n) => {
                    // a lead byte that is valid on its own fails on the continuation byte after it
                    let lead_ok = matches!(bytes[start], 0xC2..=0xF4);
                    (start + n, if lead_ok { "invalid continuation byte" } else { "invalid start byte" })
                }
                None => (bytes.len(), "unexpected end of data"),
            };
            let at = if end - start == 1 {
                format!("byte 0x{:02x} in position {}", bytes[start], start)
            } else {
                format!("bytes in position {}-{}", start, end - 1)
            };
            Err(format!("'utf-8' codec can't decode {}: {}", at, reason))
        }
    }
}

/// python `str.strip()`: strips every char for which `str.isspace()` is true. that set is rust's
/// White_Space plus U+001C..=U+001F (python counts the file/group/record/unit separators as spaces)
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// python `a.lower().strip() == b.lower().strip()` used by several lookups
pub fn eq_ci_strip(a: &str, b: &str) -> bool {
    py_strip(&a.to_lowercase()) == py_strip(&b.to_lowercase())
}

/// python's json decoder recurses; a container nested deeper than this fails with python's
/// RecursionError text. python's real limit is its interpreter recursion limit, so the exact depth differs
const MAX_JSON_DEPTH: usize = 1000;

/// python `json.loads(text)` on a str: the same accepted documents, the same error positions (in
/// characters) and the same `JSONDecodeError` text as CPython's scanner, returned as python's `str(e)`.
/// python's NaN, Infinity, -Infinity and out-of-range numbers are read as `true`: they are truthy and
/// not strings, and a non-dict top level is rejected for them as it is for `true`
pub fn py_json_loads(text: &str) -> Result<Value, String> {
    if text.starts_with('\u{feff}') {
        return Err("Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)".into());
    }
    let doc: Vec<char> = text.chars().collect();
    let scanner = JsonScanner { doc: &doc };
    let start = scanner.ws(0);
    let (value, end) = scanner.value(start, 0)?;
    let end = scanner.ws(end);
    if end != doc.len() {
        return Err(decode_error(&doc, "Extra data", end));
    }
    Ok(value)
}

/// python `RecursionError` text for a container nested deeper than `MAX_JSON_DEPTH`
fn recursion_error(kind: &str) -> String {
    format!("maximum recursion depth exceeded while decoding a JSON {kind} from a unicode string")
}

fn decode_error(doc: &[char], msg: &str, pos: usize) -> String {
    let lineno = doc[..pos].iter().filter(|&&c| c == '\n').count() + 1;
    let colno = match doc[..pos].iter().rposition(|&c| c == '\n') {
        Some(newline) => pos - newline,
        None => pos + 1,
    };
    format!("{msg}: line {lineno} column {colno} (char {pos})")
}

/// recursive-descent mirror of CPython's `_json` scanner. every method takes the index of the
/// first character it may consume and returns the index just past what it consumed
struct JsonScanner<'a> {
    doc: &'a [char],
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
    fn python_only(&self, end: usize) -> Result<(Value, usize), String> {
        Ok((Value::Bool(true), end))
    }

    fn value(&self, i: usize, depth: usize) -> Result<(Value, usize), String> {
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

    fn number(&self, start: usize) -> Result<(Value, usize), String> {
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
    fn string(&self, start: usize) -> Result<(String, usize), String> {
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
    fn hex4(&self, u_at: usize) -> Result<(u32, usize), String> {
        let unit = (1..=4).try_fold(0u32, |acc, k| {
            self.doc.get(u_at + k).and_then(|c| c.to_digit(16)).map(|v| acc * 16 + v)
        });
        match unit {
            Some(unit) => Ok((unit, u_at + 5)),
            None => Err(decode_error(self.doc, "Invalid \\uXXXX escape", u_at)),
        }
    }

    /// `start` is the index just past the `{`
    fn object(&self, start: usize, depth: usize) -> Result<(Value, usize), String> {
        if depth > MAX_JSON_DEPTH {
            return Err(recursion_error("object"));
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
    fn array(&self, start: usize, depth: usize) -> Result<(Value, usize), String> {
        if depth > MAX_JSON_DEPTH {
            return Err(recursion_error("array"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn isoformat_roundtrip() {
        let dt = fromisoformat("2024-01-15T10:30:00").unwrap();
        assert_eq!(iso(&dt), "2024-01-15T10:30:00");
        assert_eq!(iso(&fromisoformat("2024-01-15").unwrap()), "2024-01-15T00:00:00");
        assert_eq!(iso(&fromisoformat("2024-01-15 10:30").unwrap()), "2024-01-15T10:30:00");
        assert_eq!(iso(&fromisoformat("2024-01-15T10:30:00.123456").unwrap()), "2024-01-15T10:30:00.123456");
        assert_eq!(fromisoformat("nope").unwrap_err(), "Invalid isoformat string: 'nope'");
    }

    #[test]
    fn datetime_errors() {
        assert_eq!(py_datetime(2024, 13, 1, 0, 0).unwrap_err(), "month must be in 1..12");
        assert_eq!(py_datetime(2024, 2, 30, 0, 0).unwrap_err(), "day is out of range for month");
        assert_eq!(py_datetime(2024, 2, 29, 24, 0).unwrap_err(), "hour must be in 0..23");
        assert!(py_datetime(2024, 2, 29, 23, 59).is_ok());
    }

    #[test]
    fn ints_and_rounding() {
        assert_eq!(py_int(&json!(" 14 ")), Some(14));
        assert_eq!(py_int(&json!("1.5")), None);
        assert_eq!(py_int(&json!(2.9)), Some(2));
        let q = json!({"limit": "", "x": "abc", "y": "7"}).as_object().unwrap().clone();
        assert_eq!(query_int(&q, "limit", 100), 100);
        assert_eq!(query_int(&q, "x", 100), 100);
        assert_eq!(query_int(&q, "y", 100), 7);
        assert_eq!(round2(0.125), 0.12);
        assert_eq!(round2(1.0 / 3.0), 0.33);
    }

    #[test]
    fn py_strip_and_name_compare_like_python() {
        // \x1c..\x1f are isspace() in python, so str.strip() removes them
        assert_eq!(py_strip("\u{1c}\u{1f} Ann \u{85}"), "Ann");
        assert!(eq_ci_strip("\u{1c}Ann", "ann\u{1d}"));
        assert!(!eq_ci_strip("Ann", "Anna"));
    }

    #[test]
    fn base64_lenient() {
        assert_eq!(b64decode(b"aGVs\nbG8=").unwrap(), b"hello");
        // python rejects missing padding ("Incorrect padding") rather than decoding the partial quad
        assert_eq!(b64decode(b"aGVsbG8=").unwrap(), b"hello");
        assert_eq!(b64decode(b"aGVsbG8").unwrap_err(), "Incorrect padding");
    }

    #[test]
    fn base64_padding_matches_python_3_14() {
        // outputs captured from python 3.14 base64.b64decode
        assert_eq!(b64decode(b"QQ==").unwrap(), b"A");
        assert_eq!(b64decode(b"QUI=").unwrap(), b"AB");
        assert_eq!(b64decode(b"QQ==QQ==").unwrap(), b"A\x04\x10");
        assert_eq!(b64decode(b"QUFE=QUFE=").unwrap(), b"AADAAD");
        assert_eq!(b64decode(b"QUI=Q").unwrap(), b"AB\x10");
        assert_eq!(b64decode(b"QQ=").unwrap_err(), "Incorrect padding");
        assert_eq!(b64decode(b"QQ==Q").unwrap_err(), "Incorrect padding");
        assert_eq!(b64decode(b"QUI=QUI=").unwrap_err(), "Incorrect padding");
        assert_eq!(
            b64decode(b"QUFEF=").unwrap_err(),
            "Invalid base64-encoded string: number of data characters (5) cannot be 1 more than a multiple of 4"
        );
    }
}
