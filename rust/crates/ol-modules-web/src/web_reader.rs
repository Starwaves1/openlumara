//! Port of modules/web_reader/module.py: `WebReader`, a subclass of `Http` that scrapes the
//! title, headers, paragraphs, images, links and class/id text out of web pages.
//!
//! Python subclasses `modules.http.Http`, so `WebReader` reuses `HttpCore` by composition
//! (same security checks, rate limit, on_ready IP lookup). Its `settings` are inherited from
//! `Http` in python, so the registry descriptor uses `http::meta::settings`.

#[path = "../../../generated/modules/web_reader.rs"]
pub mod meta;

use crate::http::{HttpCore, Method, RequestOpts};
use crate::pytext::{py_strip, urlsplit};
use crate::sanitize::ContentSanitizer;
use crate::util::py_type_name;
use anyhow::{anyhow, bail};
use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::truthy;
use regex::Regex;
use scraper::{ElementRef, Html, Node, Selector};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// python `allowed_text_types`
const ALLOWED_TEXT_TYPES: &[&str] = &[
    "text/plain",
    "text/markdown",
    "text/x-markdown",
    "application/markdown",
    "application/json",
    "text/html",
    "application/xhtml+xml",
    "application/xml",
    "text/xml",
];

/// python `_process_webpage` class-name / id fallbacks, in python order
const FALLBACK_NAMES: &[&str] = &["content", "description", "title", "text", "article"];

const NOTHING_SCRAPED: &str = "nothing could be scraped from the page!";

pub struct WebReader {
    base: ModuleBase,
    core: HttpCore,
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("static CSS selector")
}

/// bs4 `HTMLTreeBuilder.DEFAULT_STRING_CONTAINERS`: tags whose text gets its own string class
const STRING_CONTAINERS: &[&str] = &["rt", "rp", "style", "script", "template"];

/// bs4 `get_text()`: a text node belongs to its nearest string-container ancestor (or to plain
/// text when there is none). `get_text` on a plain element keeps only plain text, while
/// `get_text` on a container element (e.g. `<rt class="content">`) keeps only its own class.
fn element_text(el: ElementRef) -> String {
    let own = STRING_CONTAINERS.iter().copied().find(|s| *s == el.value().name());
    let mut out = String::new();
    for node in el.descendants() {
        if let Node::Text(t) = node.value() {
            let container = node.ancestors().find_map(|a| match a.value() {
                Node::Element(e) => STRING_CONTAINERS.iter().copied().find(|s| *s == e.name()),
                _ => None,
            });
            if container == own {
                out.push_str(&t.text);
            }
        }
    }
    out
}

/// python `_remove_duplicates`: keep the first occurrence of each item, in order
fn dedupe(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items.into_iter().filter(|s| seen.insert(s.clone())).collect()
}

/// python `find_all(class_=re.compile(...))` and `find_all(id=re.compile(...))` text collection
fn fallback_classes(doc: &Html) -> Map<String, Value> {
    let all = sel("*");
    let mut out = Map::new();
    for name in FALLBACK_NAMES {
        let re = Regex::new(&format!(r"\b{}\b", name)).expect("static pattern");
        let mut items: Vec<String> = Vec::new();
        // class_: bs4 matches each class token of a multi-valued attribute
        for el in doc.select(&all) {
            let hit = el.attr("class").map(|c| c.split_whitespace().any(|t| re.is_match(t))).unwrap_or(false);
            if hit {
                let text = element_text(el);
                if !py_strip(&text).is_empty() {
                    items.push(py_strip(&text).to_string());
                }
            }
        }
        // id: the id string itself
        for el in doc.select(&all) {
            let hit = el.attr("id").map(|i| re.is_match(i)).unwrap_or(false);
            if hit {
                let text = element_text(el);
                if !py_strip(&text).is_empty() {
                    items.push(py_strip(&text).to_string());
                }
            }
        }
        let items = dedupe(items);
        if !items.is_empty() {
            let sanitized: Vec<Value> = items.iter().map(|s| json!(ContentSanitizer::sanitize_html_content(s))).collect();
            out.insert(name.to_string(), Value::Array(sanitized));
        }
    }
    out
}

