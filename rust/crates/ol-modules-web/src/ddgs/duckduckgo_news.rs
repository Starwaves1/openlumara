//! `engines/duckduckgo_news.py`: news results from duckduckgo.com/news.js (vqd token, JSON).

use super::http::{get_vqd, Method, Net, Session};
use super::results::{build, json_item, json_results};
use super::{kv, lookup, Category, DdgsError, Payload, SearchArgs};
use crate::ddgs::results::Record;
use serde_json::Value;

const URL: &str = "https://duckduckgo.com/news.js";

const SAFESEARCH: &[(&str, &str)] = &[("on", "1"), ("moderate", "-1"), ("off", "-2")];

/// `elements_replace`: json key -> result field
const FIELDS: &[(&str, &str)] = &[
    ("date", "date"),
    ("title", "title"),
    ("excerpt", "body"),
    ("url", "url"),
    ("image", "image"),
    ("source", "source"),
];

/// `build_payload`: l, o, noamp, q, vqd, p, then df (timelimit) and s (page > 1)
pub fn build_payload(query: &str, args: &SearchArgs, vqd: &str) -> Result<Payload, DdgsError> {
    let safesearch = lookup(SAFESEARCH, &args.safesearch.to_lowercase())?;
    let mut payload = vec![
        kv("l", &args.region),
        kv("o", "json"),
        kv("noamp", "1"),
        kv("q", query),
        kv("vqd", vqd),
        kv("p", safesearch),
    ];
    if let Some(tl) = args.timelimit() {
        payload.push(kv("df", tl));
    }
    if args.page > 1 {
        payload.push(kv("s", (args.page - 1) * 30));
    }
    Ok(payload)
}

pub fn extract(json_text: &str) -> Result<Vec<Record>, DdgsError> {
    let mut out = Vec::new();
    for item in json_results(json_text)? {
        let obj = json_item(&item)?;
        let pairs: Vec<(&str, Value)> = FIELDS
            .iter()
            .map(|(key, field)| (*field, obj.get(*key).cloned().unwrap_or(Value::Null)))
            .collect();
        out.push(build(Category::News, pairs)?);
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
    fn payload_key_order_and_safesearch_codes() {
        let mut args = SearchArgs::default();
        let p = build_payload("news", &args, "v").unwrap();
        assert_eq!(
            p,
            vec![kv("l", "us-en"), kv("o", "json"), kv("noamp", "1"), kv("q", "news"), kv("vqd", "v"), kv("p", "-1")]
        );
        args.safesearch = "on".into();
        args.timelimit = Some("d".into());
        args.page = 2;
        let p = build_payload("news", &args, "v").unwrap();
        assert!(p.contains(&kv("p", "1")));
        assert!(p.contains(&kv("df", "d")));
        assert_eq!(p.last().unwrap(), &kv("s", 30));
    }

    #[test]
    fn excerpt_becomes_body_and_epoch_dates_become_iso() {
        let json = r#"{"results":[{"date":1767225600,"title":"Big <i>news</i>","excerpt":"Snippet &amp; more","url":"https://n.test/a%20b","image":"https://i.test/x.png","source":"Wire"}]}"#;
        let out = extract(json).unwrap();
        assert_eq!(out[0]["date"], serde_json::json!("2026-01-01T00:00:00+00:00"));
        assert_eq!(out[0]["title"], serde_json::json!("Big news"));
        assert_eq!(out[0]["body"], serde_json::json!("Snippet & more"));
        assert_eq!(out[0]["url"], serde_json::json!("https://n.test/a+b"));
        assert_eq!(out[0]["source"], serde_json::json!("Wire"));
    }
}
