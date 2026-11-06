//! Chat list projection, search highlighting, day grouping/labels and category helpers
//! (python `project_chat`, `highlight_query`, `day_label_from_key`, `_local_day`,
//! `_group_key`, `_chats_for_category`, `validate_category_format`/`_name`).

use crate::util::{html_escape, py_int_str, py_strip, py_type_name};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Utc};
use ol_core::pyjson::truthy;
use serde_json::{Map, Value};

/// the only chat fields list endpoints send
pub const CHAT_LIST_FIELDS: [&str; 8] = ["id", "title", "category", "tags", "created", "updated", "messages_found", "message_snippets"];

/// python raises OverflowError (uncaught, so the route fails) for out-of-range
/// `timedelta(minutes=tz_offset)` arithmetic; the message is python's
pub const OVERFLOW: &str = "OverflowError: date value out of range";

pub fn project_chat(chat: &Value, extra_fields: &[&str]) -> Value {
    let mut projected = Map::new();
    let Value::Object(chat) = chat else { return Value::Object(projected) };
    for key in CHAT_LIST_FIELDS.iter().chain(extra_fields.iter()) {
        if let Some(v) = chat.get(*key) {
            projected.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(projected)
}

/// python `highlight_query`: `None` text gives `None` (JSON null); otherwise html-escaped text
/// with case-insensitive matches of the stripped query and its normalized tokens in <strong>
pub fn highlight_query(text: Option<&str>, query: &str) -> Option<String> {
    let text = text?;
    let escaped_text = html_escape(text);
    if query.is_empty() {
        return Some(escaped_text);
    }
    let mut candidates = vec![py_strip(query).to_string()];
    candidates.extend(ol_core::search::normalize_words(query));
    let mut seen: Vec<String> = Vec::new();
    for c in candidates.into_iter().filter(|c| !c.is_empty()) {
        let p = regex::escape(&html_escape(&c));
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    if seen.is_empty() {
        return Some(escaped_text);
    }
    let Ok(re) = regex::RegexBuilder::new(&format!("({})", seen.join("|"))).case_insensitive(true).build() else {
        return Some(escaped_text);
    };
    Some(re.replace_all(&escaped_text, |c: &regex::Captures| format!("<strong class=\"search-highlight\">{}</strong>", &c[1])).into_owned())
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn month_name(m: u32) -> &'static str {
    MONTHS[(m as usize).saturating_sub(1).min(11)]
}

/// python `datetime.datetime.utcnow()`
pub fn utcnow() -> NaiveDateTime {
    Utc::now().naive_utc()
}

/// python `datetime.date(year, month, day)` with int() parsing of the parts
fn parse_key_date(key: &str) -> Option<(NaiveDate, usize)> {
    let parts: Vec<&str> = key.split('-').collect();
    let year = py_int_str(parts.first()?)?;
    let month = py_int_str(parts.get(1)?)?;
    let day = if parts.len() > 2 { py_int_str(parts[2])? } else { 1 };
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || day < 1 {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)?;
    Some((date, parts.len()))
}

/// python datetimes only span years 1..=9999; chrono's range is wider, so check it explicitly
fn in_python_range(dt: NaiveDateTime) -> Result<NaiveDateTime, String> {
    if (1..=9999).contains(&dt.year()) {
        Ok(dt)
    } else {
        Err(OVERFLOW.to_string())
    }
}

/// `dt - tz_offset_min minutes` (python `dt - timedelta(minutes=...)`)
fn shift_back(dt: NaiveDateTime, tz_offset_min: i64) -> Result<NaiveDateTime, String> {
    Duration::try_minutes(tz_offset_min)
        .and_then(|d| dt.checked_sub_signed(d))
        .ok_or_else(|| OVERFLOW.to_string())
        .and_then(in_python_range)
}

/// human label for a day/week/month group key (see group_key); Err where python raises
pub fn day_label_from_key_at(key: &str, tz_offset_min: i64, now: NaiveDateTime) -> Result<String, String> {
    if key.is_empty() {
        return Ok("Undated".into());
    }
    if key.starts_with("last-week") {
        return Ok("Last Week".into());
    }
    let Some((date, nparts)) = parse_key_date(key) else { return Ok("Undated".into()) };
    if nparts == 2 {
        return Ok(format!("{} {}", month_name(date.month()), date.year()));
    }
    let today = shift_back(now, tz_offset_min)?.date();
    let diff_days = (today - date).num_days();
    if diff_days == 0 {
        return Ok("Today".into());
    }
    if diff_days == 1 {
        return Ok("Yesterday".into());
    }
    if 1 < diff_days && diff_days < 7 {
        return Ok(date.format("%A").to_string());
    }
    if date.year() == today.year() {
        return Ok(format!("{} {}", month_name(date.month()), date.day()));
    }
    Ok(format!("{} {}, {}", month_name(date.month()), date.day(), date.year()))
}

pub fn day_label_from_key(key: &str, tz_offset_min: i64) -> Result<String, String> {
    day_label_from_key_at(key, tz_offset_min, utcnow())
}

fn digits_exact(s: &str, len: usize) -> Option<u32> {
    if s.len() != len || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// time component of python's `fromisoformat`: `HH[:MM[:SS[.f+]]]` or basic `HH[MM[SS[.f+]]]`.
/// returns (hour, minute, second, microsecond); fractions beyond 6 digits are ignored
fn parse_time(t: &str) -> Option<(u32, u32, u32, u32)> {
    if !t.is_ascii() {
        return None;
    }
    let (main, frac) = match t.split_once('.') {
        Some((m, f)) => (m, Some(f)),
        None => (t, None),
    };
    let (h, m, s) = if main.contains(':') {
        let parts: Vec<&str> = main.split(':').collect();
        if parts.len() > 3 {
            return None;
        }
        (parts[0], parts.get(1).copied(), parts.get(2).copied())
    } else {
        match main.len() {
            2 => (main, None, None),
            4 => (&main[..2], Some(&main[2..]), None),
            6 => (&main[..2], Some(&main[2..4]), Some(&main[4..])),
            _ => return None,
        }
    };
    let h = digits_exact(h, 2)?;
    let m = match m {
        Some(x) => digits_exact(x, 2)?,
        None => 0,
    };
    let s = match s {
        Some(x) => digits_exact(x, 2)?,
        None => 0,
    };
    let us = match frac {
        None => 0,
        Some(f) => {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut six: String = f.chars().take(6).collect();
            while six.len() < 6 {
                six.push('0');
            }
            six.parse().ok()?
        }
    };
    Some((h, m, s, us))
}

/// python `datetime.fromisoformat` (3.11+): `YYYY-MM-DD` or `YYYYMMDD`, optionally followed by
/// any single separator char and a time with an optional offset (`Z`, `+HH`, `+HHMM`, `+HH:MM`,
/// `+HH:MM:SS[.ffffff]`). Returns the naive UTC value (aware values are converted, as
/// `_local_day` does). Ok(None) for unparseable input, Err where the conversion overflows
pub fn parse_iso(s: &str) -> Result<Option<NaiveDateTime>, String> {
    if !s.is_ascii() {
        return Ok(None);
    }
    let (y, m, d, rest) = if s.len() >= 10 && &s[4..5] == "-" && &s[7..8] == "-" {
        (digits_exact(&s[0..4], 4), digits_exact(&s[5..7], 2), digits_exact(&s[8..10], 2), &s[10..])
    } else if s.len() >= 8 {
        (digits_exact(&s[0..4], 4), digits_exact(&s[4..6], 2), digits_exact(&s[6..8], 2), &s[8..])
    } else {
        return Ok(None);
    };
    let (Some(y), Some(m), Some(d)) = (y, m, d) else { return Ok(None) };
    let Some(date) = NaiveDate::from_ymd_opt(y as i32, m, d) else { return Ok(None) };
    if rest.is_empty() {
        return Ok(date.and_hms_opt(0, 0, 0));
    }
    // python accepts any single character between the date and the time
    let time_and_tz = &rest[1..];
    let (time_part, offset) = if let Some(stripped) = time_and_tz.strip_suffix('Z') {
        (stripped, Some("Z"))
    } else {
        match time_and_tz.find('-').or_else(|| time_and_tz.find('+')) {
            Some(i) => (&time_and_tz[..i], Some(&time_and_tz[i..])),
            None => (time_and_tz, None),
        }
    };
    let Some((h, mi, sec, us)) = parse_time(time_part) else { return Ok(None) };
    let Some(naive) = date.and_hms_micro_opt(h, mi, sec, us) else { return Ok(None) };
    let Some(offset) = offset else { return Ok(Some(naive)) };
    let offset_us: i64 = if offset == "Z" {
        0
    } else {
        let sign: i64 = if offset.starts_with('-') { -1 } else { 1 };
        let Some((oh, om, os, ous)) = parse_time(&offset[1..]) else { return Ok(None) };
        if oh >= 24 || om >= 60 || os >= 60 {
            return Ok(None);
        }
        sign * (((oh * 3600 + om * 60 + os) as i64) * 1_000_000 + ous as i64)
    };
    naive
        .checked_sub_signed(Duration::microseconds(offset_us))
        .ok_or_else(|| OVERFLOW.to_string())
        .and_then(in_python_range)
        .map(Some)
}

/// naive UTC isoformat -> browser-local date string 'YYYY-MM-DD' ('' if unparseable)
pub fn local_day(updated: &str, tz_offset_min: i64) -> Result<String, String> {
    if updated.is_empty() {
        return Ok(String::new());
    }
    let Some(dt) = parse_iso(updated)? else { return Ok(String::new()) };
    Ok(shift_back(dt, tz_offset_min)?.date().format("%Y-%m-%d").to_string())
}

/// sidebar grouping tiers (ISO weeks, Monday start): this week -> 'YYYY-MM-DD',
/// last week -> 'last-week-<monday>', older -> 'YYYY-MM'
pub fn group_key_at(updated: &str, tz_offset_min: i64, now: NaiveDateTime) -> Result<String, String> {
    let day = local_day(updated, tz_offset_min)?;
    if day.is_empty() {
        return Ok(String::new());
    }
    let today = shift_back(now, tz_offset_min)?.date();
    let overflow = || OVERFLOW.to_string();
    let in_range = |d: &NaiveDate| (1..=9999).contains(&d.year());
    let this_monday = today
        .checked_sub_signed(Duration::days(today.weekday().num_days_from_monday() as i64))
        .filter(in_range)
        .ok_or_else(overflow)?;
    let last_monday = this_monday.checked_sub_signed(Duration::days(7)).filter(in_range).ok_or_else(overflow)?;
    let iso = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
    if day >= iso(this_monday) {
        return Ok(day);
    }
    if day >= iso(last_monday) {
        return Ok(format!("last-week-{}", iso(last_monday)));
    }
    Ok(day.chars().take(7).collect())
}

pub fn group_key(updated: &str, tz_offset_min: i64) -> Result<String, String> {
    group_key_at(updated, tz_offset_min, utcnow())
}

/// `chat.get("updated", "")` as a string
pub fn updated_of(chat: &Value) -> String {
    match chat.get("updated") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// python `item in (chat.get("tags") or [])`
fn tag_contains(tags: Option<&Value>, tag: &str) -> Result<bool, String> {
    match tags {
        None => Ok(false),
        Some(v) if !truthy(v) => Ok(false),
        Some(Value::Array(items)) => Ok(items.iter().any(|x| x.as_str() == Some(tag))),
        Some(Value::String(s)) => Ok(s.contains(tag)),
        Some(Value::Object(m)) => Ok(m.contains_key(tag)),
        Some(other) => Err(format!("argument of type '{}' is not iterable", py_type_name(other))),
    }
}

/// python `_chats_for_category`: `category` must be equal (an empty string means no filter),
/// and with tags every tag must be contained in the chat's tags (AND)
pub fn chats_for_category<'a>(chats: &'a [Value], category: Option<&str>, tags: &[String]) -> Result<Vec<&'a Value>, String> {
    let category = category.filter(|c| !c.is_empty());
    let mut out = Vec::new();
    'chats: for chat in chats {
        if let Some(cat) = category {
            if chat.get("category").and_then(Value::as_str) != Some(cat) {
                continue;
            }
        }
        for tag in tags {
            if !tag_contains(chat.get("tags"), tag)? {
                continue 'chats;
            }
        }
        out.push(chat);
    }
    Ok(out)
}

/// python `validate_category_format`: stripped and lowercased; Err with python's message
pub fn validate_category_format(name: &str) -> Result<String, String> {
    let name = py_strip(name).to_lowercase();
    if name.is_empty() {
        return Err("category name cannot be empty".into());
    }
    if name.contains(':') {
        return Err("':' is reserved for subcategories".into());
    }
    Ok(name)
}

/// python `validate_category_name`: format validation, then the duplicate check against
/// every chat's category (a missing category counts as 'general')
pub fn validate_category_name(name: &str, chats: &[Value]) -> Result<String, String> {
    let name = validate_category_format(name)?;
    let existing: Vec<String> = chats
        .iter()
        .map(|chat| match chat.get("category") {
            Some(v) if truthy(v) => match v {
                Value::String(s) => Ok(s.to_lowercase()),
                other => Err(format!("'{}' object has no attribute 'lower'", py_type_name(other))),
            },
            _ => Ok("general".to_string()),
        })
        .collect::<Result<_, _>>()?;
    if existing.contains(&name) {
        return Err(format!("'{}' already exists", name));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    #[test]
    fn projection() {
        let chat = json!({"id": "a", "metadata": {}, "title": "t", "updated": "u", "title_match": true, "tags": []});
        assert_eq!(project_chat(&chat, &[]), json!({"id": "a", "title": "t", "tags": [], "updated": "u"}));
        let p = project_chat(&chat, &["title_match"]);
        let keys: Vec<&String> = p.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["id", "title", "tags", "updated", "title_match"]);
    }

    #[test]
    fn highlighting() {
        assert_eq!(highlight_query(Some("Hello World"), "world").unwrap(), "Hello <strong class=\"search-highlight\">World</strong>");
        assert_eq!(highlight_query(Some("a <b> & c"), "").unwrap(), "a &lt;b&gt; &amp; c");
        assert_eq!(
            highlight_query(Some("fish & chips"), "fish & chips").unwrap(),
            "<strong class=\"search-highlight\">fish &amp; chips</strong>"
        );
        // individual tokens are highlighted too
        let h = highlight_query(Some("the quick brown fox"), "fox quick").unwrap();
        assert!(h.contains("<strong class=\"search-highlight\">quick</strong>"));
        assert!(h.contains("<strong class=\"search-highlight\">fox</strong>"));
        // python returns None (JSON null) for a None snippet
        assert_eq!(highlight_query(None, "fox"), None);
        // the query is stripped first (python str(query).strip())
        assert_eq!(highlight_query(Some("a b"), "  b  ").unwrap(), "a <strong class=\"search-highlight\">b</strong>");
    }

    #[test]
    fn labels() {
        // 2026-10-14 is a Wednesday
        let now = at("2026-10-14T12:00:00");
        assert_eq!(day_label_from_key_at("", 0, now).unwrap(), "Undated");
        assert_eq!(day_label_from_key_at("last-week-2026-10-05", 0, now).unwrap(), "Last Week");
        assert_eq!(day_label_from_key_at("2026-10-14", 0, now).unwrap(), "Today");
        assert_eq!(day_label_from_key_at("2026-10-13", 0, now).unwrap(), "Yesterday");
        assert_eq!(day_label_from_key_at("2026-10-12", 0, now).unwrap(), "Monday");
        assert_eq!(day_label_from_key_at("2026-10-01", 0, now).unwrap(), "October 1");
        assert_eq!(day_label_from_key_at("2025-12-24", 0, now).unwrap(), "December 24, 2025");
        assert_eq!(day_label_from_key_at("2026-09", 0, now).unwrap(), "September 2026");
        assert_eq!(day_label_from_key_at("garbage", 0, now).unwrap(), "Undated");
        assert_eq!(day_label_from_key_at("2026-13-01", 0, now).unwrap(), "Undated");
        // tz offset shifts "today": at 01:00 UTC with UTC-5 (offset +300) it is still the 13th locally
        let early = at("2026-10-14T01:00:00");
        assert_eq!(day_label_from_key_at("2026-10-13", 300, early).unwrap(), "Today");
    }

    #[test]
    fn grouping() {
        let now = at("2026-10-14T12:00:00");
        assert_eq!(group_key_at("2026-10-14T08:00:00.123456", 0, now).unwrap(), "2026-10-14");
        assert_eq!(group_key_at("2026-10-12T00:00:00", 0, now).unwrap(), "2026-10-12");
        assert_eq!(group_key_at("2026-10-11T23:00:00", 0, now).unwrap(), "last-week-2026-10-05");
        assert_eq!(group_key_at("2026-10-05T00:00:00", 0, now).unwrap(), "last-week-2026-10-05");
        assert_eq!(group_key_at("2026-10-04T23:59:59", 0, now).unwrap(), "2026-10");
        assert_eq!(group_key_at("2026-03-01T00:00:00", 0, now).unwrap(), "2026-03");
        assert_eq!(group_key_at("", 0, now).unwrap(), "");
        assert_eq!(group_key_at("nope", 0, now).unwrap(), "");
        // aware timestamps are converted to utc first
        assert_eq!(local_day("2026-10-14T01:00:00+02:00", 0).unwrap(), "2026-10-13");
        assert_eq!(local_day("2026-10-14T01:00:00", 120).unwrap(), "2026-10-13");
        assert_eq!(local_day("2026-10-14", 0).unwrap(), "2026-10-14");
    }

    #[test]
    fn python_iso_offsets() {
        // python 3.11+ fromisoformat accepts these offset spellings
        let want = Some(at("2026-10-13T23:00:00"));
        assert_eq!(parse_iso("2026-10-14T01:00:00+0200").unwrap(), want);
        assert_eq!(parse_iso("2026-10-14T01:00:00+02:00").unwrap(), want);
        assert_eq!(parse_iso("2026-10-14T01:00:00+02").unwrap(), want);
        assert_eq!(parse_iso("2026-10-14T01:00:00-0000").unwrap(), Some(at("2026-10-14T01:00:00")));
        assert_eq!(parse_iso("2026-10-14T01:00:00Z").unwrap(), Some(at("2026-10-14T01:00:00")));
        assert_eq!(parse_iso("2026-10-14T03:00:30+02:00:30").unwrap(), Some(at("2026-10-14T01:00:00")));
        assert_eq!(parse_iso("2026-10-14T01:00:00-05").unwrap(), Some(at("2026-10-14T06:00:00")));
        assert_eq!(local_day("2026-10-14T01:00:00+0200", 0).unwrap(), "2026-10-13");
        // malformed input is not parseable (python ValueError -> '')
        assert_eq!(parse_iso("2026-10-14T01:00:00+25").unwrap(), None);
        assert_eq!(parse_iso("2026-10-14T01:00:00+").unwrap(), None);
        assert_eq!(parse_iso("2026-10-14 ").unwrap(), None);
    }

    #[test]
    fn hostile_tz_offset_overflows_like_python() {
        // tz_offset comes from the query string: python raises OverflowError (route 500)
        let now = at("2026-10-14T12:00:00");
        assert!(local_day("2026-10-14T01:00:00", i64::MAX).is_err());
        assert!(group_key_at("2026-10-14T01:00:00", i64::MAX, now).is_err());
        assert!(day_label_from_key_at("2026-10-14", i64::MIN, now).is_err());
        // a naive value near the year-1 edge overflows when shifted, an aware one when converted
        assert!(local_day("0001-01-01T00:30:00", 60).is_err());
        assert!(parse_iso("0001-01-01T00:30:00+01:00").is_err());
        // last monday would be in year 0: python raises OverflowError there too
        assert!(group_key_at("0001-01-03T00:30:00", 0, at("0001-01-02T12:00:00")).is_err());
        // a sane offset still works
        assert_eq!(day_label_from_key_at("2026-10-14", 0, now).unwrap(), "Today");
    }

    #[test]
    fn category_filter_and_validation() {
        let chats = vec![
            json!({"id": "a", "category": "work", "tags": ["x", "y"]}),
            json!({"id": "b", "category": "work", "tags": ["x"]}),
            json!({"id": "c", "tags": ["y"]}),
        ];
        let ids = |v: Vec<&Value>| v.iter().map(|c| c["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(ids(chats_for_category(&chats, Some("work"), &[]).unwrap()), ["a", "b"]);
        assert_eq!(ids(chats_for_category(&chats, Some(""), &[]).unwrap()), ["a", "b", "c"]);
        // AND semantics over tags
        assert_eq!(ids(chats_for_category(&chats, None, &["x".into(), "y".into()]).unwrap()), ["a"]);
        assert_eq!(validate_category_format("  Work ").unwrap(), "work");
        assert_eq!(validate_category_format(" \u{1c} ").unwrap_err(), "category name cannot be empty");
        assert_eq!(validate_category_format("a:b").unwrap_err(), "':' is reserved for subcategories");
        assert_eq!(validate_category_name("WORK", &chats).unwrap_err(), "'work' already exists");
        assert_eq!(validate_category_name("misc", &chats).unwrap(), "misc");
        // a chat without a category counts as 'general'
        let no_cat = vec![json!({"id": "d"})];
        assert_eq!(validate_category_name("General", &no_cat).unwrap_err(), "'general' already exists");
    }
}
