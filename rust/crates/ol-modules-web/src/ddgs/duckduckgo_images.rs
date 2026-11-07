//! `engines/duckduckgo_images.py`: image results from duckduckgo.com/i.js (vqd token, JSON).

use super::http::{get_vqd, Method, Net, Session};
use super::results::{build, json_item, json_results};
use super::{kv, lookup, Category, DdgsError, Payload, SearchArgs};
use crate::ddgs::results::Record;

const URL: &str = "https://duckduckgo.com/i.js";

const HEADERS: &[(&str, &str)] = &[
    ("Accept", "*/*"),
    ("Accept-Language", "en-US,en;q=0.5"),
    ("Referer", "https://duckduckgo.com/"),
    ("Sec-GPC", "1"),
    ("Connection", "keep-alive"),
    ("Sec-Fetch-Dest", "empty"),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Site", "same-origin"),
    ("Priority", "u=4"),
];

const SAFESEARCH: &[(&str, &str)] = &[("on", "1"), ("moderate", "1"), ("off", "-1")];
const TIMELIMIT: &[(&str, &str)] = &[("d", "Day"), ("w", "Week"), ("m", "Month"), ("y", "Year")];

/// `elements_replace`: result field <- json key (identical names)
const FIELDS: &[&str] = &["title", "image", "thumbnail", "url", "height", "width", "source"];

/// the `timelimit_base[timelimit]` lookup, which python runs before the vqd request
pub fn check_timelimit(args: &SearchArgs) -> Result<(), DdgsError> {
    if let Some(tl) = args.timelimit() {
        lookup(TIMELIMIT, tl)?;
    }
    Ok(())
}

/// `build_payload`: o, q, l, vqd, p, ct, then f (filters) and s (page > 1)
pub fn build_payload(query: &str, args: &SearchArgs, vqd: &str) -> Result<Payload, DdgsError> {
    let timelimit = match args.timelimit() {
        Some(tl) => format!("time:{}", lookup(TIMELIMIT, tl)?),
        None => String::new(),
    };
    let size = prefixed("size:", args.extra("size"));
    let color = prefixed("color:", args.extra("color"));
    let type_image = prefixed("type:", args.extra("type_image"));
    let layout = prefixed("layout:", args.extra("layout"));
    let license = prefixed("license:", args.extra("license_image"));
    let safesearch = lookup(SAFESEARCH, &args.safesearch.to_lowercase())?;

    let mut payload = vec![
        kv("o", "json"),
        kv("q", query),
        kv("l", &args.region),
        kv("vqd", vqd),
        kv("p", safesearch),
        kv("ct", "AT"),
    ];
    if [&timelimit, &size, &color, &type_image, &layout, &license].iter().any(|s| !s.is_empty()) {
        payload.push(kv("f", format!("{timelimit},{size},{color},{type_image},{layout},{license}")));
    }
    if args.page > 1 {
        payload.push(kv("s", (args.page - 1) * 100));
    }
    Ok(payload)
}

/// `f"{prefix}{value}" if value else ""`
fn prefixed(prefix: &str, value: &str) -> String {
    if value.is_empty() { String::new() } else { format!("{prefix}{value}") }
}

/// `extract_results`: json results, each field set through the result normalisers
pub fn extract(json_text: &str) -> Result<Vec<Record>, DdgsError> {
    let mut out = Vec::new();
    for item in json_results(json_text)? {
        let obj = json_item(&item)?;
        let mut pairs = Vec::new();
        for field in FIELDS {
            pairs.push((*field, obj.get(*field).cloned().unwrap_or(serde_json::Value::Null)));
        }
        out.push(build(Category::Images, pairs)?);
    }
    Ok(out)
}

pub async fn search(net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    check_timelimit(args)?;
    let mut session = Session::new(net, HEADERS)?;
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
    fn payload_builds_filter_string_only_when_a_filter_is_set() {
        let args = SearchArgs::default();
        let p = build_payload("cats", &args, "4-abc").unwrap();
        assert_eq!(
            p,
            vec![kv("o", "json"), kv("q", "cats"), kv("l", "us-en"), kv("vqd", "4-abc"), kv("p", "1"), kv("ct", "AT")]
        );
        let mut args = SearchArgs { timelimit: Some("m".into()), safesearch: "OFF".into(), page: 2, ..Default::default() };
        args.extra.insert("size".into(), "Large".into());
        let p = build_payload("cats", &args, "v").unwrap();
        assert!(p.contains(&kv("p", "-1")));
        assert!(p.contains(&kv("f", "time:Month,size:Large,,,,")));
        assert_eq!(p.last().unwrap(), &kv("s", 100));
    }

    #[test]
    fn unknown_timelimit_is_a_key_error() {
        let args = SearchArgs { timelimit: Some("h".into()), ..Default::default() };
        assert!(check_timelimit(&args).is_err());
        assert!(build_payload("x", &args, "v").is_err());
    }

    #[test]
    fn extract_returns_the_seven_image_fields_with_normalised_values() {
        let json = r#"{"results":[{"title":"A <b>cat</b>","image":"https://i.test/a%20b.jpg","thumbnail":"https://t.test/x.jpg","url":"https://p.test/page","height":"100","width":"200","source":"Bing"},{"title":"No image"}]}"#;
        let out = extract(json).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["title"], serde_json::json!("A cat"));
        assert_eq!(out[0]["image"], serde_json::json!("https://i.test/a+b.jpg"));
        assert_eq!(out[1]["url"], serde_json::Value::Null);
        assert_eq!(out[1]["height"], serde_json::Value::Null);
        assert_eq!(out[0].keys().collect::<Vec<_>>(), vec!["title", "image", "thumbnail", "url", "height", "width", "source"]);
    }
}
