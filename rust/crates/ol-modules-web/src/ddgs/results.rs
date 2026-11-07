//! Result records and aggregation: `results.py` (dataclasses + BaseResult.__setattr__ normalisers),
//! `utils.py` (_normalize_text / _normalize_url / _normalize_date), `similarity.py`
//! (SimpleFilterRanker) and `ResultsAggregator` from `results.py`.

use super::{Category, DdgsError};
use crate::pytext::{html_unescape, py_len, py_split, url_unquote};
use crate::util::py_type_name;
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use ol_core::pyjson::{py_str, truthy};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use unicode_normalization::UnicodeNormalization;

/// one result dict, keys in dataclass field order (`result.__dict__`)
pub type Record = Map<String, Value>;

/// dataclass field names of a category's result type, in declaration order
pub fn fields(cat: Category) -> &'static [&'static str] {
    match cat {
        Category::Text => &["title", "href", "body"],
        Category::Images => &["title", "image", "thumbnail", "url", "height", "width", "source"],
        Category::News => &["date", "title", "body", "url", "image", "source"],
        Category::Videos => &[
            "title",
            "content",
            "description",
            "duration",
            "embed_html",
            "embed_url",
            "image_token",
            "images",
            "provider",
            "published",
            "publisher",
            "statistics",
            "uploader",
        ],
        Category::Books => &["title", "author", "publisher", "info", "url", "thumbnail"],
    }
}

/// `dataclass()` defaults: "" for strings, {} for the two dict fields of VideosResult
fn default_value(cat: Category, field: &str) -> Value {
    match (cat, field) {
        (Category::Videos, "images") | (Category::Videos, "statistics") => json!({}),
        _ => json!(""),
    }
}

/// a freshly constructed result with every field at its default
pub fn template(cat: Category) -> Record {
    let mut rec = Map::new();
    for field in fields(cat) {
        rec.insert((*field).to_string(), default_value(cat, field));
    }
    rec
}

/// `BaseResult.__setattr__`: a truthy value of a normalised field is normalised, others stored as-is
pub fn set(rec: &mut Record, field: &str, value: Value) -> Result<(), DdgsError> {
    let value = if truthy(&value) { normalize(field, value)? } else { value };
    rec.insert(field.to_string(), value);
    Ok(())
}

/// a result built from `(field, value)` pairs in the order given, starting from the template
pub fn build(cat: Category, pairs: Vec<(&str, Value)>) -> Result<Record, DdgsError> {
    let mut rec = template(cat);
    for (field, value) in pairs {
        set(&mut rec, field, value)?;
    }
    Ok(rec)
}

fn normalize(field: &str, value: Value) -> Result<Value, DdgsError> {
    match field {
        "title" | "body" | "author" | "publisher" | "info" => Ok(Value::String(normalize_text(text_arg(&value)?))),
        "href" | "url" | "thumbnail" | "image" => Ok(Value::String(normalize_url(url_arg(&value)?))),
        // _normalize_date: ints (bool included, as in python) become ISO timestamps, the rest pass through
        "date" => match &value {
            Value::Bool(b) => iso_from_timestamp(i64::from(*b)).map(Value::String),
            Value::Number(n) if n.is_i64() => iso_from_timestamp(n.as_i64().unwrap_or_default()).map(Value::String),
            _ => Ok(value),
        },
        _ => Ok(value),
    }
}

/// `_normalize_text(value)` on a truthy non-str: `re.sub` rejects it
fn text_arg(value: &Value) -> Result<&str, DdgsError> {
    value.as_str().ok_or_else(|| {
        DdgsError::Failed(format!("expected string or bytes-like object, got '{}'", py_type_name(value)))
    })
}

/// `_normalize_url(value)` on a truthy non-str: `unquote` runs `'%' not in value` (a TypeError for
/// scalars); for a list or dict that passes and `string.split` is the AttributeError
fn url_arg(value: &Value) -> Result<&str, DdgsError> {
    value.as_str().ok_or_else(|| {
        let t = py_type_name(value);
        DdgsError::Failed(match value {
            Value::Array(_) | Value::Object(_) => format!("'{t}' object has no attribute 'split'"),
            _ => format!("argument of type '{t}' is not iterable"),
        })
    })
}

