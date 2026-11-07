//! `engines/yahoo.py`: text results from search.yahoo.com (html scraping; Bing-redirect links are
//! unwrapped from their `/RU=` target).

use super::html::{self, all_elements, attr_concat, ancestor_below, class_has, desc_where, named, text_of};
use super::http::{Method, Net, Session};
use super::results::{build, collapse_ws, set, Record};
use super::{kv, Category, DdgsError, Payload, SearchArgs};
use crate::pytext::url_unquote;
use base64::Engine;
use serde_json::Value;

/// `token_urlsafe(n)`: n random bytes, urlsafe base64 without padding
fn token_urlsafe(n: usize) -> String {
    let bytes: Vec<u8> = (0..n).map(|_| rand::random::<u8>()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `build_payload`: the search url is re-randomised (`_ylt`, `_ylu`) on every call
pub fn build_payload(query: &str, args: &SearchArgs) -> (String, Payload) {
    let url = format!(
        "https://search.yahoo.com/search;_ylt={};_ylu={}",
        token_urlsafe(24 * 3 / 4),
        token_urlsafe(47 * 3 / 4)
    );
    let mut payload = vec![kv("p", query)];
    if args.page > 1 {
        payload.push(kv("b", (args.page - 1) * 7 + 1));
    }
    if let Some(tl) = args.timelimit() {
        payload.push(kv("btf", tl));
    }
    (url, payload)
}

/// `extract_url`: the target of a `/RU=` redirect
pub fn extract_url(u: &str) -> String {
    let after = u.split_once("/RU=").map(|(_, t)| t).unwrap_or("");
    let before_rk = after.split("/RK=").next().unwrap_or("");
    let before_rs = before_rk.split("/RS=").next().unwrap_or("");
    url_unquote(&before_rs.replace('+', " "))
}

/// `extract_results` with `items_xpath = //div[contains(@class, 'relsrch')]`
pub fn extract(html_text: &str) -> Result<Vec<Record>, DdgsError> {
    let doc = html::parse(html_text);
    let mut out = Vec::new();
    for item in all_elements(&doc).into_iter().filter(|e| named(*e, "div") && class_has(*e, "relsrch")) {
        let in_title_div = |a: scraper::ElementRef<'_>| named(a, "div") && class_has(a, "Title");
        // .//div[contains(@class,'Title')]//h3//text()
        let title_nodes = desc_where(item, |e| named(e, "h3") && ancestor_below(e, item, in_title_div));
        let title = collapse_ws(&text_of(&title_nodes));
        // .//div[contains(@class,'Title')]//a/@href
        let href_nodes = desc_where(item, |e| named(e, "a") && ancestor_below(e, item, in_title_div));
        let href = collapse_ws(&attr_concat(&href_nodes, "href"));
        // .//div[contains(@class,'Text')]//text()
        let body_nodes = desc_where(item, |e| named(e, "div") && class_has(e, "Text"));
        let body = collapse_ws(&text_of(&body_nodes));
        out.push(build(
            Category::Text,
            vec![("title", title.into()), ("href", href.into()), ("body", body.into())],
        )?);
    }
    Ok(out)
}

/// `post_extract_results`: drop bing ad clicks, unwrap `/RU=` redirects (through the normaliser)
pub fn post(results: Vec<Record>) -> Result<Vec<Record>, DdgsError> {
    let mut out = Vec::new();
    for mut rec in results {
        let href = rec.get("href").and_then(Value::as_str).unwrap_or("").to_string();
        if href.starts_with("https://www.bing.com/aclick?") {
            continue;
        }
        if href.contains("/RU=") {
            set(&mut rec, "href", Value::String(extract_url(&href)))?;
        }
        out.push(rec);
    }
    Ok(out)
}

pub async fn search(net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    let (url, payload) = build_payload(query, args);
    let mut session = Session::new(net, &[])?;
    let Some(text) = session.request_text(Method::Get, &url, &payload).await? else {
        return Ok(Vec::new());
    };
    post(extract(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_targets_are_unwrapped_and_unquoted() {
        let u = "https://r.search.yahoo.com/_ylt=x/RV=2/RE=1/RO=10/RU=https%3a%2f%2fexample.com%2fa+b/RK=2/RS=abc";
        assert_eq!(extract_url(u), "https://example.com/a b");
    }

    #[test]
    fn payload_paging_and_timelimit() {
        let args = SearchArgs { page: 3, timelimit: Some("m".into()), ..Default::default() };
        let (url, payload) = build_payload("news", &args);
        assert!(url.starts_with("https://search.yahoo.com/search;_ylt="));
        assert!(url.contains(";_ylu="));
        assert_eq!(payload, vec![kv("p", "news"), kv("b", 15), kv("btf", "m")]);
    }

    #[test]
    fn extracts_title_link_and_body_and_drops_ad_clicks() {
        let page = r#"<div class="relsrch"><div class="Title"><h3><a href="https://r.search.yahoo.com/RU=https%3a%2f%2fa.test%2f/RK=0">Alpha</a></h3><a href="https://www.bing.com/aclick?x=1">ad</a></div>
            <div class="Text">Alpha body</div></div>
            <div class="relsrch"><div class="Title"><h3><a href="https://b.test/">Beta</a></h3></div></div>"#;
        let results = post(extract(page).unwrap()).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["href"], serde_json::json!("https://a.test/"));
        assert_eq!(results[0]["title"], serde_json::json!("Alpha"));
        assert_eq!(results[0]["body"], serde_json::json!("Alpha body"));
        assert_eq!(results[1]["title"], serde_json::json!("Beta"));
    }
}
