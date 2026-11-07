//! `engines/bing_images.py`: image results from bing.com/images/async (JSON metadata in `a.iusc`).

use super::html::{self, all_elements, child_named, class_has, class_is, desc_where, first_text, has_attr, named};
use super::http::{Method, Net, Session};
use super::results::{build, no_attr};
use super::{kv, lookup, Category, DdgsError, Payload, SearchArgs};
use crate::ddgs::results::Record;
use crate::pytext::{py_split, py_strip};
use scraper::ElementRef;
use serde_json::Value;

const URL: &str = "https://www.bing.com/images/async";

/// `max(int(kwargs.get("max_results", 10)), 35)`: max_results is never in kwargs, so it is always 35
const COUNT: i64 = 35;

/// `{'day': 1440, 'week': 10080, 'month': 44640, 'year': 525600}[timelimit]`
const QFT_AGE: &[(&str, &str)] = &[("day", "1440"), ("week", "10080"), ("month", "44640"), ("year", "525600")];

/// `build_payload`: q, async, first, count, then qft (timelimit)
pub fn build_payload(query: &str, args: &SearchArgs) -> Result<Payload, DdgsError> {
    let mut payload = vec![
        kv("q", query),
        kv("async", "1"),
        kv("first", (args.page - 1) * COUNT + 1),
        kv("count", COUNT),
    ];
    if let Some(tl) = args.timelimit() {
        // web_search passes d/w/m/y, which this table does not have: python raises KeyError
        payload.push(kv("qft", format!("filterui:age-lt{}", lookup(QFT_AGE, tl)?)));
    }
    Ok(payload)
}

/// `//div[./div[@class='imgpt']/a[@m] and ./div[@class='infopt']]`
fn is_item(e: ElementRef<'_>) -> bool {
    if !named(e, "div") {
        return false;
    }
    let has_imgpt = html::has_child_where(e, |c| {
        named(c, "div") && class_is(c, "imgpt") && html::has_child_where(c, |a| named(a, "a") && has_attr(a, "m"))
    });
    let has_infopt = html::has_child_where(e, |c| named(c, "div") && class_is(c, "infopt"));
    has_imgpt && has_infopt
}

/// `extract_results`: items that carry an `a.iusc[@m]` metadata blob
pub fn extract(html_text: &str) -> Result<Vec<Record>, DdgsError> {
    let doc = html::parse(html_text);
    let mut out = Vec::new();
    for item in all_elements(&doc).into_iter().filter(|e| is_item(*e)) {
        // .//a[@class='iusc']/@m  (first match only)
        let metadata = desc_where(item, |e| named(e, "a") && class_is(e, "iusc") && has_attr(e, "m"))
            .into_iter()
            .next()
            .and_then(|a| a.value().attr("m").map(str::to_string));
        let Some(metadata) = metadata else { continue };

        let m: Value = crate::util::py_json_loads(&metadata).map_err(DdgsError::Failed)?;
        // `m.get("t")` on a non-dict: python names the type of the decoded value
        let m = m.as_object().ok_or_else(|| no_attr(&m, "get"))?;
        let field = |key: &str| m.get(key).cloned().unwrap_or(Value::Null);
        let mut rec = build(
            Category::Images,
            vec![
                ("title", field("t")),
                ("image", field("murl")),
                ("thumbnail", field("turl")),
                ("url", field("purl")),
            ],
        )?;

        // .//div[contains(@class,'img_info')][./span]/span[@class='nowrap']/text()  (first text node)
        let dimension = desc_where(item, |e| named(e, "div") && class_has(e, "img_info") && html::has_child_where(e, |c| named(c, "span")))
            .into_iter()
            .flat_map(|div| child_named(div, "span"))
            .filter(|s| class_is(*s, "nowrap"))
            .find_map(first_text);
        if let Some(dimension) = dimension {
            // width, height = dimension.replace("×", "x").split("x")
            let dim = dimension.replace('×', "x");
            let parts: Vec<&str> = dim.split('x').collect();
            let [width, height] = parts.as_slice() else {
                let msg = if parts.len() < 2 {
                    format!("not enough values to unpack (expected 2, got {})", parts.len())
                } else {
                    format!("too many values to unpack (expected 2, got {})", parts.len())
                };
                return Err(DdgsError::Failed(msg));
            };
            // height.split()[0].strip()
            let height_token = py_split(height)
                .into_iter()
                .next()
                .ok_or_else(|| DdgsError::Failed("list index out of range".into()))?;
            rec.insert("width".into(), Value::String(py_strip(width).to_string()));
            rec.insert("height".into(), Value::String(py_strip(height_token).to_string()));
        }

        // .//div[@class='lnkw']//a/text()  (first text node)
        let source = desc_where(item, |e| named(e, "div") && class_is(e, "lnkw"))
            .into_iter()
            .flat_map(|div| desc_where(div, |e| named(e, "a")))
            .find_map(first_text);
        if let Some(source) = source {
            rec.insert("source".into(), Value::String(source));
        }
        out.push(rec);
    }
    Ok(out)
}

