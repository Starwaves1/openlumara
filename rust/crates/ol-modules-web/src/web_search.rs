//! Port of modules/web_search/module.py: `WebSearch`, the `text / images / news / videos / books`
//! tools. Each one runs the ddgs library (`ddgs::Ddgs`), applies the display-time URL policy,
//! sanitizes the text fields and returns the results wrapped as untrusted external data.
//!
//! Python subclasses `modules.http.Http`, so the policy checks reuse `HttpCore` by composition
//! (same domain policy, same untrusted-data wrapper, same on_ready IP lookup).

#[path = "../../../generated/modules/web_search.rs"]
pub mod meta;

use crate::ddgs::results::Record;
use crate::ddgs::{Category, Ddgs, SearchArgs};
use crate::http::{http_log, HttpCore, ALLOWED_SCHEMES, DANGEROUS_PORTS};
use crate::pytext::{py_strip, urlsplit};
use crate::sanitize::{py_list_repr, ContentSanitizer};
use anyhow::bail;
use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::{as_i64, truthy};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const NO_QUERY: &str = "A non-empty search query is required.";
const FOLLOW_TEMPLATE: &str = "For each result, follow template: **Title**: Summary (URL). At the end, provide a conclusive summary of all results.";
const INVALID_DOMAIN: &str = "[invalid domain]";

pub struct WebSearch {
    base: ModuleBase,
    core: HttpCore,
}

/// python `kind` name of a tool (the DDGS method name)
fn kind_name(cat: Category) -> &'static str {
    match cat {
        Category::Text => "text",
        Category::Images => "images",
        Category::News => "news",
        Category::Videos => "videos",
        Category::Books => "books",
    }
}

/// `_search(kind, query, url_key, fields)` arguments per tool
fn shape(cat: Category) -> (&'static str, &'static [&'static str]) {
    match cat {
        Category::Text => ("href", &["title", "body"]),
        Category::Images => ("url", &["title", "image", "thumbnail"]),
        Category::News => ("url", &["title", "body"]),
        Category::Videos => ("content", &["title", "description"]),
        Category::Books => ("url", &["title", "author", "publisher", "info"]),
    }
}

/// the category kwargs each tool forwards to `DDGS.<kind>(...)`
fn category_keys(cat: Category) -> &'static [&'static str] {
    match cat {
        Category::Images => &["size", "color", "type_image", "layout", "license_image"],
        Category::Videos => &["resolution", "duration", "license_videos"],
        _ => &[],
    }
}