/// `value.<name>` on a json value that is not the object python expects: AttributeError text
pub fn no_attr(value: &Value, name: &str) -> DdgsError {
    DdgsError::Failed(format!("'{}' object has no attribute '{name}'", py_type_name(value)))
}

/// python `value[index]` on a json value, with the exception python raises: IndexError on lists
/// and strs (strs index by character), KeyError on dicts (str of the int key), TypeError otherwise
pub fn py_getitem(value: &Value, index: usize) -> Result<Value, DdgsError> {
    match value {
        Value::Array(a) => a.get(index).cloned().ok_or_else(|| DdgsError::Failed("list index out of range".into())),
        Value::String(s) => s
            .chars()
            .nth(index)
            .map(|c| Value::String(c.to_string()))
            .ok_or_else(|| DdgsError::Failed("string index out of range".into())),
        Value::Object(_) => Err(DdgsError::Failed(index.to_string())),
        other => Err(DdgsError::Failed(format!("'{}' object is not subscriptable", py_type_name(other)))),
    }
}

/// python `value["key"]` on a json value, with the exception python raises (KeyError text is the repr)
pub fn py_key<'a>(value: &'a Value, key: &str) -> Result<&'a Value, DdgsError> {
    match value {
        Value::Object(o) => o.get(key).ok_or_else(|| DdgsError::Failed(format!("'{key}'"))),
        Value::Array(_) => Err(DdgsError::Failed("list indices must be integers or slices, not str".into())),
        Value::String(_) => Err(DdgsError::Failed("string indices must be integers, not 'str'".into())),
        other => Err(DdgsError::Failed(format!("'{}' object is not subscriptable", py_type_name(other)))),
    }
}

static TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new("<.*?>").unwrap());
static CATEGORY_C_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\p{C}").unwrap());

/// `_normalize_text`: strip tags, unescape entities, NFC, drop "C" category characters, collapse whitespace
pub fn normalize_text(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let text = TAG_RE.replace_all(raw, "");
    let text = html_unescape(&text);
    let text: String = text.nfc().collect();
    let text = CATEGORY_C_RE.replace_all(&text, "");
    py_split(&text).join(" ")
}

/// `_normalize_url`: unquote and turn spaces into '+'
pub fn normalize_url(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    url_unquote(url).replace(' ', "+")
}

/// `" ".join("".join(xpath_values).split())`: whitespace-collapsed text of an xpath result
pub fn collapse_ws(raw: &str) -> String {
    py_split(raw).join(" ")
}

/// `datetime.fromtimestamp(ts, timezone.utc).isoformat()`
pub fn iso_from_timestamp(ts: i64) -> Result<String, DdgsError> {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(iso_utc)
        .ok_or_else(|| DdgsError::Failed("timestamp out of range".into()))
}

/// `datetime.isoformat()` of a UTC datetime with microseconds already dropped
pub fn iso_utc(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

/// `str(value)` as used for the dedupe key (None -> "None")
fn key_text(v: Option<&Value>) -> String {
    py_str(v.unwrap_or(&Value::Null))
}

/// the field whose first occurrence in a result's `__dict__` is also one of ResultsAggregator's
/// cache fields {href, image, url, embed_url}
fn key_field(cat: Category) -> &'static str {
    match cat {
        Category::Text => "href",
        Category::Images => "image",
        Category::News | Category::Books => "url",
        Category::Videos => "embed_url",
    }
}

/// `len(item.__dict__.get("body", ""))`
fn body_len(cat: Category, rec: &Record) -> Result<usize, DdgsError> {
    if !matches!(cat, Category::Text | Category::News) {
        return Ok(0);
    }
    match rec.get("body") {
        None => Ok(0),
        Some(Value::String(s)) => Ok(py_len(s)),
        Some(other) => Err(DdgsError::Failed(format!("object of type '{}' has no len()", py_type_name(other)))),
    }
}