/// python `_process_webpage(html)`: structured summary of an HTML/XML document
fn process_webpage(html: &str) -> Value {
    let doc = Html::parse_document(html);
    let mut output = Map::new();

    if let Some(title) = doc.select(&sel("title")).next() {
        output.insert("title".into(), json!(py_strip(&element_text(title))));
    }

    let headers = dedupe(
        doc.select(&sel("h1, h2, h3, h4, h5, h6")).map(|e| py_strip(&element_text(e)).to_string()).collect(),
    );
    let paragraphs = dedupe(doc.select(&sel("p")).map(|e| py_strip(&element_text(e)).to_string()).collect());
    let images = dedupe(
        doc.select(&sel("img"))
            .filter_map(|e| e.attr("alt"))
            .filter(|alt| !alt.is_empty())
            .map(|s| s.to_string())
            .collect(),
    );
    let urls = dedupe(doc.select(&sel("a[href]")).filter_map(|e| e.attr("href")).map(|s| s.to_string()).collect());

    let sanitize_all = |items: &[String]| -> Value {
        Value::Array(items.iter().map(|s| json!(ContentSanitizer::sanitize_html_content(s))).collect())
    };
    if !headers.is_empty() {
        output.insert("headers".into(), sanitize_all(&headers));
    }
    if !paragraphs.is_empty() {
        output.insert("paragraphs".into(), sanitize_all(&paragraphs));
    }
    if !images.is_empty() {
        output.insert("images".into(), sanitize_all(&images));
    }
    if !urls.is_empty() {
        output.insert("urls".into(), Value::Array(urls.into_iter().map(Value::String).collect()));
    }

    if headers.is_empty() && paragraphs.is_empty() {
        let classes = fallback_classes(&doc);
        if classes.is_empty() {
            output.insert("message".into(), json!(NOTHING_SCRAPED));
        } else {
            output.insert("classes".into(), Value::Object(classes));
        }
    }

    Value::Object(output)
}

/// python `for x in iterable` over a tool argument
fn py_iterate(v: &Value) -> anyhow::Result<Vec<Value>> {
    match v {
        Value::Array(a) => Ok(a.clone()),
        Value::String(s) => Ok(s.chars().map(|c| Value::String(c.to_string())).collect()),
        Value::Object(m) => Ok(m.keys().map(|k| Value::String(k.clone())).collect()),
        other => bail!("'{}' object is not iterable", py_type_name(other)),
    }
}

