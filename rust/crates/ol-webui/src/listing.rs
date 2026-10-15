//! Chat list projection, search highlighting and day grouping/labels
//! (python `project_chat`, `highlight_query`, `day_label_from_key`, `_local_day`,
//! `_group_key`).

use crate::util::html_escape;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Utc};
use serde_json::{Map, Value};

/// the only chat fields list endpoints send
pub const CHAT_LIST_FIELDS: [&str; 8] = ["id", "title", "category", "tags", "created", "updated", "messages_found", "message_snippets"];

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

/// html-escaped text with case-insensitive query matches wrapped in <strong>
pub fn highlight_query(text: &str, query: &str) -> String {
    let escaped_text = html_escape(text);
    if query.is_empty() {
        return escaped_text;
    }
    let mut candidates = vec![query.trim().to_string()];
    candidates.extend(ol_core::search::normalize_words(query));
    let mut seen: Vec<String> = Vec::new();
    for c in candidates.into_iter().filter(|c| !c.is_empty()) {
        let p = regex::escape(&html_escape(&c));
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    if seen.is_empty() {
        return escaped_text;
    }
    let Ok(re) = regex::RegexBuilder::new(&format!("({})", seen.join("|"))).case_insensitive(true).build() else {
        return escaped_text;
    };
    re.replace_all(&escaped_text, |c: &regex::Captures| format!("<strong class=\"search-highlight\">{}</strong>", &c[1])).into_owned()
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
    let year = crate::util::py_int_str(parts.first()?)?;
    let month = crate::util::py_int_str(parts.get(1)?)?;
    let day = if parts.len() > 2 { crate::util::py_int_str(parts[2])? } else { 1 };
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || day < 1 {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)?;
    Some((date, parts.len()))
}

/// human label for a day/week/month group key (see group_key)
pub fn day_label_from_key_at(key: &str, tz_offset_min: i64, now: NaiveDateTime) -> String {
    if key.is_empty() {
        return "Undated".into();
    }
    if key.starts_with("last-week") {
        return "Last Week".into();
    }
    let Some((date, nparts)) = parse_key_date(key) else { return "Undated".into() };
    if nparts == 2 {
        return format!("{} {}", month_name(date.month()), date.year());
    }
    let today = (now - Duration::minutes(tz_offset_min)).date();
    let diff_days = (today - date).num_days();
    if diff_days == 0 {
        return "Today".into();
    }
    if diff_days == 1 {
        return "Yesterday".into();
    }
    if 1 < diff_days && diff_days < 7 {
        return date.format("%A").to_string();
    }
    if date.year() == today.year() {
        return format!("{} {}", month_name(date.month()), date.day());
    }
    format!("{} {}, {}", month_name(date.month()), date.day(), date.year())
}

pub fn day_label_from_key(key: &str, tz_offset_min: i64) -> String {
    day_label_from_key_at(key, tz_offset_min, utcnow())
}

/// python `datetime.fromisoformat` (naive result; aware values converted to naive UTC)
pub fn parse_iso(s: &str) -> Option<NaiveDateTime> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.naive_utc());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M%:z", "%Y-%m-%d %H:%M%:z"] {
        if let Ok(dt) = chrono::DateTime::parse_from_str(s, fmt) {
            return Some(dt.naive_utc());
        }
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H", "%Y-%m-%d %H"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt);
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0);
    }
    if s.len() == 8 {
        if let Ok(d) = NaiveDate::parse_from_str(s, "%Y%m%d") {
            return d.and_hms_opt(0, 0, 0);
        }
    }
    None
}

/// naive UTC isoformat -> browser-local date string 'YYYY-MM-DD' ('' if unparseable)
pub fn local_day(updated: &str, tz_offset_min: i64) -> String {
    if updated.is_empty() {
        return String::new();
    }
    let Some(dt) = parse_iso(updated) else { return String::new() };
    (dt - Duration::minutes(tz_offset_min)).date().format("%Y-%m-%d").to_string()
}