/// `ResultsAggregator`: dedupe by the key field, count occurrences, keep the longest body
pub struct Aggregator {
    cat: Category,
    order: Vec<String>,
    counts: HashMap<String, usize>,
    cache: HashMap<String, Record>,
}

impl Aggregator {
    pub fn new(cat: Category) -> Self {
        Self { cat, order: Vec::new(), counts: HashMap::new(), cache: HashMap::new() }
    }

    /// number of distinct items so far (`len(aggregator)`)
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// `extend`: stops at the first item that raises, keeping the items before it
    pub fn extend(&mut self, items: Vec<Record>) -> Result<(), DdgsError> {
        for item in items {
            self.append(item)?;
        }
        Ok(())
    }

    fn append(&mut self, item: Record) -> Result<(), DdgsError> {
        let key = key_text(item.get(key_field(self.cat)));
        let replace = match self.cache.get(&key) {
            None => true,
            Some(old) => {
                let new_len = body_len(self.cat, &item)?;
                let old_len = body_len(self.cat, old)?;
                new_len > old_len
            }
        };
        if replace {
            self.cache.insert(key.clone(), item);
        }
        if !self.counts.contains_key(&key) {
            self.order.push(key.clone());
        }
        *self.counts.entry(key).or_insert(0) += 1;
        Ok(())
    }

    /// `extract_dicts()`: items by descending frequency, ties in first-seen order
    pub fn extract(&self) -> Vec<Record> {
        let mut keys = self.order.clone();
        keys.sort_by(|a, b| self.counts[b].cmp(&self.counts[a]));
        keys.iter().map(|k| self.cache[k].clone()).collect()
    }
}

// python `re` `\w` on str is exactly categories Lu Ll Lt Lm Lo Nd Nl No plus "_" (verified over all code points);
// rust's `\W` is not the same (it counts marks like U+0301 as word characters), so the class is spelled out.
static TOKEN_SPLIT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[^\p{Lu}\p{Ll}\p{Lt}\p{Lm}\p{Lo}\p{Nd}\p{Nl}\p{No}_]+").unwrap());

/// `_extract_tokens`: lower-cased words of at least three characters
fn query_tokens(query: &str) -> Vec<String> {
    TOKEN_SPLIT_RE
        .split(&query.to_lowercase())
        .filter(|t| py_len(t) >= 3)
        .map(str::to_string)
        .collect()
}

fn has_any_token(text: &str, tokens: &[String]) -> bool {
    let lower = text.to_lowercase();
    tokens.iter().any(|t| lower.contains(t.as_str()))
}

/// the python errors a non-string field raises inside SimpleFilterRanker.rank
fn str_field(doc: &Record, field: &str) -> Result<Option<String>, DdgsError> {
    match doc.get(field) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(DdgsError::Failed(format!("argument of type '{}' is not iterable", py_type_name(other)))),
    }
}

/// `SimpleFilterRanker.rank`: wikipedia hits first, then title+body, title, body, neither
pub fn rank(docs: Vec<Record>, query: &str) -> Result<Vec<Record>, DdgsError> {
    let tokens = query_tokens(query);
    let mut wiki = Vec::new();
    let mut both = Vec::new();
    let mut title_only = Vec::new();
    let mut body_only = Vec::new();
    let mut neither = Vec::new();

    for doc in docs {
        let title = str_field(&doc, "title")?.unwrap_or_default();
        // skip Wikimedia category pages
        if title.contains("Category:") && title.contains("Wikimedia") {
            continue;
        }
        let href = str_field(&doc, "href")?.unwrap_or_default();
        if href.contains("wikipedia.org") {
            wiki.push(doc);
            continue;
        }
        // fallback to 'description' when there is no 'body' key
        let body_value = doc.get("body").or_else(|| doc.get("description"));
        let body = match body_value {
            None => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => return Err(DdgsError::Failed(format!("'{}' object has no attribute 'lower'", py_type_name(other)))),
        };
        match (has_any_token(&title, &tokens), has_any_token(&body, &tokens)) {
            (true, true) => both.push(doc),
            (true, false) => title_only.push(doc),
            (false, true) => body_only.push(doc),
            (false, false) => neither.push(doc),
        }
    }

    let mut out = wiki;
    out.extend(both);
    out.extend(title_only);
    out.extend(body_only);
    out.extend(neither);
    Ok(out)
}

