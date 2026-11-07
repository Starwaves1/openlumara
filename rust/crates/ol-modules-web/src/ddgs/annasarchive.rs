//! `engines/annasarchive.py`: book results from one of Anna's Archive mirrors (html scraping).

use super::html::{self, all_elements, attr_concat, child_named, class_has, desc_where, direct_text, has_child_where, named, text_of};
use super::http::{Method, Net, Session};
use super::results::{build, collapse_ws, set, Record};
use super::{kv, Category, DdgsError, Payload, SearchArgs};
use once_cell::sync::Lazy;
use rand::seq::SliceRandom;
use serde_json::Value;

/// `random.choice(['gd', 'gl', 'pk'])`, drawn once per process like the class attribute
static MIRROR: Lazy<&'static str> = Lazy::new(|| {
    let mirrors = ["gd", "gl", "pk"];
    mirrors.choose(&mut rand::thread_rng()).copied().unwrap_or("gd")
});

/// the `search_url` base: `https://annas-archive.<mirror>`
fn base_url() -> String {
    format!("https://annas-archive.{}", *MIRROR)
}

/// `build_payload`: q and page (timelimit is ignored)
pub fn build_payload(query: &str, args: &SearchArgs) -> Payload {
    vec![kv("q", query), kv("page", args.page)]
}

/// `pre_process_html`: html comments are removed before parsing
fn pre_process(html_text: &str) -> String {
    html_text.replace("<!--", "").replace("-->", "")
}

/// `extract_results` (items, then each field's whitespace-collapsed xpath text)
pub fn extract(html_text: &str) -> Result<Vec<Record>, DdgsError> {
    let doc = html::parse(&pre_process(html_text));
    let mut out = Vec::new();
    // //div[contains(@class, 'record-list-outer')]/div
    let mut items = Vec::new();
    for outer in all_elements(&doc).into_iter().filter(|e| named(*e, "div") && class_has(*e, "record-list-outer")) {
        for child in child_named(outer, "div") {
            if !items.iter().any(|seen: &scraper::ElementRef<'_>| seen.id() == child.id()) {
                items.push(child);
            }
        }
    }
    for item in items {
        let title = collapse_ws(&text_of(&desc_where(item, |e| named(e, "a") && class_has(e, "text-lg"))));
        let author = collapse_ws(&text_of(&desc_where(item, |e| named(e, "a") && has_child_where(e, |c| named(c, "span") && class_has(c, "user")))));
        let publisher = collapse_ws(&text_of(&desc_where(item, |e| named(e, "a") && has_child_where(e, |c| named(c, "span") && class_has(c, "company")))));
        // .//div[contains(@class,'text-gray-800')]/text()
        let info = collapse_ws(&direct_text(&desc_where(item, |e| named(e, "div") && class_has(e, "text-gray-800"))));
        let url = collapse_ws(&attr_concat(&child_named(item, "a"), "href"));
        let thumbnail = collapse_ws(&attr_concat(&desc_where(item, |e| named(e, "img")), "src"));
        out.push(build(
            Category::Books,
            vec![
                ("title", Value::String(title)),
                ("author", Value::String(author)),
                ("publisher", Value::String(publisher)),
                ("info", Value::String(info)),
                ("url", Value::String(url)),
                ("thumbnail", Value::String(thumbnail)),
            ],
        )?);
    }
    Ok(out)
}

/// `post_extract_results`: the item link is joined onto the mirror's base url
pub fn post(mut results: Vec<Record>) -> Result<Vec<Record>, DdgsError> {
    let base = base_url();
    for rec in results.iter_mut() {
        let url = rec.get("url").and_then(Value::as_str).unwrap_or("").to_string();
        set(rec, "url", Value::String(format!("{base}{url}")))?;
    }
    Ok(results)
}

pub async fn search(net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    let payload = build_payload(query, args);
    let mut session = Session::new(net, &[])?;
    let url = format!("{}/search", base_url());
    let Some(text) = session.request_text(Method::Get, &url, &payload).await? else {
        return Ok(Vec::new());
    };
    post(extract(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_and_mirror_base() {
        let mut args = SearchArgs::default();
        args.page = 2;
        assert_eq!(build_payload("dune", &args), vec![kv("q", "dune"), kv("page", 2)]);
        assert!(["https://annas-archive.gd", "https://annas-archive.gl", "https://annas-archive.pk"].contains(&base_url().as_str()));
    }

    #[test]
    fn extracts_book_fields_and_joins_the_link_onto_the_mirror() {
        let page = r#"<div class="record-list-outer">
          <div><!-- hidden --><a class="text-lg" href="/md5/abc">Dune <i>Messiah</i></a>
            <a href="/md5/abc"><span class="user">Frank Herbert</span></a>
            <a href="/md5/abc"><span class="company">Ace</span></a>
            <div class="text-gray-800">1965 · English</div>
            <img src="https://covers.test/c.jpg"></div>
        </div>"#;
        let out = post(extract(page).unwrap()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["title"], serde_json::json!("Dune Messiah"));
        assert_eq!(out[0]["author"], serde_json::json!("Frank Herbert"));
        assert_eq!(out[0]["publisher"], serde_json::json!("Ace"));
        assert_eq!(out[0]["info"], serde_json::json!("1965 · English"));
        assert_eq!(out[0]["thumbnail"], serde_json::json!("https://covers.test/c.jpg"));
        assert!(out[0]["url"].as_str().unwrap().ends_with("/md5/abc"));
        assert!(out[0]["url"].as_str().unwrap().starts_with("https://annas-archive."));
    }
}