/// sidebar grouping tiers (ISO weeks, Monday start): this week -> 'YYYY-MM-DD',
/// last week -> 'last-week-<monday>', older -> 'YYYY-MM'
pub fn group_key_at(updated: &str, tz_offset_min: i64, now: NaiveDateTime) -> String {
    let day = local_day(updated, tz_offset_min);
    if day.is_empty() {
        return String::new();
    }
    let today = (now - Duration::minutes(tz_offset_min)).date();
    let this_monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let last_monday = this_monday - Duration::days(7);
    let iso = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
    if day >= iso(this_monday) {
        return day;
    }
    if day >= iso(last_monday) {
        return format!("last-week-{}", iso(last_monday));
    }
    day.chars().take(7).collect()
}

pub fn group_key(updated: &str, tz_offset_min: i64) -> String {
    group_key_at(updated, tz_offset_min, utcnow())
}

/// `chat.get("updated", "")` as a string
pub fn updated_of(chat: &Value) -> String {
    match chat.get("updated") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
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
        assert_eq!(highlight_query("Hello World", "world"), "Hello <strong class=\"search-highlight\">World</strong>");
        assert_eq!(highlight_query("a <b> & c", ""), "a &lt;b&gt; &amp; c");
        assert_eq!(
            highlight_query("fish & chips", "fish & chips"),
            "<strong class=\"search-highlight\">fish &amp; chips</strong>"
        );
        // individual tokens are highlighted too
        let h = highlight_query("the quick brown fox", "fox quick");
        assert!(h.contains("<strong class=\"search-highlight\">quick</strong>"));
        assert!(h.contains("<strong class=\"search-highlight\">fox</strong>"));
    }

    #[test]
    fn labels() {
        // 2026-10-14 is a Wednesday
        let now = at("2026-10-14T12:00:00");
        assert_eq!(day_label_from_key_at("", 0, now), "Undated");
        assert_eq!(day_label_from_key_at("last-week-2026-10-05", 0, now), "Last Week");
        assert_eq!(day_label_from_key_at("2026-10-14", 0, now), "Today");
        assert_eq!(day_label_from_key_at("2026-10-13", 0, now), "Yesterday");
        assert_eq!(day_label_from_key_at("2026-10-12", 0, now), "Monday");
        assert_eq!(day_label_from_key_at("2026-10-01", 0, now), "October 1");
        assert_eq!(day_label_from_key_at("2025-12-24", 0, now), "December 24, 2025");
        assert_eq!(day_label_from_key_at("2026-09", 0, now), "September 2026");
        assert_eq!(day_label_from_key_at("garbage", 0, now), "Undated");
        assert_eq!(day_label_from_key_at("2026-13-01", 0, now), "Undated");
        // tz offset shifts "today": at 01:00 UTC with UTC-5 (offset +300) it is still the 13th locally
        let early = at("2026-10-14T01:00:00");
        assert_eq!(day_label_from_key_at("2026-10-13", 300, early), "Today");
    }

    #[test]
    fn grouping() {
        let now = at("2026-10-14T12:00:00");
        assert_eq!(group_key_at("2026-10-14T08:00:00.123456", 0, now), "2026-10-14");
        assert_eq!(group_key_at("2026-10-12T00:00:00", 0, now), "2026-10-12");
        assert_eq!(group_key_at("2026-10-11T23:00:00", 0, now), "last-week-2026-10-05");
        assert_eq!(group_key_at("2026-10-05T00:00:00", 0, now), "last-week-2026-10-05");
        assert_eq!(group_key_at("2026-10-04T23:59:59", 0, now), "2026-10");
        assert_eq!(group_key_at("2026-03-01T00:00:00", 0, now), "2026-03");
        assert_eq!(group_key_at("", 0, now), "");
        assert_eq!(group_key_at("nope", 0, now), "");
        // aware timestamps are converted to utc first
        assert_eq!(local_day("2026-10-14T01:00:00+02:00", 0), "2026-10-13");
        assert_eq!(local_day("2026-10-14T01:00:00", 120), "2026-10-13");
        assert_eq!(local_day("2026-10-14", 0), "2026-10-14");
    }
}