pub async fn search(net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
    let payload = build_payload(query, args)?;
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
    fn single_part_dimension_is_python_not_enough_values_error() {
        let html = r#"<div class="item"><div class="imgpt"><a class="iusc" m='{"t":"T","murl":"https://i/x.jpg","turl":"https://t/x.jpg","purl":"https://p/x"}'></a></div>
            <div class="infopt"><div class="img_info"><span class="nowrap">1024</span></div></div></div>"#;
        assert_eq!(extract(html).unwrap_err().message(), "not enough values to unpack (expected 2, got 1)");
        let two_x = html.replace("1024", "1x2x3");
        assert_eq!(extract(&two_x).unwrap_err().message(), "too many values to unpack (expected 2, got 3)");
    }

    #[test]
    fn whitespace_only_first_text_node_is_python_text_0() {
        // python: dimension[0] is the blank node ' ', split('x') has one part: unpack ValueError
        let blank_dim = r#"<div class="item"><div class="imgpt"><a class="iusc" m='{"t":"T","murl":"https://i/x.jpg","turl":"https://t/x.jpg","purl":"https://p/x"}'></a></div>
            <div class="infopt"><div class="img_info"><span class="nowrap"> <b>x</b>640x480</span></div></div></div>"#;
        assert_eq!(extract(blank_dim).unwrap_err().message(), "not enough values to unpack (expected 2, got 1)");
        // python: source[0] is the blank text node '\n  ' of <a>, kept as-is
        let blank_source = r#"<div class="item"><div class="imgpt"><a class="iusc" m='{"t":"T","murl":"https://i/x.jpg","turl":"https://t/x.jpg","purl":"https://p/x"}'></a></div>
            <div class="infopt"><div class="img_info"><span class="nowrap">640x480</span></div><div class="lnkw"><a>
  <img>Pet Site</a></div></div></div>"#;
        let out = extract(blank_source).unwrap();
        assert_eq!(out[0]["source"], serde_json::json!("\n  "));
    }

    #[test]
    fn payload_uses_count_35_and_python_qft_table() {
        let args = SearchArgs::default();
        let p = build_payload("dogs", &args).unwrap();
        assert_eq!(p, vec![kv("q", "dogs"), kv("async", "1"), kv("first", 1), kv("count", 35)]);
        let mut args = SearchArgs::default();
        args.page = 2;
        assert!(build_payload("dogs", &args).unwrap().contains(&kv("first", 36)));
        // the timelimit codes web_search sends (d/w/m/y) are not keys of the table: KeyError
        args.timelimit = Some("w".into());
        assert!(build_payload("dogs", &args).is_err());
        args.timelimit = Some("week".into());
        assert_eq!(build_payload("dogs", &args).unwrap().last().unwrap(), &kv("qft", "filterui:age-lt10080"));
    }

    #[test]
    fn extracts_metadata_dimensions_and_source() {
        let page = r#"<div class="imgpt"><a class="iusc" m='{"t":"Tabby cat","murl":"https://i.test/cat.jpg","turl":"https://t.test/cat.jpg","purl":"https://p.test/cat"}'></a></div>
            <div class="infopt">
              <div class="img_info"><span class="nowrap">640 × 480</span></div>
              <div class="lnkw"><a href="https://p.test/cat">Pet Site</a></div>
            </div>
            <div class="imgpt"><a class="x">no metadata</a></div><div class="infopt"></div>"#;
        let out = extract(&format!("<div class=\"wrap\">{page}</div>")).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["title"], serde_json::json!("Tabby cat"));
        assert_eq!(out[0]["image"], serde_json::json!("https://i.test/cat.jpg"));
        assert_eq!(out[0]["url"], serde_json::json!("https://p.test/cat"));
        assert_eq!(out[0]["width"], serde_json::json!("640"));
        assert_eq!(out[0]["height"], serde_json::json!("480"));
        assert_eq!(out[0]["source"], serde_json::json!("Pet Site"));
    }
}
