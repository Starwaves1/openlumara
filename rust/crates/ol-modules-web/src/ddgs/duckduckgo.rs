//! `engines/duckduckgo.py`: text results from html.duckduckgo.com (POST form, html scraping).

use super::html::{self, all_elements, attr_concat, child_named, desc_named, named, text_of};
use super::http::{Method, Net, Session};
use super::results::{build, collapse_ws};
use super::{kv, Category, DdgsError, Payload, SearchArgs};
use crate::ddgs::results::Record;
use serde_json::Value;

const URL: &str = "https://html.duckduckgo.com/html/";

/// `build_payload`: q, b, l, then s (page > 1) and df (timelimit)
pub fn build_payload(query: &str, args: &SearchArgs) -> Payload {
    let mut payload = vec![kv("q", query), kv("b", ""), kv("l", &args.region)];
    if args.page > 1 {
        payload.push(kv("s", 10 + (args.page - 2) * 15));
    }
    if let Some(tl) = args.timelimit() {
        payload.push(kv("df", tl));
    }
    payload
}

/// `extract_results` with `items_xpath = //div[contains(@class, 'body')]`
pub fn extract(html_text: &str) -> Result<Vec<Record>, DdgsError> {
    let doc = html::parse(html_text);
    let mut out = Vec::new();
    for item in all_elements(&doc).into_iter().filter(|e| named(*e, "div") && html::class_has(*e, "body")) {
        let title = collapse_ws(&text_of(&desc_named(item, "h2")));
        let href = collapse_ws(&attr_concat(&child_named(item, "a"), "href"));
        let body = collapse_ws(&text_of(&child_named(item, "a")));
        out.push(build(
            Category::Text,
            vec![("title", title.into()), ("href", href.into()), ("body", body.into())],
        )?);
    }
    Ok(out)
}

/// `post_extract_results`: drop the ad redirect links
pub fn post(results: Vec<Record>) -> Vec<Record> {
    results
        .into_iter()
        .filter(|r| !r.get("href").and_then(Value::as_str).unwrap_or("").starts_with("https://duckduckgo.com/y.js?"))
        .collect()
}

pub async fn search(net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    let payload = build_payload(query, args);
    let mut session = Session::new(net, &[])?;
    let Some(html_text) = session.request_text(Method::Post, URL, &payload).await? else {
        return Ok(Vec::new());
    };
    Ok(post(extract(&html_text)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_has_python_key_order_and_paging_rules() {
        let mut args = SearchArgs::default();
        let p = build_payload("rust lang", &args);
        assert_eq!(p, vec![kv("q", "rust lang"), kv("b", ""), kv("l", "us-en")]);
        args.page = 3;
        args.timelimit = Some("w".into());
        let p = build_payload("x", &args);
        assert_eq!(p.last().unwrap(), &kv("df", "w"));
        // s = 10 + (page - 2) * 15
        assert!(p.contains(&kv("s", 25)));
    }

    #[test]
    fn extracts_title_href_body_and_drops_ad_redirects() {
        let page = r#"<html><body>
            <div class="result body"><h2><a href="https://example.com/a">Example   <b>Page</b></a></h2>
              <a href="https://example.com/a">Example <b>Page</b> snippet&amp;more</a></div>
            <div class="result body"><a href="https://duckduckgo.com/y.js?ad=1">Ad</a><h2>Ad</h2></div>
            </body></html>"#;
        let results = extract(page).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["title"], Value::String("Example Page".into()));
        assert_eq!(results[0]["href"], Value::String("https://example.com/a".into()));
        assert_eq!(results[0]["body"], Value::String("Example Page snippet&more".into()));
        let kept = post(results);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["href"], Value::String("https://example.com/a".into()));
    }
}