impl WebSearch {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        let core = HttpCore::new(base.config.clone());
        Self { base, core }
    }

    /// `_get_domain(url)`: the hostname for display, or "[invalid domain]"
    fn domain_of(url: &str) -> String {
        urlsplit(url).ok().and_then(|p| p.hostname()).unwrap_or_else(|| INVALID_DOMAIN.to_string())
    }

    /// `_url_passes_policy(url)`: scheme, https_only, hostname, dangerous ports and the domain
    /// lists. No DNS: the SSRF check of `Http` applies later, when a link is actually fetched.
    /// Err = python's ValueError from `parsed.port`, which aborts the whole search.
    fn url_passes_policy(&self, url: &str) -> Result<bool, String> {
        if url.is_empty() {
            return Ok(false);
        }
        let Ok(parsed) = urlsplit(url) else { return Ok(false) };
        let scheme = parsed.scheme.to_lowercase();
        if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
            return Ok(false);
        }
        if self.core.config.get_bool("https_only") && scheme != "https" {
            return Ok(false);
        }
        let Some(hostname) = parsed.hostname() else { return Ok(false) };
        if parsed.port()?.is_some_and(|port| DANGEROUS_PORTS.contains(&port)) {
            return Ok(false);
        }
        Ok(self.core.check_domain_policy(&hostname).is_ok())
    }

    /// `_clamp_max_results`: `max_results or config["max_results"]`, int(), at least 1
    fn clamp_max_results(&self, requested: &Value) -> usize {
        let requested = if truthy(requested) {
            requested.clone()
        } else {
            self.core.config.get1("max_results").unwrap_or(json!(5))
        };
        // int() of a value it cannot convert raises, and python falls back to 5
        as_i64(&requested).unwrap_or(5).max(1) as usize
    }

    /// the keyword arguments python passes to `DDGS.<kind>(query, **kwargs)`
    fn search_args(&self, cat: Category, args: &ToolArgs) -> SearchArgs {
        // python's tool default is 10; an explicit null is passed through like any other value
        let requested = args.0.get("max_results").cloned().unwrap_or(json!(10));
        let mut sa = SearchArgs {
            max_results: Some(self.clamp_max_results(&requested)),
            page: args.i64("page").unwrap_or(1),
            backend: args.str_or("backend", "auto"),
            ..SearchArgs::default()
        };
        // `books` does not forward region, safesearch or timelimit: DDGS uses its defaults
        if cat != Category::Books {
            sa.region = args.str_or("region", "us-en");
            sa.safesearch = args.str_or("safesearch", "moderate");
            sa.timelimit = args.str("timelimit");
        }
        for key in category_keys(cat) {
            if let Some(value) = args.str(key) {
                sa.extra.insert((*key).to_string(), value);
            }
        }
        sa
    }

    /// the per-result loop of `_run_search`: redacts results that fail the URL policy, sanitizes
    /// the text fields of the rest. Err = a policy check raised, which fails the whole search.
    fn sanitize_results(&self, url_key: &str, fields: &[&str], raw: Vec<Record>) -> Result<Vec<Value>, String> {
        let mut out = Vec::with_capacity(raw.len());
        for mut res in raw {
            let url = match res.get(url_key) {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            if !self.url_passes_policy(&url)? {
                let mut redacted = Map::new();
                for f in fields {
                    redacted.insert((*f).to_string(), json!("[REDACTED]"));
                }
                redacted.insert(url_key.to_string(), json!("[REDACTED]"));
                let domain = Self::domain_of(&url);
                redacted.insert(
                    "note".to_string(),
                    json!(format!("This result was rejected due to security policy (domain: {domain}).")),
                );
                out.push(Value::Object(redacted));
                continue;
            }
            for f in fields {
                // python `res.get(f, "")`: a missing field is stored as ""
                let original = res.get(*f).cloned().unwrap_or_else(|| json!(""));
                let value = match &original {
                    Value::String(s) if !py_strip(s).is_empty() => {
                        let checked = ContentSanitizer::sanitize_with_detection(s);
                        if matches!(checked.risk_level, "medium" | "high" | "critical") {
                            http_log(&format!(
                                "Search result injection detected in '{f}': {} (risk: {})",
                                py_list_repr(&checked.detection_result.patterns_found),
                                checked.risk_level
                            ));
                        }
                        Value::String(checked.sanitized_content)
                    }
                    _ => original,
                };
                res.insert((*f).to_string(), value);
            }
            out.push(Value::Object(res));
        }
        Ok(out)
    }

    /// `_search(kind, query, url_key, fields, **kwargs)`
    async fn search(&self, cat: Category, query: &Value, args: SearchArgs) -> Value {
        let query = match query {
            Value::String(q) if !py_strip(q).is_empty() => q,
            _ => return self.base.result(NO_QUERY, false),
        };
        let kind = kind_name(cat);
        let (url_key, fields) = shape(cat);
        let proxy = self.core.config.get_str("proxy");
        let ddgs = Ddgs::new(proxy.as_deref());
        let outcome = match ddgs.search_sync(cat, query, &args).await {
            Ok(raw) => self.sanitize_results(url_key, fields, raw),
            Err(e) => Err(e.to_string()),
        };
        match outcome {
            Ok(results) => {
                let wrapped = self.core.wrap_untrusted(&Value::Array(results), Some(&format!("web_search:{kind}")));
                self.base.result(Value::Array(vec![wrapped, Value::String(FOLLOW_TEMPLATE.to_string())]), true)
            }
            Err(e) => {
                http_log(&format!("{kind} search failed: {e}"));
                self.base.result(format!("An error occurred during {kind} search."), false)
            }
        }
    }
}

