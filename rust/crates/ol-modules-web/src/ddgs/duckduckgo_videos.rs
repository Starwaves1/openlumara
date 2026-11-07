//! `engines/duckduckgo_videos.py`: video results from duckduckgo.com/v.js (vqd token, JSON).

use super::http::{get_vqd, Method, Net, Session};
use super::results::{build, json_item, json_results};
use super::{kv, lookup, Category, DdgsError, Payload, SearchArgs};
use crate::ddgs::results::Record;
use serde_json::Value;

const URL: &str = "https://duckduckgo.com/v.js";

const SAFESEARCH: &[(&str, &str)] = &[("on", "1"), ("moderate", "-1"), ("off", "-2")];

/// `elements_replace` (identical json keys and field names)
const FIELDS: &[&str] = &[
    "content",
    "description",
    "duration",
    "embed_html",
    "embed_url",
    "image_token",
    "images",
    "provider",
    "published",
    "publisher",
    "statistics",
    "title",
    "uploader",
];

/// `build_payload`: l, o, q, vqd, f (filters, always present), p, then s (page > 1)
pub fn build_payload(query: &str, args: &SearchArgs, vqd: &str) -> Result<Payload, DdgsError> {
    let timelimit = match args.timelimit() {
        Some(tl) => format!("publishedAfter:{tl}"),
        None => String::new(),
    };
    let resolution = prefixed("videoDefinition:", args.extra("resolution"));
    let duration = prefixed("videoDuration:", args.extra("duration"));
    let license = prefixed("videoLicense:", args.extra("license_videos"));
    let safesearch = lookup(SAFESEARCH, &args.safesearch.to_lowercase())?;

    let mut payload = vec![
        kv("l", &args.region),
        kv("o", "json"),
        kv("q", query),
        kv("vqd", vqd),
        kv("f", format!("{timelimit},{resolution},{duration},{license}")),
        kv("p", safesearch),
    ];
    if args.page > 1 {
        payload.push(kv("s", (args.page - 1) * 60));
    }
    Ok(payload)
}

fn prefixed(prefix: &str, value: &str) -> String {
    if value.is_empty() { String::new() } else { format!("{prefix}{value}") }
}

pub fn extract(json_text: &str) -> Result<Vec<Record>, DdgsError> {
    let mut out = Vec::new();
    for item in json_results(json_text)? {
        let obj = json_item(&item)?;
        let pairs: Vec<(&str, Value)> = FIELDS
            .iter()
            .map(|field| (*field, obj.get(*field).cloned().unwrap_or(Value::Null)))
            .collect();
        out.push(build(Category::Videos, pairs)?);
    }
    Ok(out)
}

pub async fn search(net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    let mut session = Session::new(net, &[])?;
    let vqd = get_vqd(&mut session, query).await?;
    let payload = build_payload(query, args, &vqd)?;
    let Some(text) = session.request_text(Method::Get, URL, &payload).await? else {
        return Ok(Vec::new());
    };
    extract(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_string_is_always_four_comma_separated_slots() {
        let args = SearchArgs::default();
        let p = build_payload("clip", &args, "v").unwrap();
        assert!(p.contains(&kv("f", ",,,")));
        assert!(p.contains(&kv("p", "-1")));
        let mut args = SearchArgs { timelimit: Some("w".into()), ..Default::default() };
        args.extra.insert("resolution".into(), "high".into());
        args.extra.insert("duration".into(), "short".into());
        let p = build_payload("clip", &args, "v").unwrap();
        assert!(p.contains(&kv("f", "publishedAfter:w,videoDefinition:high,videoDuration:short,")));
    }

    #[test]
    fn video_record_keeps_all_thirteen_fields_in_order() {
        let json = r#"{"results":[{"title":"A <b>clip</b>","content":"https://v.test/watch?v=1","description":"d","embed_url":"https://e.test/1","publisher":"Pub <i>x</i>","statistics":{"viewCount":5}}]}"#;
        let out = extract(json).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["title"], serde_json::json!("A clip"));
        assert_eq!(out[0]["publisher"], serde_json::json!("Pub x"));
        assert_eq!(out[0]["uploader"], Value::Null);
        assert_eq!(out[0].len(), 13);
        assert_eq!(out[0].keys().next().unwrap(), "title");
    }
}
