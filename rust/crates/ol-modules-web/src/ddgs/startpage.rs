//! `engines/startpage.py`: text results from startpage.com (POST form with a session `sc` token).

use super::html::{self, all_elements, attr_concat, child_named, class_has, desc_named, desc_where, named, text_of};
use super::http::{Method, Net, Session};
use super::results::{build, collapse_ws};
use super::{kv, lookup, region_parts, Category, DdgsError, Payload, SearchArgs};
use crate::ddgs::results::Record;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

const URL: &str = "https://www.startpage.com/sp/search";
const HOME: &str = "https://www.startpage.com/";
const HEADERS: &[(&str, &str)] = &[("Referer", "https://www.startpage.com/")];

const SAFESEARCH: &[(&str, &str)] = &[("on", "heavy"), ("moderate", "moderate"), ("off", "none")];

/// `get_sc()`: the `sc` hidden input of the search form on the start page (empty when absent)
pub async fn get_sc(session: &mut Session) -> Result<String, DdgsError> {
    let reply = session.request(Method::Get, HOME, &[]).await?;
    sc_from_page(&reply.text())
}

static LXML_NON_ELEMENT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<!--.*?-->|<![^>]*>|<\?[^>]*>").unwrap());

/// lxml's `etree.fromstring` yields no root (ParserError `Document is empty`) when the input holds only
/// whitespace, comments, processing instructions or a doctype; any text or tag gives a root.
fn lxml_document_is_empty(page: &str) -> bool {
    LXML_NON_ELEMENT_RE.replace_all(page, "").trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r')).is_empty()
}

/// the `sc` value of a start page body; a body with no element at all is lxml's `Document is empty`
pub fn sc_from_page(page: &str) -> Result<String, DdgsError> {
    if lxml_document_is_empty(page) {
        return Err(DdgsError::Failed("Document is empty".into()));
    }
    let doc = html::parse(page);
    // //form[@id="search"]//input[@name="sc"]/@value  (first value)
    for form in all_elements(&doc).into_iter().filter(|e| named(*e, "form") && e.value().attr("id") == Some("search")) {
        let inputs = desc_where(form, |e| named(e, "input") && e.value().attr("name") == Some("sc"));
        if let Some(value) = inputs.iter().find_map(|i| i.value().attr("value")) {
            return Ok(value.to_string());
        }
    }
    Ok(String::new())
}

/// `build_payload`: needs the `sc` token, so it is async
pub async fn build_payload(session: &mut Session, query: &str, args: &SearchArgs) -> Result<Payload, DdgsError> {
    let (country, lang) = region_parts(&args.region)?;
    let sc = get_sc(session).await?;
    let qadf = lookup(SAFESEARCH, &args.safesearch.to_lowercase())?;
    let mut payload = vec![
        kv("query", query),
        kv("cat", "web"),
        kv("t", "device"),
        kv("sc", sc),
        kv("lui", "english"),
        kv("language", "english"),
        kv("abp", "1"),
        kv("abd", "0"),
        kv("abe", "0"),
        kv("qsr", format!("{}_{}", lang, country.to_uppercase())),
        kv("qadf", qadf),
        kv("segment", "organic"),
    ];
    if args.page > 1 {
        payload.push(kv("page", args.page));
    }
    if let Some(tl) = args.timelimit() {
        payload.push(kv("with_date", tl));
    }
    Ok(payload)
}

/// `extract_results`: `//div[contains(@class, 'result css')][./a]`
pub fn extract(html_text: &str) -> Result<Vec<Record>, DdgsError> {
    let doc = html::parse(html_text);
    let mut out = Vec::new();
    for item in all_elements(&doc)
        .into_iter()
        .filter(|e| named(*e, "div") && class_has(*e, "result css") && html::has_child_where(*e, |c| named(c, "a")))
    {
        let title = collapse_ws(&text_of(&desc_named(item, "h2")));
        let href = collapse_ws(&attr_concat(&child_named(item, "a"), "href"));
        let body = collapse_ws(&text_of(&desc_named(item, "p")));
        out.push(build(
            Category::Text,
            vec![("title", title.into()), ("href", href.into()), ("body", body.into())],
        )?);
    }
    Ok(out)
}

/// `post_extract_results`: drop google.com links
pub fn post(results: Vec<Record>) -> Vec<Record> {
    results
        .into_iter()
        .filter(|r| !r.get("href").and_then(Value::as_str).unwrap_or("").starts_with("https://www.google.com"))
        .collect()
}

pub async fn search(net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    let mut session = Session::new(net, HEADERS)?;
    let payload = build_payload(&mut session, query, args).await?;
    let Some(text) = session.request_text(Method::Post, URL, &payload).await? else {
        return Ok(Vec::new());
    };
    Ok(post(extract(&text)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_result_blocks_with_a_direct_link_and_drops_google_links() {
        let page = r#"<div class="result css"><a href="https://s.test/1"><h2>One</h2></a><p>Body <b>one</b></p></div>
            <div class="result css"><h2>No link</h2></div>
            <div class="result css"><a href="https://www.google.com/x">G</a></div>"#;
        let results = extract(page).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["title"], serde_json::json!("One"));
        assert_eq!(results[0]["body"], serde_json::json!("Body one"));
        let kept = post(results);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn empty_start_page_body_raises_lxml_document_is_empty() {
        assert_eq!(sc_from_page("").unwrap_err().message(), "Document is empty");
        assert_eq!(sc_from_page(" \n\t").unwrap_err().message(), "Document is empty");
        assert_eq!(sc_from_page("<!-- only a comment -->").unwrap_err().message(), "Document is empty");
        assert_eq!(sc_from_page("<!DOCTYPE html>\n<?pi x?>").unwrap_err().message(), "Document is empty");
        assert_eq!(sc_from_page("<html></html>").unwrap(), "");
        assert_eq!(sc_from_page("plain text").unwrap(), "");
    }

    #[test]
    fn sc_token_is_read_from_the_search_form() {
        assert_eq!(sc_from_page(r#"<form id="search"><input name="sc" value="tok9"></form>"#).unwrap(), "tok9");
    }

    #[test]
    fn sc_token_comes_from_the_search_form() {
        let page = r#"<form id="search"><input name="sc" value="abc123"></form>"#;
        let doc = html::parse(page);
        let form = all_elements(&doc).into_iter().find(|e| named(*e, "form")).unwrap();
        let input = desc_named(form, "input")[0];
        assert_eq!(input.value().attr("value"), Some("abc123"));
    }
}
