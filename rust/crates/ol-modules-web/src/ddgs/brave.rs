//! `engines/brave.py`: text results from search.brave.com (html scraping; region and safesearch
//! travel as cookies).

use super::html::{self, all_elements, ancestor_below, attr_concat, class_has, desc_where, is_last_child_named, named, text_of};
use super::http::{Method, Net, Session};
use super::results::{build, collapse_ws};
use super::{kv, lookup, region_parts, Category, DdgsError, Payload, SearchArgs};
use crate::ddgs::results::Record;
use scraper::ElementRef;

const URL: &str = "https://search.brave.com/search";
const COOKIE_HOST: &str = "https://search.brave.com";

const TIMELIMIT: &[(&str, &str)] = &[("d", "pd"), ("w", "pw"), ("m", "pm"), ("y", "py")];

/// `build_payload`: returns the query params and the cookies the python code sets on the client
pub fn build_payload(query: &str, args: &SearchArgs) -> Result<(Payload, Vec<(String, String)>), DdgsError> {
    let mut payload = vec![kv("q", query), kv("source", "web")];
    let (country, _lang) = region_parts(&args.region)?;
    let mut cookies = vec![(country.clone(), country), ("useLocation".to_string(), "0".to_string())];
    if args.safesearch != "moderate" {
        let value = if args.safesearch == "on" { "strict" } else { "off" };
        cookies.push(("safesearch".to_string(), value.to_string()));
    }
    if let Some(tl) = args.timelimit() {
        payload.push(kv("tf", lookup(TIMELIMIT, tl)?));
    }
    if args.page > 1 {
        payload.push(kv("offset", args.page - 1));
    }
    Ok((payload, cookies))
}

/// `extract_results` for the web results (`//div[@data-type='web']`)
pub fn extract(html_text: &str) -> Result<Vec<Record>, DdgsError> {
    let doc = html::parse(html_text);
    let mut out = Vec::new();
    for item in all_elements(&doc).into_iter().filter(|e| named(*e, "div") && e.value().attr("data-type") == Some("web")) {
        // .//div[(contains(@class,'title') or contains(@class,'sitename-container')) and position()=last()]//text()
        let title_nodes = desc_where(item, |e| {
            named(e, "div")
                && (class_has(e, "title") || class_has(e, "sitename-container"))
                && is_last_child_named(e, "div")
        });
        let title = collapse_ws(&text_of(&title_nodes));
        // .//a[div[contains(@class, 'title')]]/@href
        let href_nodes = desc_where(item, |e| named(e, "a") && html::has_child_where(e, |c| named(c, "div") && class_has(c, "title")));
        let href = collapse_ws(&attr_concat(&href_nodes, "href"));
        // .//div[contains(@class,'snippet')]//div[contains(@class,'content')]//text()
        let body_nodes = desc_where(item, |e| {
            named(e, "div") && class_has(e, "content") && ancestor_below(e, item, |a: ElementRef<'_>| named(a, "div") && class_has(a, "snippet"))
        });
        let body = collapse_ws(&text_of(&body_nodes));
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
    fn region_and_safesearch_become_cookies() {
        let args = SearchArgs::default();
        let (payload, cookies) = build_payload("rust", &args).unwrap();
        assert_eq!(payload, vec![kv("q", "rust"), kv("source", "web")]);
        assert_eq!(cookies, vec![("us".to_string(), "us".to_string()), ("useLocation".to_string(), "0".to_string())]);

        let mut args = SearchArgs::default();
        args.safesearch = "off".into();
        args.timelimit = Some("y".into());
        args.page = 3;
        let (payload, cookies) = build_payload("rust", &args).unwrap();
        assert_eq!(cookies.last().unwrap(), &("safesearch".to_string(), "off".to_string()));
        assert_eq!(payload.last().unwrap(), &kv("offset", 2));
        assert!(payload.contains(&kv("tf", "py")));
    }

    #[test]
    fn title_is_the_last_title_div_and_body_is_inside_snippet_content() {
        // the sitename is not the last div child of its parent, so only the title div counts
        let page = r#"<div data-type="web">
              <div class="header">
                <div class="sitename-container">Site</div>
                <div class="crumbs"></div>
                <a href="https://a.test/x"><div class="title">Page <em>Title</em></div></a>
              </div>
              <div class="snippet"><div class="content">A snippet &amp; text</div></div>
            </div>"#;
        let out = extract(page).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["title"], serde_json::json!("Page Title"));
        assert_eq!(out[0]["href"], serde_json::json!("https://a.test/x"));
        assert_eq!(out[0]["body"], serde_json::json!("A snippet & text"));
    }
}
