//! XPath stand-ins over scraper's tree. ddgs selects nodes with lxml XPath (`engines/*.py`);
//! each helper below is one of those XPath shapes written out by hand, and the doc comment on
//! the caller names the original expression. Node-sets are returned in document order and
//! without duplicates, as XPath does.

use html5ever::driver::{self, ParseOpts};
use scraper::{ElementRef, Html, HtmlTreeSink};
use std::collections::HashSet;
use tendril::TendrilSink;

/// Parses a page the way lxml does for ddgs: html5ever with scripting disabled, so `<noscript>`
/// content is markup (lazy-load `<img>` inside it stays in the tree). scraper's own
/// `Html::parse_document` runs with scripting on and would make that content raw text.
pub fn parse(text: &str) -> Html {
    let mut opts = ParseOpts::default();
    opts.tree_builder.scripting_enabled = false;
    driver::parse_document(HtmlTreeSink::new(Html::new_document()), opts).one(text)
}

/// `//*` of the document (root included), document order
pub fn all_elements(doc: &Html) -> Vec<ElementRef<'_>> {
    let root = doc.root_element();
    let mut out = vec![root];
    out.extend(descendants(root));
    out
}

/// `.//*` of an element: strict descendant elements, document order
pub fn descendants(el: ElementRef<'_>) -> Vec<ElementRef<'_>> {
    el.descendants().skip(1).filter_map(ElementRef::wrap).collect()
}

/// `./*`: element children
pub fn children(el: ElementRef<'_>) -> Vec<ElementRef<'_>> {
    el.children().filter_map(ElementRef::wrap).collect()
}

/// `.//name`
pub fn desc_named<'a>(el: ElementRef<'a>, name: &str) -> Vec<ElementRef<'a>> {
    descendants(el).into_iter().filter(|e| named(*e, name)).collect()
}

/// `./name`
pub fn child_named<'a>(el: ElementRef<'a>, name: &str) -> Vec<ElementRef<'a>> {
    children(el).into_iter().filter(|e| named(*e, name)).collect()
}

/// `.//*[pred]`
pub fn desc_where<'a>(el: ElementRef<'a>, pred: impl Fn(ElementRef<'a>) -> bool) -> Vec<ElementRef<'a>> {
    descendants(el).into_iter().filter(|e| pred(*e)).collect()
}

/// `self::name`
pub fn named(el: ElementRef<'_>, name: &str) -> bool {
    el.value().name() == name
}

/// `contains(@class, needle)`
pub fn class_has(el: ElementRef<'_>, needle: &str) -> bool {
    el.value().attr("class").is_some_and(|c| c.contains(needle))
}

/// `@class = value`
pub fn class_is(el: ElementRef<'_>, value: &str) -> bool {
    el.value().attr("class") == Some(value)
}

/// `[@attr]`
pub fn has_attr(el: ElementRef<'_>, name: &str) -> bool {
    el.value().attr(name).is_some()
}

/// `[./child[pred]]`: some element child satisfies `pred`
pub fn has_child_where(el: ElementRef<'_>, pred: impl Fn(ElementRef<'_>) -> bool) -> bool {
    children(el).into_iter().any(pred)
}

/// `../name`: the parent is an element called `name`
pub fn parent_named(el: ElementRef<'_>, name: &str) -> bool {
    el.parent().and_then(ElementRef::wrap).is_some_and(|p| named(p, name))
}

/// `position()=last()` among the element siblings named `name`
pub fn is_last_child_named(el: ElementRef<'_>, name: &str) -> bool {
    let Some(parent) = el.parent() else { return false };
    parent
        .children()
        .filter_map(ElementRef::wrap)
        .filter(|c| named(*c, name))
        .last()
        .is_some_and(|last| last.id() == el.id())
}

