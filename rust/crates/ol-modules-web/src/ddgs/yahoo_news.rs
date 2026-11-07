//! `engines/yahoo_news.py`: news results from news.search.yahoo.com (html scraping).

use super::html::{self, all_elements, attr_concat, class_has, desc_named, desc_where, has_ancestor, named, text_of};
use super::http::{Method, Net, Session};
use super::results::{build, collapse_ws, iso_utc, set, Record};
use super::{kv, Category, DdgsError, Payload, SearchArgs};
use crate::pytext::url_unquote;
use chrono::{Duration, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use scraper::ElementRef;
use serde_json::Value;

const URL: &str = "https://news.search.yahoo.com/search";

/// `DATE_RE`: "<n> <unit>s?"
static DATE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(\d+)\s*(year|month|week|day|hour|minute)s?\b").unwrap());

/// `build_payload`: p, then b (page > 1) and btf (timelimit)
pub fn build_payload(query: &str, args: &SearchArgs) -> Payload {
    let mut payload = vec![kv("p", query)];
    if args.page > 1 {
        payload.push(kv("b", (args.page - 1) * 10 + 1));
    }
    if let Some(tl) = args.timelimit() {
        payload.push(kv("btf", tl));
    }
    payload
}

/// `extract_date`: "N units ago" relative to now (whole seconds), other strings unchanged
pub fn extract_date(text: &str) -> Result<String, DdgsError> {
    let Some(caps) = DATE_RE.captures(text) else {
        return Ok(text.to_string());
    };
    let number: i64 = caps[1].parse().map_err(|e| DdgsError::Failed(format!("{e}")))?;
    let unit = caps[2].to_lowercase();
    let delta = match unit.as_str() {
        "minute" => Duration::try_minutes(number),
        "hour" => Duration::try_hours(number),
        "day" => Duration::try_days(number),
        "week" => Duration::try_weeks(number),
        "month" => Duration::try_days(number.checked_mul(30).unwrap_or(i64::MAX)),
        _ => Duration::try_days(number.checked_mul(365).unwrap_or(i64::MAX)),
    }
    .ok_or_else(|| DdgsError::Failed("date out of range".into()))?;
    let when = Utc::now().checked_sub_signed(delta).ok_or_else(|| DdgsError::Failed("date out of range".into()))?;
    Ok(iso_utc(when))
}

/// `extract_url`: the target of a `/RU=` redirect, without its query string (IndexError if absent)
pub fn extract_url(u: &str) -> Result<String, DdgsError> {
    let (_, after) = u
        .split_once("/RU=")
        .ok_or_else(|| DdgsError::Failed("list index out of range".into()))?;
    let before_rk = after.split("/RK=").next().unwrap_or("");
    let no_query = before_rk.split('?').next().unwrap_or("");
    Ok(url_unquote(&no_query.replace('+', " ")))
}

/// `extract_image`: the part after the first "-/", else the url itself
pub fn extract_image(u: &str) -> String {
    match u.find("-/") {
        Some(idx) => u[idx + 2..].to_string(),
        None => u.to_string(),
    }
}

/// `extract_source`: the part before " ·  via Yahoo"
pub fn extract_source(s: &str) -> String {
    s.split(" ·  via Yahoo").next().unwrap_or("").to_string()
}

/// the first `src` / `data-src` attribute of the first img that has either (`(.//img/@data-src | .//img/@src)[1]`)
fn first_image_source(item: ElementRef<'_>) -> String {
    for img in desc_named(item, "img") {
        for (name, value) in img.value().attrs() {
            if name == "data-src" || name == "src" {
                return value.to_string();
            }
        }
    }
    String::new()
}

/// `//div[@id='web']//li[a]`
fn is_item(e: ElementRef<'_>) -> bool {
    named(e, "li")
        && html::has_child_where(e, |c| named(c, "a"))
        && has_ancestor(e, |a| named(a, "div") && a.value().attr("id") == Some("web"))
}

/// `extract_results` (BaseSearchEngine path: whitespace-collapsed xpath text for every field)
pub fn extract(html_text: &str) -> Result<Vec<Record>, DdgsError> {
    let doc = html::parse(html_text);
    let mut out = Vec::new();
    for item in all_elements(&doc).into_iter().filter(|e| is_item(*e)) {
        let date = text_of(&desc_where(item, |e| named(e, "span") && class_has(e, "time")));
        let title = text_of(&desc_named(item, "h4"));
        let body = text_of(&desc_named(item, "p"));
        // .//h4/a/@href
        let url = attr_concat(&desc_where(item, |e| named(e, "a") && html::parent_named(e, "h4")), "href");
        let image = first_image_source(item);
        let source = text_of(&desc_where(item, |e| named(e, "span") && class_has(e, "source")));
        out.push(build(
            Category::News,
            vec![
                ("date", Value::String(collapse_ws(&date))),
                ("title", Value::String(collapse_ws(&title))),
                ("body", Value::String(collapse_ws(&body))),
                ("url", Value::String(collapse_ws(&url))),
                ("image", Value::String(collapse_ws(&image))),
                ("source", Value::String(collapse_ws(&source))),
            ],
        )?);
    }
    Ok(out)
}

/// one result's post-processing steps, in python's order
fn post_one(rec: &mut Record) -> Result<(), DdgsError> {
    let text = |rec: &Record, field: &str| rec.get(field).and_then(Value::as_str).unwrap_or("").to_string();
    set(rec, "date", Value::String(extract_date(&text(rec, "date"))?))?;
    set(rec, "url", Value::String(extract_url(&text(rec, "url"))?))?;
    set(rec, "image", Value::String(extract_image(&text(rec, "image"))))?;
    set(rec, "source", Value::String(extract_source(&text(rec, "source"))))?;
    Ok(())
}

/// `post_extract_results`: python wraps the loop in one try, so the first failing result stops
/// the processing of the rest (results already changed keep their changes)
pub fn post(mut results: Vec<Record>) -> Vec<Record> {
    for rec in results.iter_mut() {
        if post_one(rec).is_err() {
            break;
        }
    }
    results
}

pub async fn search(net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    let payload: Payload = build_payload(query, args);
    let mut session = Session::new(net, &[])?;
    let Some(text) = session.request_text(Method::Get, URL, &payload).await? else {
        return Ok(Vec::new());
    };
    Ok(post(extract(&text)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_dates_and_redirects() {
        let rel = extract_date("posted 2 hours ago").unwrap();
        let parsed = chrono::DateTime::parse_from_str(&rel, "%Y-%m-%dT%H:%M:%S%:z").unwrap();
        let drift = (Utc::now() - Duration::hours(2) - parsed.with_timezone(&Utc)).num_seconds().abs();
        assert!(drift < 60);
        assert_eq!(extract_date("Jan 3").unwrap(), "Jan 3");
        assert_eq!(
            extract_url("https://r.search.yahoo.com/RU=https%3A%2F%2Fn.test%2Fa%3Fq%3D1/RK=0").unwrap(),
            // %3F is not a raw "?", so the query survives the split (python does the same)
            "https://n.test/a?q=1"
        );
        assert!(extract_url("https://no-redirect.test/").is_err());
        assert_eq!(extract_image("https://x.test/-/https://img.test/a.jpg"), "https://img.test/a.jpg");
        // the source text is whitespace-collapsed before extract_source runs, so its two-space
        // separator never matches: python keeps the suffix too
        assert_eq!(extract_source("Reuters ·  via Yahoo"), "Reuters");
        assert_eq!(extract_source("Reuters · via Yahoo"), "Reuters · via Yahoo");
    }

    #[test]
    fn extracts_news_items_inside_the_web_div() {
        let page = r#"<div id="web"><ul><li><a href="x"></a><h4><a href="https://r.search.yahoo.com/RU=https%3A%2F%2Fn.test%2Fstory/RK=0">Big story</a></h4>
            <p>Lead paragraph</p><span class="time">3 days ago</span><span class="source">Wire ·  via Yahoo</span>
            <img data-src="https://th.test/-/https://img.test/p.jpg" src="other"></li></ul></div>
            <ul><li><a href="y"></a><h4>Outside</h4></li></ul>"#;
        let out = post(extract(page).unwrap());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["title"], serde_json::json!("Big story"));
        assert_eq!(out[0]["url"], serde_json::json!("https://n.test/story"));
        assert_eq!(out[0]["image"], serde_json::json!("https://img.test/p.jpg"));
        assert_eq!(out[0]["source"], serde_json::json!("Wire · via Yahoo"));
        assert!(out[0]["date"].as_str().unwrap().ends_with("+00:00"));
    }
}
