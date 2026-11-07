//! `engines/grokipedia.py`: the first typeahead hit of grokipedia.com.

use super::http::{Method, Net, Session};
use super::results::{build, Record};
use super::{kv, Category, DdgsError, Payload, SearchArgs};
use ol_core::pyjson::{py_str, truthy};
use serde_json::{Map, Value};

const URL: &str = "https://grokipedia.com/api/typeahead";

/// `build_payload`: query and limit
pub fn build_payload(query: &str) -> Payload {
    vec![kv("query", query), kv("limit", "1")]
}

/// `extract_results`: a single result built from the first item
pub fn extract(text: &str) -> Result<Vec<Record>, DdgsError> {
    let json: Value = serde_json::from_str(text).map_err(|e| DdgsError::Failed(format!("{e}")))?;
    let obj = json
        .as_object()
        .ok_or_else(|| DdgsError::Failed("response is not a dict".into()))?;
    // `items = json_data.get("results", [])`; `if not items: return []`
    let items = obj.get("results").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
    if !truthy(&items) {
        return Ok(Vec::new());
    }
    let first: &Map<String, Value> = items
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_object)
        .ok_or_else(|| DdgsError::Failed("items[0] is not a dict".into()))?;

    // items[0].get("title", "").strip("_")
    let title = match first.get("title") {
        None => String::new(),
        Some(Value::String(s)) => s.trim_matches('_').to_string(),
        Some(other) => return Err(DdgsError::Failed(format!("'{}' object has no attribute 'strip'", py_type(other)))),
    };
    // body = items[0].get("snippet", ""); body.split("\n\n", 1)[1] if "\n\n" in body else body
    let snippet = match first.get("snippet") {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => return Err(DdgsError::Failed(format!("argument of type '{}' is not iterable", py_type(other)))),
    };
    let body = match snippet.split_once("\n\n") {
        Some((_, rest)) => rest.to_string(),
        None => snippet,
    };
    // items[0]['slug']
    let slug = first.get("slug").ok_or_else(|| DdgsError::Failed("KeyError: 'slug'".into()))?;
    let href = format!("https://grokipedia.com/page/{}", py_str(slug));

    Ok(vec![build(
        Category::Text,
        vec![("title", title.into()), ("href", href.into()), ("body", body.into())],
    )?])
}

fn py_type(v: &Value) -> &'static str {
    crate::util::py_type_name(v)
}

pub async fn search(net: &Net, query: &str, _args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    let payload = build_payload(query);
    let mut session = Session::new(net, &[])?;
    let Some(text) = session.request_text(Method::Get, URL, &payload).await? else {
        return Ok(Vec::new());
    };
    extract(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_is_query_and_limit_one() {
        assert_eq!(build_payload("moon"), vec![kv("query", "moon"), kv("limit", "1")]);
    }

    #[test]
    fn title_is_stripped_of_underscores_and_body_is_after_first_blank_line() {
        let json = r#"{"results":[{"title":"__Lunar_orbit__","snippet":"Intro line\n\nThe real body","slug":"Lunar_orbit"}]}"#;
        let out = extract(json).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["title"], serde_json::json!("Lunar_orbit"));
        assert_eq!(out[0]["body"], serde_json::json!("The real body"));
        assert_eq!(out[0]["href"], serde_json::json!("https://grokipedia.com/page/Lunar_orbit"));
        assert!(extract(r#"{"results":[]}"#).unwrap().is_empty());
        assert!(extract(r#"{"results":[{"title":"x","snippet":"y"}]}"#).is_err());
    }
}