/// the tail of `_search_sync`: rank, cut to `max_results`, or raise when nothing came back.
/// `err` is the last engine error seen (python's `err` variable).
pub fn finish(agg: &Aggregator, query: &str, max_results: Option<usize>, err: Option<DdgsError>) -> Result<Vec<Record>, DdgsError> {
    let ranked = rank(agg.extract(), query)?;
    if !ranked.is_empty() {
        return Ok(match max_results {
            Some(m) => ranked.into_iter().take(m).collect(),
            None => ranked,
        });
    }
    match err {
        Some(e) if e.message().contains("timed out") => Err(DdgsError::Timeout(e.message())),
        Some(e) => Err(DdgsError::Failed(e.message())),
        None => Err(DdgsError::Failed("No results found.".into())),
    }
}

/// `json.loads(text).get("results", [])` as the JSON engines iterate it
pub fn json_results(text: &str) -> Result<Vec<Value>, DdgsError> {
    let root: Value = crate::util::py_json_loads(text).map_err(DdgsError::Failed)?;
    let obj = root
        .as_object()
        .ok_or_else(|| DdgsError::Failed(format!("'{}' object has no attribute 'get'", py_type_name(&root))))?;
    match obj.get("results") {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        // `for item in x` over an empty dict or str yields nothing
        Some(Value::Object(o)) if o.is_empty() => Ok(Vec::new()),
        Some(Value::String(s)) if s.is_empty() => Ok(Vec::new()),
        // a non-empty dict yields its keys and a non-empty str its characters: both are str
        Some(Value::Object(_)) | Some(Value::String(_)) => Err(DdgsError::Failed("'str' object has no attribute 'get'".into())),
        // a for loop over an int, float, bool or None raises whatever its truthiness
        Some(other) => Err(DdgsError::Failed(format!("'{}' object is not iterable", py_type_name(other)))),
    }
}

