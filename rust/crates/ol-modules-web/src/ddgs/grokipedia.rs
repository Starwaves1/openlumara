//! `engines/grokipedia.py`: the first typeahead hit of grokipedia.com.

use super::http::{Method, Net, Session};
use super::results::{build, no_attr, py_getitem, Record};
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
    let json: Value = crate::util::py_json_loads(text).map_err(DdgsError::Failed)?;
    let obj = json.as_object().ok_or_else(|| no_attr(&json, "get"))?;
    // `items = json_data.get("results", [])`; `if not items: return []`
    let items = obj.get("results").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
    if !truthy(&items) {
        return Ok(Vec::new());
    }
    // `items[0].get(...)`: the item must be a dict
    let item0 = py_getitem(&items, 0)?;
    let first: &Map<String, Value> = item0.as_object().ok_or_else(|| no_attr(&item0, "get"))?;

    // items[0].get("title", "").strip("_")
    let title = match first.get("title") {
        None => String::new(),
        Some(Value::String(s)) => s.trim_matches('_').to_string(),
        Some(other) => return Err(DdgsError::Failed(format!("'{}' object has no attribute 'strip'", py_type(other)))),
    };
    // body = items[0].get("snippet", ""); body.split("\n\n", 1)[1] if "\n\n" in body else body
    let body = match first.get("snippet") {
        None => Value::String(String::new()),
        Some(Value::String(snippet)) => match snippet.split_once("\n\n") {
            Some((_, rest)) => Value::String(rest.to_string()),
            None => Value::String(snippet.clone()),
        },
        // `in` on a list is an element comparison: only a "\n\n" element makes it true, then split fails
        Some(Value::Array(items)) => {
            if items.iter().any(|v| v.as_str() == Some("\n\n")) {
                return Err(DdgsError::Failed("'list' object has no attribute 'split'".into()));
            }
            Value::Array(items.clone())
        }
        // a dict's `in` tests its keys, which are all str, so "\n\n" is never one of them
        Some(obj @ Value::Object(_)) => obj.clone(),
        Some(other) => return Err(DdgsError::Failed(format!("argument of type '{}' is not iterable", py_type(other)))),
    };
    // items[0]['slug']
    let slug = first.get("slug").ok_or_else(|| DdgsError::Failed("'slug'".into()))?;
    let href = format!("https://grokipedia.com/page/{}", py_str(slug));

    Ok(vec![build(
        Category::Text,
        vec![("title", title.into()), ("href", href.into()), ("body", body)],
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

    #[test]
    fn snippet_shapes_fail_like_python_base_result_and_membership() {
        let snippet = |v: &str| format!(r#"{{"results":[{{"title":"t","snippet":{v},"slug":"s"}}]}}"#);
        // a list is `in`-tested by element: no "\n\n" element leaves it alone, the setattr normaliser rejects it
        assert_eq!(extract(&snippet(r#"["a"]"#)).unwrap_err().message(), "expected string or bytes-like object, got 'list'");
        assert_eq!(extract(&snippet(r#"{"a":1}"#)).unwrap_err().message(), "expected string or bytes-like object, got 'dict'");
        assert_eq!(extract(&snippet(r#"["\n\n"]"#)).unwrap_err().message(), "'list' object has no attribute 'split'");
        assert_eq!(extract(&snippet("5")).unwrap_err().message(), "argument of type 'int' is not iterable");
        assert_eq!(extract(&snippet("null")).unwrap_err().message(), "argument of type 'NoneType' is not iterable");
        assert_eq!(extract("{").unwrap_err().message(), "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)");
    }

    #[test]
    fn non_dict_shapes_raise_python_exception_text() {
        assert_eq!(extract("[1]").unwrap_err().message(), "'list' object has no attribute 'get'");
        assert_eq!(extract(r#"{"results":5}"#).unwrap_err().message(), "'int' object is not subscriptable");
        assert_eq!(extract(r#"{"results":[5]}"#).unwrap_err().message(), "'int' object has no attribute 'get'");
        assert_eq!(extract(r#"{"results":{"a":1}}"#).unwrap_err().message(), "0");
        assert_eq!(extract(r#"{"results":"abc"}"#).unwrap_err().message(), "'str' object has no attribute 'get'");
    }
}