impl WebReader {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        let core = HttpCore::new(base.config.clone());
        Self { base, core }
    }

    /// python `asyncio.Semaphore(self.config.get("max_concurrent_tasks", 4))`
    fn max_concurrent_tasks(&self) -> anyhow::Result<usize> {
        let v = self.base.config.get1("max_concurrent_tasks").unwrap_or(json!(4));
        match &v {
            Value::Bool(b) => Ok(usize::from(*b)),
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.0);
                if f < 0.0 {
                    bail!("Semaphore initial value must be >= 0");
                }
                // asyncio admits a waiter while the float value is > 0, so 2.5 allows 3 holders
                Ok(f.ceil() as usize)
            }
            other => bail!("'<' not supported between instances of '{}' and 'int'", py_type_name(other)),
        }
    }

    /// python `read(path)`: every failure is returned as a result, never raised
    pub async fn read(&self, path: &Value) -> anyhow::Result<Value> {
        match self.read_inner(path).await {
            Ok(v) => Ok(v),
            Err(e) => Ok(self.base.result(format!("error {}", e), false)),
        }
    }

    async fn read_inner(&self, path: &Value) -> anyhow::Result<Value> {
        // urlparse(path): falsy non-strings parse as '', truthy non-strings fail on .decode()
        let path_str: &str = match path {
            Value::String(s) => s,
            other if truthy(other) => bail!("'{}' object has no attribute 'decode'", py_type_name(other)),
            _ => "",
        };
        let parsed = urlsplit(path_str).map_err(|e| anyhow!(e))?;
        if !matches!(parsed.scheme.as_str(), "http" | "https") {
            return Ok(self.base.result("Invalid URL. Please provide a valid http or https link.", false));
        }
        let domain = parsed.netloc.clone();

        let result = self
            .core
            .make_request(Method::Get, path_str, RequestOpts { include_content: true, ..Default::default() })
            .await?;
        let data = result.get("content").cloned().unwrap_or(Value::Null);
        let data_map = match &data {
            Value::Object(m) => m,
            _ => return Ok(self.base.result(data.clone(), false)),
        };

        // data.get("headers", {}).get("Content-Type", "").lower()
        let mut content_type = data_map
            .get("headers")
            .and_then(|h| h.get("Content-Type"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if py_strip(&content_type).is_empty() {
            content_type = "text/html".into();
        }
        let file_content = data_map.get("content").and_then(Value::as_str).unwrap_or("");

        if !ALLOWED_TEXT_TYPES.iter().any(|t| content_type.starts_with(t)) {
            return Ok(self.base.result(format!("Unsupported or disallowed content type: {}", content_type), false));
        }

        let output = if content_type.contains("html") || content_type.contains("xml") {
            process_webpage(file_content)
        } else {
            json!({"text": file_content})
        };
        let wrapped = self.core.wrap_untrusted(&output, Some(&format!("webpage:{}", domain)));
        Ok(self.base.result(wrapped, true))
    }

    /// python `read_multiple(paths)`: results in input order, at most max_concurrent_tasks in flight
    pub async fn read_multiple(&self, paths: &Value) -> anyhow::Result<Value> {
        let permits = self.max_concurrent_tasks()?;
        let items = py_iterate(paths)?;
        let sem = Semaphore::new(permits);
        let outcomes = futures::future::join_all(items.into_iter().map(|p| self.handle_one(&sem, p))).await;
        let results = outcomes.into_iter().collect::<anyhow::Result<Vec<Value>>>()?;
        Ok(self.base.result(Value::Array(results), true))
    }

    async fn handle_one(&self, sem: &Semaphore, p: Value) -> anyhow::Result<Value> {
        let _permit = sem.acquire().await?;
        let path = match &p {
            Value::Object(m) => m.get("path").cloned().ok_or_else(|| anyhow!("'path'"))?,
            _ => p.clone(),
        };
        match self.read(&path).await {
            Ok(v) => Ok(v),
            Err(e) => Ok(json!({"path": path, "error": e.to_string()})),
        }
    }
}

#[async_trait]
impl Module for WebReader {
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
        match method {
            "read" => self.read(&args.raw("path").cloned().unwrap_or(Value::Null)).await,
            "read_multiple" => self.read_multiple(&args.raw("paths").cloned().unwrap_or(Value::Null)).await,
            _ => bail!("'{}' object has no attribute '{}'", meta::CLASS_NAME, method),
        }
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

    fn detached() -> WebReader {
        WebReader::new(ModuleBase::detached("web_reader"))
    }

    #[tokio::test]
    async fn read_rejects_non_http_schemes_with_exact_message() {
        let r = detached();
        let out = r.call_tool("read", ToolArgs(json!({"path": "ftp://example.com/x"}).as_object().unwrap().clone())).await.unwrap();
        assert_eq!(out, json!({"status": "error", "content": "Invalid URL. Please provide a valid http or https link."}));
    }

    #[tokio::test]
    async fn read_non_string_path_matches_python_errors() {
        let r = detached();
        let out = r.read(&json!(5)).await.unwrap();
        assert_eq!(out, json!({"status": "error", "content": "error 'int' object has no attribute 'decode'"}));
        let out = r.read(&Value::Null).await.unwrap();
        assert_eq!(out, json!({"status": "error", "content": "Invalid URL. Please provide a valid http or https link."}));
    }

    #[tokio::test]
    async fn read_multiple_keeps_input_order_and_dict_paths() {
        let r = detached();
        let paths = json!(["ftp://a.example", {"path": "gopher://b.example"}]);
        let out = r.read_multiple(&paths).await.unwrap();
        let invalid = json!({"status": "error", "content": "Invalid URL. Please provide a valid http or https link."});
        assert_eq!(out, json!({"status": "success", "content": [invalid.clone(), invalid]}));
    }

    #[test]
    fn process_webpage_extracts_structure_in_python_order() {
        let html = r#"<html><head><title> T </title><script>var x=1;</script></head>
            <body><h1>Head</h1><p>Para one</p><p>Para one</p><img alt="pic"><img alt="">
            <a href="/a">a</a><a href="/a">a</a></body></html>"#;
        let out = process_webpage(html);
        assert_eq!(
            out,
            json!({"title": "T", "headers": ["Head"], "paragraphs": ["Para one"], "images": ["pic"], "urls": ["/a"]})
        );
        let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["title", "headers", "paragraphs", "images", "urls"]);
    }

    #[test]
    fn process_webpage_falls_back_to_class_and_id_text() {
        let html = r#"<html><body><div class="article-body"><span>Hello world</span></div>
            <div id="main-text">Body</div></body></html>"#;
        assert_eq!(process_webpage(html), json!({"classes": {"text": ["Body"], "article": ["Hello world"]}}));
    }

    #[test]
    fn element_text_skips_ruby_annotations_like_bs4() {
        // bs4 4.15: p.get_text() == '漢x' (rt and rp text is its own string class)
        assert_eq!(process_webpage("<p>漢<rt>kan</rt><rp>(</rp>x</p>"), json!({"paragraphs": ["漢x"]}));
        // bs4: div.get_text() == 'A' for the class fallback
        assert_eq!(process_webpage(r#"<div class="content">A<rt>B</rt></div>"#), json!({"classes": {"content": ["A"]}}));
    }

    #[test]
    fn element_text_container_element_keeps_its_own_text() {
        // bs4: rt.get_text() == 'kan' on a container element, its nested rp text is dropped
        assert_eq!(process_webpage(r#"<rt class="content">k<rp>(</rp>an</rt>"#), json!({"classes": {"content": ["kan"]}}));
        // bs4: script.get_text() == 'var a;' on a container element
        assert_eq!(process_webpage(r#"<script class="content">var a;</script>"#), json!({"classes": {"content": ["var a;"]}}));
    }

    #[test]
    fn process_webpage_reports_when_nothing_is_scraped() {
        assert_eq!(process_webpage("<html><body></body></html>"), json!({"message": "nothing could be scraped from the page!"}));
    }
}