/// one JSON result item as a dict (`item.get(key)` needs a dict)
pub fn json_item(item: &Value) -> Result<&Map<String, Value>, DdgsError> {
    item.as_object().ok_or_else(|| no_attr(item, "get"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_normaliser_matches_ddgs_utils() {
        assert_eq!(normalize_text("<b>Rust</b> &amp; \u{200b}Go\n\n  now"), "Rust & Go now");
        assert_eq!(normalize_text(""), "");
        // non-string truthy values raise, as _normalize_text does on int/bool
        let mut rec = template(Category::Text);
        assert!(set(&mut rec, "title", json!(5)).is_err());
        let text_err = set(&mut rec, "title", json!(5)).unwrap_err();
        assert_eq!(text_err.message(), "expected string or bytes-like object, got 'int'");
        let url_err = set(&mut rec, "url", json!(5)).unwrap_err();
        assert_eq!(url_err.message(), "argument of type 'int' is not iterable");
        assert_eq!(json_results(r#"{"results": {"a": 1}}"#).unwrap_err().message(), "'str' object has no attribute 'get'");
        assert_eq!(json_results(r#"{"results": 3}"#).unwrap_err().message(), "'int' object is not iterable");
        assert_eq!(json_results(r#"{"results": [3]}"#).unwrap().len(), 1);
        assert_eq!(json_item(&json!(3)).unwrap_err().message(), "'int' object has no attribute 'get'");
        assert_eq!(py_getitem(&json!(["a"]), 1).unwrap_err().message(), "list index out of range");
        assert_eq!(py_getitem(&json!({"a": 1}), 1).unwrap_err().message(), "1");
        assert_eq!(py_key(&json!([1]), "query").unwrap_err().message(), "list indices must be integers or slices, not str");
        assert_eq!(normalize_url("https://a.test/x%20y z"), "https://a.test/x+y+z");
    }

    #[test]
    fn query_tokens_split_like_python_re_word_class() {
        // python: re.split(r'\W+', q.lower()) filtered to len >= 3 (combining marks are separators, Nd/No are word chars)
        assert_eq!(query_tokens("abc\u{301}def"), vec!["abc", "def"]);
        assert_eq!(query_tokens("ab_cd\u{345}efg"), vec!["ab_cd", "efg"]);
        assert_eq!(query_tokens("caf\u{e9}-x\u{b2}abc"), vec!["caf\u{e9}", "x\u{b2}abc"]);
        assert_eq!(query_tokens("\u{4e2d}\u{6587}abc\u{661}\u{662}\u{663}xyz"), vec!["\u{4e2d}\u{6587}abc\u{661}\u{662}\u{663}xyz"]);
        assert_eq!(query_tokens("e\u{301}x"), Vec::<String>::new());
    }

    #[test]
    fn date_normaliser_turns_ints_into_utc_iso() {
        let mut rec = template(Category::News);
        set(&mut rec, "date", json!(1767225600)).unwrap();
        assert_eq!(rec["date"], json!("2026-01-01T00:00:00+00:00"));
        // falsy values are never normalised
        set(&mut rec, "date", json!(0)).unwrap();
        assert_eq!(rec["date"], json!(0));
    }

    #[test]
    fn aggregator_orders_by_frequency_then_first_seen_and_keeps_longest_body() {
        let mk = |href: &str, body: &str| {
            let mut r = template(Category::Text);
            r.insert("href".into(), json!(href));
            r.insert("title".into(), json!("t"));
            r.insert("body".into(), json!(body));
            r
        };
        let mut agg = Aggregator::new(Category::Text);
        agg.extend(vec![mk("https://a", "x"), mk("https://b", "bb")]).unwrap();
        agg.extend(vec![mk("https://b", "b"), mk("https://a", "a longer body"), mk("https://c", "c")]).unwrap();
        let out = agg.extract();
        let hrefs: Vec<&str> = out.iter().map(|r| r["href"].as_str().unwrap()).collect();
        // a and b both appear twice (a first seen), c once
        assert_eq!(hrefs, vec!["https://a", "https://b", "https://c"]);
        assert_eq!(out[0]["body"], json!("a longer body"));
        assert_eq!(out[1]["body"], json!("bb"));
    }

    #[test]
    fn ranker_puts_wikipedia_then_title_matches_and_drops_wikimedia_categories() {
        let mk = |title: &str, href: &str, body: &str| {
            let mut r = template(Category::Text);
            r.insert("title".into(), json!(title));
            r.insert("href".into(), json!(href));
            r.insert("body".into(), json!(body));
            r
        };
        let docs = vec![
            mk("Other", "https://x.test/1", "nothing"),
            mk("Rust language", "https://x.test/2", "nothing"),
            mk("Category: Wikimedia list", "https://x.test/3", "rust"),
            mk("Rust", "https://en.wikipedia.org/wiki/Rust", "rust"),
            mk("Body hit", "https://x.test/4", "about rust"),
        ];
        let out = rank(docs, "rust").unwrap();
        let hrefs: Vec<&str> = out.iter().map(|r| r["href"].as_str().unwrap()).collect();
        assert_eq!(
            hrefs,
            vec![
                "https://en.wikipedia.org/wiki/Rust",
                "https://x.test/2",
                "https://x.test/4",
                "https://x.test/1"
            ]
        );
    }

    #[test]
    fn empty_result_set_raises_no_results_or_timeout() {
        let agg = Aggregator::new(Category::Books);
        let err = finish(&agg, "q", Some(5), None).unwrap_err();
        assert_eq!(err.message(), "No results found.");
        let err = finish(&agg, "q", Some(5), Some(DdgsError::Timeout("request timed out".into()))).unwrap_err();
        assert!(matches!(err, DdgsError::Timeout(_)));
    }
}