/// an ancestor strictly between `el` and `root` satisfies `pred` (`root//x` shapes)
pub fn ancestor_below<'a>(el: ElementRef<'a>, root: ElementRef<'a>, pred: impl Fn(ElementRef<'a>) -> bool) -> bool {
    for node in el.ancestors() {
        if node.id() == root.id() {
            return false;
        }
        if ElementRef::wrap(node).is_some_and(|e| pred(e)) {
            return true;
        }
    }
    false
}

/// some ancestor of `el` (any depth) satisfies `pred`
pub fn has_ancestor<'a>(el: ElementRef<'a>, pred: impl Fn(ElementRef<'a>) -> bool) -> bool {
    el.ancestors().filter_map(ElementRef::wrap).any(|e| pred(e))
}

/// `"".join(xpath(".//x//text()"))` over a node-set: every text node under the elements, each
/// once, in document order (the elements may nest)
pub fn text_of(els: &[ElementRef<'_>]) -> String {
    let mut seen = HashSet::new();
    let mut out = String::new();
    for el in els {
        for node in el.descendants() {
            if let Some(text) = node.value().as_text() {
                if seen.insert(node.id()) {
                    out.push_str(&text.text);
                }
            }
        }
    }
    out
}

/// `"".join(xpath("./text()"))`: the direct text children of each element
pub fn direct_text(els: &[ElementRef<'_>]) -> String {
    let mut out = String::new();
    for el in els {
        for node in el.children() {
            if let Some(text) = node.value().as_text() {
                out.push_str(&text.text);
            }
        }
    }
    out
}

/// `text()[0]` of one element: the first direct text child, whitespace-only text included.
/// lxml keeps whitespace-only text nodes inside `span`/`a` (`//span/text()` on
/// `<span> <b>x</b>640x480</span>` is `[' ', '640x480']`), so python takes the blank one too.
pub fn first_text(el: ElementRef<'_>) -> Option<String> {
    el.children().find_map(|n| n.value().as_text().map(|t| t.text.to_string()))
}

/// `"".join(xpath("./@attr"))` over a node-set (attributes that are present, document order)
pub fn attr_concat(els: &[ElementRef<'_>], name: &str) -> String {
    els.iter().filter_map(|e| e.value().attr(name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_union_dedupes_nested_nodes_in_document_order() {
        let doc = parse("<div id=a>x<span>y<b>z</b></span>w</div>");
        let outer = all_elements(&doc).into_iter().find(|e| e.value().attr("id") == Some("a")).unwrap();
        let inner = desc_named(outer, "span");
        // outer text includes everything; the span adds nothing new
        assert_eq!(text_of(&[outer]), "xyzw");
        assert_eq!(text_of(&[outer, inner[0]]), "xyzw");
        assert_eq!(text_of(&inner), "yz");
    }

    #[test]
    fn noscript_content_is_markup_like_lxml() {
        let doc = parse("<div><noscript><img data-src=lazy.jpg></noscript></div>");
        let imgs = desc_named(all_elements(&doc)[0], "img");
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].value().attr("data-src"), Some("lazy.jpg"));
    }

    #[test]
    fn first_text_keeps_whitespace_only_text_nodes() {
        let doc = parse("<span class=nowrap> <b>x</b>640x480</span>");
        let span = desc_named(all_elements(&doc)[0], "span")[0];
        assert_eq!(first_text(span).as_deref(), Some(" "));
        let empty = parse("<span><b>x</b></span>");
        let span = desc_named(all_elements(&empty)[0], "span")[0];
        assert_eq!(first_text(span), None);
    }

    #[test]
    fn last_child_and_ancestor_predicates() {
        let doc = parse("<ul><div class=t></div><div class=b></div></ul>");
        let divs: Vec<_> = all_elements(&doc).into_iter().filter(|e| named(*e, "div")).collect();
        assert!(!is_last_child_named(divs[0], "div"));
        assert!(is_last_child_named(divs[1], "div"));
        assert!(has_ancestor(divs[0], |e| named(e, "ul")));
    }
}
