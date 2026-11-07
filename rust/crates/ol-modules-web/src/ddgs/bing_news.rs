//! `engines/bing_news.py`: news results from bing.com/news/infinitescrollajax (html scraping).

use super::html::{self, all_elements, attr_concat, class_has, class_is, desc_where, has_attr, named, text_of, ancestor_below};
use super::http::{Method, Net, Session};
use super::results::{build, collapse_ws, iso_utc, set, Record};
use super::{kv, lookup, region_parts, Category, DdgsError, Payload, SearchArgs};
use chrono::{Duration, Local, NaiveDate, TimeZone, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use scraper::ElementRef;
use serde_json::Value;

const URL: &str = "https://www.bing.com/news/infinitescrollajax";

const QFT: &[(&str, &str)] = &[
    ("d", "interval=\"4\""),
    ("w", "interval=\"7\""),
    ("m", "interval=\"9\""),
    ("y", "interval=\"9\""),
];

/// `DATE_RE`: a number followed by an optional unit word
static DATE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(\d+)\s*(days|tagen|jours|giorni|dias|días|дн\.|день)?\b").unwrap());

/// `build_payload`: q, InfiniteScroll, first, SFX, cc, setlang, then qft (timelimit)
pub fn build_payload(query: &str, args: &SearchArgs) -> Result<Payload, DdgsError> {
    let (country, lang) = region_parts(&args.region)?;
    let mut payload = vec![
        kv("q", query),
        kv("InfiniteScroll", "1"),
        kv("first", args.page * 10 + 1),
        kv("SFX", args.page),
        kv("cc", country),
        kv("setlang", lang),
    ];
    if let Some(tl) = args.timelimit() {
        payload.push(kv("qft", lookup(QFT, tl)?));
    }
    Ok(payload)
}

/// `extract_date(pub_date_str)`: absolute dates (naive, taken in local time) or "N days ago"
pub fn extract_date(text: &str) -> Result<String, DdgsError> {
    for fmt in ["%d.%m.%Y", "%m/%d/%Y", "%d/%m/%Y"] {
        if let Ok(day) = NaiveDate::parse_from_str(text, fmt) {
            let naive = day
                .and_hms_opt(0, 0, 0)
                .ok_or_else(|| DdgsError::Failed("invalid time".into()))?;
            let local = Local
                .from_local_datetime(&naive)
                .earliest()
                .ok_or_else(|| DdgsError::Failed("ambiguous local time".into()))?;
            return Ok(iso_utc(local.with_timezone(&Utc)));
        }
    }
    if let Some(caps) = DATE_RE.captures(text) {
        let days: i64 = caps[1].parse().map_err(|e| DdgsError::Failed(format!("{e}")))?;
        let delta = Duration::try_days(days).ok_or_else(|| DdgsError::Failed("date out of range".into()))?;
        let when = Utc::now().checked_sub_signed(delta).ok_or_else(|| DdgsError::Failed("date out of range".into()))?;
        return Ok(iso_utc(when));
    }
    Ok(text.to_string())
}

/// `.//span[@aria-label]//@aria-label`: aria-label of a span (or of anything inside one)
fn in_aria_span(e: ElementRef<'_>, item: ElementRef<'_>) -> bool {
    let aria_span = |x: ElementRef<'_>| named(x, "span") && has_attr(x, "aria-label");
    (aria_span(e) || ancestor_below(e, item, aria_span)) && has_attr(e, "aria-label")
}

/// `.//a[contains(@class,'image')]//@src`: src of an image link (or of anything inside one)
fn in_image_link(e: ElementRef<'_>, item: ElementRef<'_>) -> bool {
    let link = |x: ElementRef<'_>| named(x, "a") && class_has(x, "image");
    (link(e) || ancestor_below(e, item, link)) && has_attr(e, "src")
}

/// `extract_results` (BaseSearchEngine path: whitespace-collapsed xpath text for every field)
pub fn extract(html_text: &str) -> Result<Vec<Record>, DdgsError> {
    let doc = html::parse(html_text);
    let mut out = Vec::new();
    for item in all_elements(&doc).into_iter().filter(|e| named(*e, "div") && class_has(*e, "newsitem")) {
        let date = attr_concat(&desc_where(item, |e| in_aria_span(e, item)), "aria-label");
        let title = item.value().attr("data-title").unwrap_or("");
        let body = text_of(&desc_where(item, |e| named(e, "div") && class_is(e, "snippet")));
        let url = item.value().attr("url").unwrap_or("");
        let image = attr_concat(&desc_where(item, |e| in_image_link(e, item)), "src");
        let source = item.value().attr("data-author").unwrap_or("");
        out.push(build(
            Category::News,
            vec![
                ("date", Value::String(collapse_ws(&date))),
                ("title", Value::String(collapse_ws(title))),
                ("body", Value::String(collapse_ws(&body))),
                ("url", Value::String(collapse_ws(url))),
                ("image", Value::String(collapse_ws(&image))),
                ("source", Value::String(collapse_ws(source))),
            ],
        )?);
    }
    Ok(out)
}

/// `post_extract_results`: ISO dates and absolute image links
pub fn post(mut results: Vec<Record>) -> Result<Vec<Record>, DdgsError> {
    for rec in results.iter_mut() {
        let date = rec.get("date").and_then(Value::as_str).unwrap_or("").to_string();
        set(rec, "date", Value::String(extract_date(&date)?))?;
        let image = rec.get("image").and_then(Value::as_str).unwrap_or("").to_string();
        let new_image = if image.is_empty() {
            String::new()
        } else {
            format!("https://www.bing.com{}", image.split('&').next().unwrap_or(""))
        };
        set(rec, "image", Value::String(new_image))?;
    }
    Ok(results)
}

pub async fn search(net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    let payload = build_payload(query, args)?;
    let mut session = Session::new(net, &[])?;
    let Some(text) = session.request_text(Method::Get, URL, &payload).await? else {
        return Ok(Vec::new());
    };
    post(extract(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_has_bing_paging_and_interval_filters() {
        let mut args = SearchArgs { page: 2, ..Default::default() };
        let p = build_payload("storm", &args).unwrap();
        assert_eq!(
            p,
            vec![
                kv("q", "storm"),
                kv("InfiniteScroll", "1"),
                kv("first", 21),
                kv("SFX", 2),
                kv("cc", "us"),
                kv("setlang", "en"),
            ]
        );
        args.timelimit = Some("w".into());
        assert!(build_payload("storm", &args).unwrap().contains(&kv("qft", "interval=\"7\"")));
    }

    #[test]
    fn absolute_and_relative_dates() {
        // a naive midnight in local time, converted to UTC
        let expected = Local.from_local_datetime(&NaiveDate::from_ymd_opt(2026, 2, 1).unwrap().and_hms_opt(0, 0, 0).unwrap()).earliest().unwrap();
        assert_eq!(extract_date("02/01/2026").unwrap(), iso_utc(expected.with_timezone(&Utc)));
        assert_eq!(extract_date("not a date").unwrap(), "not a date");
        // "3 days ago" is three days before now, whole seconds
        let rel = extract_date("3 days ago").unwrap();
        let parsed = chrono::DateTime::parse_from_str(&rel, "%Y-%m-%dT%H:%M:%S%:z").unwrap();
        let drift = (Utc::now() - Duration::days(3) - parsed.with_timezone(&Utc)).num_seconds().abs();
        assert!(drift < 60, "drift {drift}");
    }

    #[test]
    fn extracts_items_and_normalises_image_link() {
        let page = r#"<div class="newsitem" data-title="Storm  hits" url="https://news.test/a" data-author="Wire">
            <span aria-label="2 hours ago">2h</span>
            <div class="snippet"><div>Winds <b>rise</b></div></div>
            <a class="image" href="x"><img src="/th?id=1&amp;w=2"></a></div>"#;
        let out = post(extract(page).unwrap()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["title"], serde_json::json!("Storm hits"));
        assert_eq!(out[0]["body"], serde_json::json!("Winds rise"));
        assert_eq!(out[0]["source"], serde_json::json!("Wire"));
        assert_eq!(out[0]["image"], serde_json::json!("https://www.bing.com/th?id=1"));
        assert!(out[0]["date"].as_str().unwrap().ends_with("+00:00"));
    }
}
