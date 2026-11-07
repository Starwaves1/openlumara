//! `engines/mojeek.py`: text results from mojeek.com (html scraping; region as cookies).

use super::html::{self, all_elements, attr_concat, class_has, class_is, desc_where, named, parent_named, text_of};
use super::http::{Method, Net, Session};
use super::results::{build, collapse_ws};
use super::{kv, region_parts, Category, DdgsError, Payload, SearchArgs};
use crate::ddgs::results::Record;
use scraper::ElementRef;

const URL: &str = "https://www.mojeek.com/search";
const COOKIE_HOST: &str = "https://www.mojeek.com";

/// `build_payload`: returns the query params and the region cookies (arc, lb)
pub fn build_payload(query: &str, args: &SearchArgs) -> Result<(Payload, Vec<(String, String)>), DdgsError> {
    let (country, lang) = region_parts(&args.region)?;
    let cookies = vec![("arc".to_string(), country), ("lb".to_string(), lang)];
    let mut payload = vec![kv("q", query)];
    if args.safesearch == "on" {
        payload.push(kv("safe", "1"));
    }
    if args.page > 1 {
        payload.push(kv("s", (args.page - 1) * 10 + 1));
    }
    Ok((payload, cookies))
}

/// `//ul[contains(@class, 'results')]/li`
fn is_item(e: ElementRef<'_>) -> bool {
    named(e, "li") && e.parent().and_then(ElementRef::wrap).is_some_and(|ul| named(ul, "ul") && class_has(ul, "results"))
}

pub fn extract(html_text: &str) -> Result<Vec<Record>, DdgsError> {
    let doc = html::parse(html_text);
    let mut out = Vec::new();
    for item in all_elements(&doc).into_iter().filter(|e| is_item(*e)) {
        let title = collapse_ws(&text_of(&desc_where(item, |e| named(e, "h2"))));
        // .//h2/a/@href
        let href_nodes = desc_where(item, |e| named(e, "a") && parent_named(e, "h2"));
        let href = collapse_ws(&attr_concat(&href_nodes, "href"));
        // .//p[@class='s']//text()
        let body = collapse_ws(&text_of(&desc_where(item, |e| named(e, "p") && class_is(e, "s"))));
        out.push(build(
            Category::Text,
            vec![("title", title.into()), ("href", href.into()), ("body", body.into())],
        )?);
    }
    Ok(out)
}

pub async fn search(net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    let (payload, cookies) = build_payload(query, args)?;
    let mut session = Session::new(net, &[])?;
    let cookie_refs: Vec<(&str, &str)> = cookies.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    session.set_cookies(COOKIE_HOST, &cookie_refs);
    let Some(text) = session.request_text(Method::Get, URL, &payload).await? else {
        return Ok(Vec::new());
    };
    extract(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_and_region_cookies() {
        let mut args = SearchArgs::default();
        args.region = "GB-en".into();
        args.safesearch = "on".into();
        args.page = 2;
        let (payload, cookies) = build_payload("tea", &args).unwrap();
        assert_eq!(cookies, vec![("arc".to_string(), "gb".to_string()), ("lb".to_string(), "en".to_string())]);
        assert_eq!(payload, vec![kv("q", "tea"), kv("safe", "1"), kv("s", 11)]);
    }

    #[test]
    fn extracts_results_list_items() {
        let page = r#"<ul class="results-list"><li><h2><a href="https://m.test/1">First <i>hit</i></a></h2>
              <p class="s">Snippet one</p><p class="x">ignored</p></li>
            <li><h2><a href="https://m.test/2">Second</a></h2></li></ul>
            <ul class="other"><li><h2><a href="https://no.test">no</a></h2></li></ul>"#;
        let out = extract(page).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["title"], serde_json::json!("First hit"));
        assert_eq!(out[0]["href"], serde_json::json!("https://m.test/1"));
        assert_eq!(out[0]["body"], serde_json::json!("Snippet one"));
        assert_eq!(out[1]["body"], serde_json::json!(""));
    }
}
