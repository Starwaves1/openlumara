//! `engines/wikipedia.py`: the first opensearch hit of the language edition, with its intro extract.

use super::http::{Method, Net, Session};
use super::results::{no_attr, py_getitem, py_key, set, template, Record};
use super::{region_parts, Category, DdgsError, SearchArgs};
use crate::pytext::quote;
use crate::util::py_type_name;
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
    // `if not json_data[1]: return []`
    let second = py_getitem(&json, 1)?;
    if !truthy(&second) {
        return Ok(Vec::new());
    }
    // `result.title = json_data[1][0]`; `result.href = json_data[3][0]`
    let title = py_getitem(&second, 0)?;
    let href = py_getitem(&py_getitem(&json, 3)?, 0)?;

    let mut rec = template(Category::Text);
    set(&mut rec, "title", title)?;
    set(&mut rec, "href", href)?;

    // quote(result.title): the normalised title
    let title_text = rec.get("title").and_then(Value::as_str).ok_or_else(|| DdgsError::Failed("title is not a str".into()))?.to_string();
    // python `if resp_data:` skips an empty body (falsy), keeping body=''
    if let Some(resp) = session.request_text(Method::Get, &extract_url(lang, &title_text), &[]).await?.filter(|r| !r.is_empty()) {
        let data: Value = serde_json::from_str(&resp).map_err(json_error)?;
        // `json_data["query"]["pages"].values()`
        let pages = py_key(py_key(&data, "query")?, "pages")?;
        let pages = pages.as_object().ok_or_else(|| no_attr(pages, "values"))?;
        // `next(iter(...))`: StopIteration on an empty dict, whose str() is ""
        let first = pages.values().next().ok_or_else(|| DdgsError::Failed(String::new()))?;
        // `.get("extract", "")`
        let extract = first
            .as_object()
            .ok_or_else(|| no_attr(first, "get"))?
            .get("extract")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        set(&mut rec, "body", extract)?;
    }
    // `"may refer to:" in result.body`: a list or dict body tests membership without raising
    let contains_marker = match rec.get("body") {
        Some(Value::String(body)) => body.contains("may refer to:"),
        Some(Value::Array(_) | Value::Object(_)) | None => false,
        Some(other) => return Err(DdgsError::Failed(format!("argument of type '{}' is not iterable", py_type_name(other)))),
    };
    if contains_marker {
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

    #[tokio::test]
    async fn opensearch_shape_errors_read_like_python() {
        let net = Net { proxy: None, timeout: std::time::Duration::from_secs(5) };
        let mut session = Session::new(&net, &[]).unwrap();
        // json_data[3][0] on an empty list
        let no_urls = r#"["zzz", ["Title"], [], []]"#;
        assert_eq!(extract(&mut session, "en", no_urls).await.unwrap_err().message(), "list index out of range");
        // json_data[1] on a dict: KeyError: 1
        let dict_root = r#"{"a": 1}"#;
        assert_eq!(extract(&mut session, "en", dict_root).await.unwrap_err().message(), "1");
        // json_data[1] on a number: TypeError
        assert_eq!(extract(&mut session, "en", "5").await.unwrap_err().message(), "'int' object is not subscriptable");
    }
}