#[async_trait]
impl Module for WebSearch {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        // inherited from Http.on_ready
        self.core.on_ready().await;
        Ok(())
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        let cat = match method {
            "text" => Category::Text,
            "images" => Category::Images,
            "news" => Category::News,
            "videos" => Category::Videos,
            "books" => Category::Books,
            _ => bail!("'{}' object has no attribute '{}'", meta::CLASS_NAME, method),
        };
        let query = args.0.get("query").cloned().unwrap_or(Value::Null);
        let search_args = self.search_args(cat, &args);
        Ok(self.search(cat, &query, search_args).await)
    }

    fn ui_routes(&self) -> Vec<UiRoute> {
        meta::ui_routes().into_iter().map(|(r, _)| r).collect()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ol_core::module::validate_tool_args;

    fn detached() -> WebSearch {
        WebSearch::new(ModuleBase::detached("web_search"))
    }

    fn record(pairs: Vec<(&str, Value)>) -> Record {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[tokio::test]
    async fn blank_or_non_string_query_returns_the_python_error_string() {
        let ws = detached();
        for query in [json!("   "), json!(5), Value::Null] {
            let args = ToolArgs(json!({"query": query}).as_object().unwrap().clone());
            let out = ws.call_tool("text", args).await.unwrap();
            assert_eq!(out, json!({"status": "error", "content": "A non-empty search query is required."}));
        }
    }

    #[test]
    fn rejected_urls_are_redacted_with_the_python_note_in_field_order() {
        let ws = detached();
        let raw = vec![record(vec![
            ("title", json!("Bad")),
            ("body", json!("Body")),
            ("href", json!("ftp://evil.example/x")),
        ])];
        let out = ws.sanitize_results("href", &["title", "body"], raw).unwrap();
        assert_eq!(
            out,
            vec![json!({
                "title": "[REDACTED]",
                "body": "[REDACTED]",
                "href": "[REDACTED]",
                "note": "This result was rejected due to security policy (domain: evil.example)."
            })]
        );
        // the python dict keeps this key order in the JSON the model sees
        let keys: Vec<&String> = out[0].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["title", "body", "href", "note"]);
    }

    #[test]
    fn port_policy_and_missing_fields() {
        let ws = detached();
        // dangerous ports are refused at display time, whatever the scheme config says
        assert!(!ws.url_passes_policy("https://example.com:22/x").unwrap());
        assert!(ws.url_passes_policy("https://example.com/x").unwrap());
        // python's ValueError from parsed.port aborts the whole search
        assert!(ws.url_passes_policy("https://example.com:99999/x").is_err());
        // a missing text field is stored as "" and an empty one is not sanitized
        let raw = vec![record(vec![("href", json!("https://example.com/a")), ("title", json!("Hi")), ("body", json!(""))])];
        let out = ws.sanitize_results("href", &["title", "body", "extra"], raw).unwrap();
        assert_eq!(out[0]["title"], json!("Hi"));
        assert_eq!(out[0]["body"], json!(""));
        assert_eq!(out[0]["extra"], json!(""));
    }

    #[test]
    fn max_results_follows_python_clamp() {
        let ws = detached();
        assert_eq!(ws.clamp_max_results(&json!(-3)), 1);
        assert_eq!(ws.clamp_max_results(&json!("7")), 7);
        assert_eq!(ws.clamp_max_results(&json!(2.9)), 2);
        assert_eq!(ws.clamp_max_results(&json!("abc")), 5);
    }

    #[tokio::test]
    async fn books_do_not_forward_region_and_images_forward_category_kwargs() {
        let ws = detached();
        let args = ToolArgs(json!({"query": "dune", "region": "uk-en", "size": "Large", "max_results": 3}).as_object().unwrap().clone());
        let books = ws.search_args(Category::Books, &args);
        assert_eq!(books.region, "us-en");
        assert_eq!(books.max_results, Some(3));
        assert!(books.extra.is_empty());
        let images = ws.search_args(Category::Images, &args);
        assert_eq!(images.region, "uk-en");
        assert_eq!(images.extra("size"), "Large");
    }

    #[test]
    fn binding_errors_match_python_typeerror_text() {
        // python raises these at call binding; the dispatcher validates against the tool spec
        let spec = |method: &str| meta::tools().into_iter().find(|s| s.method == method).unwrap();
        let books_args = json!({"query": "dune", "region": "uk-en"});
        assert_eq!(
            validate_tool_args("WebSearch", &spec("books"), books_args.as_object().unwrap()).unwrap_err(),
            "WebSearch.books() got an unexpected keyword argument 'region'"
        );
        assert_eq!(
            validate_tool_args("WebSearch", &spec("text"), &Map::new()).unwrap_err(),
            "WebSearch.text() missing 1 required positional argument: 'query'"
        );
        // an explicit null query is bound fine and reaches the python-style error result
        let null_query = json!({"query": null});
        assert!(validate_tool_args("WebSearch", &spec("text"), null_query.as_object().unwrap()).is_ok());
    }
}
