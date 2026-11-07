//! `engines/wikipedia.py`: the first opensearch hit of the language edition, with its intro extract.

use super::http::{Method, Net, Session};
use super::results::{set, template, Record};
use super::{region_parts, Category, DdgsError, SearchArgs};
use crate::pytext::quote;
use ol_core::pyjson::truthy;
use serde_json::Value;

/// `search_url` built in `build_payload` for the region's language
fn search_url(lang: &str, query: &str) -> String {
    format!("https://{lang}.wikipedia.org/w/api.php?action=opensearch&profile=fuzzy&limit=1&search={}", quote(query, "/"))
}

/// the intro-extract request made from `extract_results`
fn extract_url(lang: &str, title: &str) -> String {
    format!(
        "https://{lang}.wikipedia.org/w/api.php?action=query&format=json&prop=extracts&titles={}&explaintext=0&exintro=0&redirects=1",
        quote(title, "/")
    )
}

fn json_error(e: serde_json::Error) -> DdgsError {
    DdgsError::Failed(format!("{e}"))
}

/// `extract_results(html_text)`; `lang` is the language the search ran in (`self.lang`)
pub async fn extract(session: &mut Session, lang: &str, text: &str) -> Result<Vec<Record>, DdgsError> {
    let json: Value = serde_json::from_str(text).map_err(json_error)?;
    let arr = json
        .as_array()
        .ok_or_else(|| DdgsError::Failed("opensearch response is not a list".into()))?;
    // `if not json_data[1]: return []`
    let second = arr.get(1).ok_or_else(|| DdgsError::Failed("list index out of range".into()))?;
    if !truthy(second) {
        return Ok(Vec::new());
    }
    let title = second
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .ok_or_else(|| DdgsError::Failed("opensearch title list is empty".into()))?;
    let href = arr
        .get(3)
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .ok_or_else(|| DdgsError::Failed("opensearch url list is empty".into()))?;

    let mut rec = template(Category::Text);
    set(&mut rec, "title", title)?;
    set(&mut rec, "href", href)?;

    // quote(result.title): the normalised title
    let title_text = rec.get("title").and_then(Value::as_str).ok_or_else(|| DdgsError::Failed("title is not a str".into()))?.to_string();
    // python `if resp_data:` skips an empty body (falsy), keeping body=''
    if let Some(resp) = session.request_text(Method::Get, &extract_url(lang, &title_text), &[]).await?.filter(|r| !r.is_empty()) {
        let data: Value = serde_json::from_str(&resp).map_err(json_error)?;
        let pages = data
            .get("query")
            .and_then(|q| q.get("pages"))
            .and_then(Value::as_object)
            .ok_or_else(|| DdgsError::Failed("'pages'".into()))?;
        let first = pages.values().next().ok_or_else(|| DdgsError::Failed("StopIteration".into()))?;
        let extract = first
            .as_object()
            .ok_or_else(|| DdgsError::Failed("page is not a dict".into()))?
            .get("extract")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        set(&mut rec, "body", extract)?;
    }
    let body = rec.get("body").and_then(Value::as_str).ok_or_else(|| DdgsError::Failed("argument of type is not iterable".into()))?;
    if body.contains("may refer to:") {
        return Ok(Vec::new());
    }
    Ok(vec![rec])
}

pub async fn search(net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    let (_country, lang) = region_parts(&args.region)?;
    let mut session = Session::new(net, &[])?;
    let Some(text) = session.request_text(Method::Get, &search_url(&lang, query), &[]).await? else {
        return Ok(Vec::new());
    };
    extract(&mut session, &lang, &text).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_quote_the_query_like_python() {
        assert_eq!(search_url("en", "a b/c?"), "https://en.wikipedia.org/w/api.php?action=opensearch&profile=fuzzy&limit=1&search=a%20b/c%3F");
        assert_eq!(
            extract_url("de", "Ärger"),
            "https://de.wikipedia.org/w/api.php?action=query&format=json&prop=extracts&titles=%C3%84rger&explaintext=0&exintro=0&redirects=1"
        );
    }

    #[tokio::test]
    async fn empty_suggestion_list_gives_no_results_and_hits_need_no_network() {
        let net = Net { proxy: None, timeout: std::time::Duration::from_secs(5) };
        let mut session = Session::new(&net, &[]).unwrap();
        let none = r#"["zzz", [], [], []]"#;
        assert!(extract(&mut session, "en", none).await.unwrap().is_empty());
        let bad = r#"{"not": "a list"}"#;
        assert!(extract(&mut session, "en", bad).await.is_err());
    }
}
