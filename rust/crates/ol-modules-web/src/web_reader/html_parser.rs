//! Port of python's `html.parser.HTMLParser` (3.14, `convert_charrefs=False`) driven the way
//! bs4's `html.parser` tree builder drives it (`BeautifulSoupHTMLParser` plus the
//! `BeautifulSoup` tree construction in bs4 4.15). Python's regexes are written out by hand:
//! the `regex` crate has no lookaround, and the python patterns depend on it.
//!
//! The result is an arena of elements and text nodes in document order. Comments, doctypes,
//! declarations and processing instructions are not kept. bs4 keeps them, but `get_text()`
//! never includes them, and they only matter here as boundaries that split text runs.

use crate::entities;
use crate::pytext::{html_unescape, py_isspace, py_strip};
use once_cell::sync::Lazy;
use std::collections::{HashMap, HashSet};

/// bs4 `HTMLTreeBuilder.DEFAULT_EMPTY_ELEMENT_TAGS`
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "keygen", "link", "menuitem", "meta", "param", "source",
    "track", "wbr", "basefont", "bgsound", "command", "frame", "image", "isindex", "nextid", "spacer",
];

/// bs4 `DEFAULT_PRESERVE_WHITESPACE_TAGS`
const PRESERVE_WHITESPACE: &[&str] = &["pre", "textarea"];

/// bs4 `DEFAULT_STRING_CONTAINERS`: text inside these tags gets its own string class
const STRING_CONTAINERS: &[&str] = &["rt", "rp", "style", "script", "template"];

/// python `HTMLParser.CDATA_CONTENT_ELEMENTS` (raw text)
const CDATA_ELEMENTS: &[&str] = &["script", "style", "xmp", "iframe", "noembed", "noframes"];

/// python `HTMLParser.RCDATA_CONTENT_ELEMENTS` (escapable raw text)
const RCDATA_ELEMENTS: &[&str] = &["textarea", "title"];

/// python `html.entities.html5`, keyed exactly as python keys it (with and without `;`)
static HTML5_KEYS: Lazy<HashSet<&'static str>> = Lazy::new(|| entities::HTML5.iter().map(|(k, _)| *k).collect());

/// bs4 `EntitySubstitution.HTML_ENTITY_TO_CHARACTER`: html5 names with the `;` stripped
static ENTITY_TO_CHAR: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let mut m = HashMap::new();
    for (k, v) in entities::HTML5.iter() {
        m.entry(k.strip_suffix(';').unwrap_or(k)).or_insert(*v);
    }
    m
});

#[derive(Debug)]
pub enum Kind {
    Root,
    Element { name: String, attrs: Vec<(String, String)> },
    /// a bs4 NavigableString. `container` is the string-container tag it sits in (`rt`, `script`,
    /// ...), or None for plain text and CDATA sections
    Text { text: String, container: Option<&'static str> },
}

#[derive(Debug)]
pub struct Node {
    pub kind: Kind,
    pub children: Vec<usize>,
}

/// A parsed document. Node 0 is the document root.
#[derive(Debug)]
pub struct Document {
    nodes: Vec<Node>,
}

impl Document {
    /// Element ids in document order whose tag name satisfies `pred`.
    pub fn elements<F: Fn(&str) -> bool>(&self, pred: F) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![0usize];
        while let Some(id) = stack.pop() {
            if let Kind::Element { name, .. } = &self.nodes[id].kind {
                if pred(name) {
                    out.push(id);
                }
            }
            stack.extend(self.nodes[id].children.iter().rev().copied());
        }
        out
    }

    /// Attribute value of element `id` (last duplicate wins, as bs4 does).
    pub fn attr(&self, id: usize, key: &str) -> Option<&str> {
        match &self.nodes[id].kind {
            Kind::Element { attrs, .. } => attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()),
            _ => None,
        }
    }

    /// bs4 `Tag.get_text()`. A plain tag keeps its plain text and CDATA; a string-container tag
    /// (e.g. `<rt>`, `<script>`) keeps only the text whose string class is its own.
    pub fn get_text(&self, id: usize) -> String {
        let own: Option<&'static str> = match &self.nodes[id].kind {
            Kind::Element { name, .. } => STRING_CONTAINERS.iter().copied().find(|c| *c == name.as_str()),
            _ => None,
        };
        let mut out = String::new();
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            if let Kind::Text { text, container } = &self.nodes[cur].kind {
                if *container == own {
                    out.push_str(text);
                }
            }
            stack.extend(self.nodes[cur].children.iter().rev().copied());
        }
        out
    }
}

/// Parse `html` the way bs4 with the `html.parser` builder does.
pub fn parse(html: &str) -> Result<Document, String> {
    let mut p = HtmlParser::new(html);
    p.goahead(false)?;
    p.goahead(true)?;
    p.end_data(false);
    Ok(Document { nodes: p.nodes })
}

// ---------------------------------------------------------------------------
// character classes
// ---------------------------------------------------------------------------

/// `[\t\n\r\f ]`
fn is_tag_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\x0c' | ' ')
}

/// `[^\t\n\r\f />]`: a character that may end a tag name
fn is_tag_stop(c: char) -> bool {
    is_tag_ws(c) || c == '/' || c == '>'
}

/// `[^\t\n\r\f /=>]`: a character that may end an attribute name
fn is_attr_stop(c: char) -> bool {
    is_tag_stop(c) || c == '='
}

/// `(?<=['"\t\n\r\f /])`: characters that may directly precede an attribute name
fn is_attr_lookbehind(c: char) -> bool {
    is_tag_ws(c) || matches!(c, '\'' | '"' | '/')
}

fn is_ascii_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// bs4 `endData` collapses text made only of ASCII spaces to "\n" or " "
fn is_ascii_space(c: char) -> bool {
    matches!(c, ' ' | '\n' | '\t' | '\x0c' | '\r')
}

fn own_container(name: &str) -> Option<&'static str> {
    STRING_CONTAINERS.iter().copied().find(|c| *c == name)
}

// ---------------------------------------------------------------------------
// tree building (bs4 BeautifulSoup + BeautifulSoupHTMLParser)
// ---------------------------------------------------------------------------

struct HtmlParser {
    raw: Vec<char>,
    /// index into `raw` where the next `goahead` call starts (python `rawdata` offset)
    pos: usize,
    nodes: Vec<Node>,
    /// open elements; `stack[0]` is the document root (bs4 `tagStack`)
    stack: Vec<usize>,
    /// how many open elements have each name (bs4 `open_tag_counter`)
    open_count: HashMap<String, usize>,
    /// open string-container elements with their string class (bs4 `string_container_stack`)
    containers: Vec<(usize, &'static str)>,
    /// how many open `pre`/`textarea` elements (bs4 `preserve_whitespace_tag_stack`)
    preserve: usize,
    /// bs4 `current_data`: text since the last tag, or None when nothing is pending
    pending: Option<String>,
    /// void elements closed as soon as they open (bs4 `already_closed_empty_element`)
    already_closed: Vec<String>,
    cdata_elem: Option<String>,
    escapable: bool,
}

impl HtmlParser {
    fn new(html: &str) -> Self {
        Self {
            raw: html.chars().collect(),
            pos: 0,
            nodes: vec![Node { kind: Kind::Root, children: Vec::new() }],
            stack: vec![0],
            open_count: HashMap::new(),
            containers: Vec::new(),
            preserve: 0,
            pending: None,
            already_closed: Vec::new(),
            cdata_elem: None,
            escapable: true,
        }
    }

    // ----- raw access -------------------------------------------------------

    fn ch(&self, i: usize) -> Option<char> {
        self.raw.get(i).copied()
    }

    fn text(&self, a: usize, b: usize) -> String {
        let n = self.raw.len();
        let (a, b) = (a.min(n), b.min(n));
        if a >= b {
            String::new()
        } else {
            self.raw[a..b].iter().collect()
        }
    }

    fn starts_with(&self, i: usize, s: &str) -> bool {
        s.chars().enumerate().all(|(k, c)| self.ch(i + k) == Some(c))
    }

    fn starts_with_ci(&self, i: usize, s: &str) -> bool {
        s.chars().enumerate().all(|(k, c)| self.ch(i + k).is_some_and(|r| r.eq_ignore_ascii_case(&c)))
    }

    fn find_char(&self, from: usize, c: char) -> Option<usize> {
        (from..self.raw.len()).find(|&k| self.raw[k] == c)
    }

    fn find_str(&self, from: usize, s: &str) -> Option<usize> {
        let pat: Vec<char> = s.chars().collect();
        (from..self.raw.len()).find(|&k| (0..pat.len()).all(|t| self.ch(k + t) == Some(pat[t])))
    }

    /// `<[a-zA-Z]`
    fn starttagopen(&self, i: usize) -> bool {
        self.ch(i) == Some('<') && self.ch(i + 1).is_some_and(|c| c.is_ascii_alphabetic())
    }

    /// `</[a-zA-Z]`
    fn endtagopen(&self, i: usize) -> bool {
        self.ch(i) == Some('<') && self.ch(i + 1) == Some('/') && self.ch(i + 2).is_some_and(|c| c.is_ascii_alphabetic())
    }

    // ----- tree building ----------------------------------------------------

    fn handle_data(&mut self, s: &str) {
        self.pending.get_or_insert_with(String::new).push_str(s);
    }

    /// bs4 `endData(containerClass)`: turn the pending text into a node under the open element.
    fn end_data(&mut self, cdata: bool) {
        let Some(mut s) = self.pending.take() else { return };
        if self.preserve == 0 && s.chars().all(is_ascii_space) {
            s = if s.contains('\n') { "\n".to_string() } else { " ".to_string() };
        }
        let container = if cdata { None } else { self.containers.last().map(|(_, c)| *c) };
        self.add_child(Kind::Text { text: s, container });
    }

    fn add_child(&mut self, kind: Kind) {
        let id = self.nodes.len();
        self.nodes.push(Node { kind, children: Vec::new() });
        let parent = *self.stack.last().expect("document root stays on the stack");
        self.nodes[parent].children.push(id);
    }

    /// bs4 `handle_starttag` + `pushTag`
    fn push_element(&mut self, name: &str, attrs: Vec<(String, String)>) {
        self.end_data(false);
        let id = self.nodes.len();
        self.add_child(Kind::Element { name: name.to_string(), attrs });
        self.stack.push(id);
        *self.open_count.entry(name.to_string()).or_insert(0) += 1;
        if PRESERVE_WHITESPACE.contains(&name) {
            self.preserve += 1;
        }
        if let Some(c) = own_container(name) {
            self.containers.push((id, c));
        }
    }

    /// bs4 `popTag`
    fn pop_top(&mut self) {
        if self.stack.len() <= 1 {
            return;
        }
        let id = self.stack.pop().expect("checked above");
        if let Kind::Element { name, .. } = &self.nodes[id].kind {
            if let Some(c) = self.open_count.get_mut(name) {
                *c = c.saturating_sub(1);
            }
            if PRESERVE_WHITESPACE.contains(&name.as_str()) {
                self.preserve -= 1;
            }
        }
        if self.containers.last().is_some_and(|(top, _)| *top == id) {
            self.containers.pop();
        }
    }

    /// bs4 `_popToTag`: pop up to and including the most recent open element with this name
    fn pop_to_tag(&mut self, name: &str) {
        while self.stack.len() > 1 {
            if self.open_count.get(name).copied().unwrap_or(0) == 0 {
                break;
            }
            let top = *self.stack.last().expect("len > 1");
            let is_match = matches!(&self.nodes[top].kind, Kind::Element { name: n, .. } if n == name);
            self.pop_top();
            if is_match {
                break;
            }
        }
    }

    /// bs4 `soup.handle_endtag`
    fn end_tag(&mut self, name: &str) {
        self.end_data(false);
        self.pop_to_tag(name);
    }

    /// bs4 `BeautifulSoupHTMLParser.handle_starttag`
    fn handle_starttag(&mut self, name: &str, attrs: Vec<(String, String)>, handle_empty_element: bool) {
        // later duplicates replace the value, keeping the first position (python dict semantics)
        let mut dict: Vec<(String, String)> = Vec::new();
        for (k, v) in attrs {
            match dict.iter_mut().find(|(existing, _)| *existing == k) {
                Some(slot) => slot.1 = v,
                None => dict.push((k, v)),
            }
        }
        self.push_element(name, dict);
        if VOID_ELEMENTS.contains(&name) && handle_empty_element {
            self.end_tag(name);
            self.already_closed.push(name.to_string());
        }
    }

    /// bs4 `BeautifulSoupHTMLParser.handle_startendtag`
    fn handle_startendtag(&mut self, name: &str, attrs: Vec<(String, String)>) {
        self.handle_starttag(name, attrs, false);
        self.end_tag(name);
    }

    /// bs4 `BeautifulSoupHTMLParser.handle_endtag`
    fn handle_endtag(&mut self, name: &str) {
        if let Some(pos) = self.already_closed.iter().position(|t| t == name) {
            self.already_closed.remove(pos);
        } else {
            self.end_tag(name);
        }
    }

    /// bs4 handle_comment / handle_decl / handle_pi: flush the text before, keep nothing
    fn skip_node(&mut self) {
        self.end_data(false);
    }

    /// bs4 `unknown_decl`: a `<![CDATA[...]]>` becomes CDATA text, anything else is dropped
    fn unknown_decl(&mut self, data: String) {
        let is_cdata = data.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("CDATA["));
        if !is_cdata {
            self.skip_node();
            return;
        }
        self.end_data(false);
        self.pending = Some(data[6..].to_string());
        self.end_data(true);
    }

    /// bs4 `handle_charref`
    fn handle_charref(&mut self, name: &str) {
        let (dereferenced, extra) = deref_numeric(name);
        self.handle_data(&dereferenced);
        self.handle_data(&extra);
    }

    /// bs4 `handle_entityref`
    fn handle_entityref(&mut self, name: &str) {
        match ENTITY_TO_CHAR.get(name) {
            Some(c) => self.handle_data(c),
            None => {
                let data = format!("&{}", name);
                self.handle_data(&data);
            }
        }
    }

    // ----- cdata mode -------------------------------------------------------

    fn set_cdata_mode(&mut self, elem: &str, escapable: bool) {
        self.cdata_elem = Some(elem.to_lowercase());
        self.escapable = escapable;
    }

    fn clear_cdata_mode(&mut self) {
        self.cdata_elem = None;
        self.escapable = true;
    }

    /// python `self.interesting.search(rawdata, i)`
    fn next_interesting(&self, from: usize) -> Option<usize> {
        let n = self.raw.len();
        match self.cdata_elem.as_deref() {
            None => (from..n).find(|&p| matches!(self.raw[p], '&' | '<')),
            Some("plaintext") => Some(n),
            Some(elem) => {
                let escapable = self.escapable;
                (from..n).find(|&p| {
                    if escapable && self.raw[p] == '&' {
                        return true;
                    }
                    if self.raw[p] != '<' || self.ch(p + 1) != Some('/') {
                        return false;
                    }
                    let name_ok = elem
                        .chars()
                        .enumerate()
                        .all(|(k, c)| self.ch(p + 2 + k).is_some_and(|r| r.eq_ignore_ascii_case(&c)));
                    name_ok && self.ch(p + 2 + elem.chars().count()).is_some_and(|c| is_tag_ws(c) || c == '/' || c == '>')
                })
            }
        }
    }

    // ----- python regex equivalents -----------------------------------------

    /// `tagfind_tolerant` at `i` (a letter): returns the tag name and the end of the match
    fn tagfind(&self, i: usize) -> (String, usize) {
        let n = self.raw.len();
        let mut j = i + 1;
        while j < n && !is_tag_stop(self.raw[j]) {
            j += 1;
        }
        let name = self.text(i, j);
        j = self.skip_tolerant_separators(j);
        (name, j)
    }

    /// `(?:[\t\n\r\f ]|/(?!>))*`
    fn skip_tolerant_separators(&self, mut j: usize) -> usize {
        loop {
            match self.ch(j) {
                Some(c) if is_tag_ws(c) => j += 1,
                Some('/') if self.ch(j + 1) != Some('>') => j += 1,
                _ => return j,
            }
        }
    }

    /// The optional `[\t\n\r\f ]*=[\t\n\r\f ]*(value)` group after an attribute name at `p`.
    /// Returns (value start, value end). None when the group does not match.
    fn value_group(&self, p: usize) -> Option<(usize, usize)> {
        let n = self.raw.len();
        let mut q = p;
        while q < n && is_tag_ws(self.raw[q]) {
            q += 1;
        }
        if self.ch(q) != Some('=') {
            return None;
        }
        q += 1;
        while q < n && is_tag_ws(self.raw[q]) {
            q += 1;
        }
        let vs = q;
        match self.ch(q) {
            Some(quote @ ('\'' | '"')) => {
                let close = (q + 1..n).find(|&k| self.raw[k] == quote)?;
                Some((vs, close + 1))
            }
            _ => {
                let mut e = q;
                while e < n && !matches!(self.raw[e], '>' | '\t' | '\n' | '\r' | '\x0c' | ' ') {
                    e += 1;
                }
                Some((vs, e))
            }
        }
    }

    /// `attrfind_tolerant` at `k`. Returns (name, raw value incl. quotes if any, match end).
    fn attrfind(&self, k: usize) -> Option<(String, Option<String>, usize)> {
        let n = self.raw.len();
        if k == 0 || !is_attr_lookbehind(self.raw[k - 1]) {
            return None;
        }
        let first = self.ch(k)?;
        if is_tag_stop(first) {
            return None;
        }
        let mut j = k + 1;
        while j < n && !is_attr_stop(self.raw[j]) {
            j += 1;
        }
        let name = self.text(k, j);
        let (value, end) = match self.value_group(j) {
            Some((vs, ve)) => (Some(self.text(vs, ve)), ve),
            None => (None, j),
        };
        Some((name, value, self.skip_tolerant_separators(end)))
    }

    /// `locatetagend` matched at `i` (the first letter of a tag name). Returns the match end.
    fn locatetagend(&self, i: usize) -> usize {
        let n = self.raw.len();
        let mut p = i + 1;
        while p < n && !is_tag_stop(self.raw[p]) {
            p += 1;
        }
        while p < n && (is_tag_ws(self.raw[p]) || self.raw[p] == '/') {
            p += 1;
        }
        loop {
            let starts_attr = p > 0 && p < n && is_attr_lookbehind(self.raw[p - 1]) && !is_tag_stop(self.raw[p]);
            if !starts_attr {
                break;
            }
            p += 1;
            while p < n && !is_attr_stop(self.raw[p]) {
                p += 1;
            }
            if let Some((_, ve)) = self.value_group(p) {
                p = ve;
            }
            while p < n && (is_tag_ws(self.raw[p]) || self.raw[p] == '/') {
                p += 1;
            }
        }
        if self.ch(p) == Some('>') {
            p += 1;
        }
        p
    }

    /// python `check_for_whole_start_tag`
    fn check_for_whole_start_tag(&self, i: usize) -> Option<usize> {
        let j = self.locatetagend(i);
        if self.ch(j - 1) != Some('>') {
            return None;
        }
        Some(j)
    }

    /// `&#(?:[0-9]+|[xX][0-9a-fA-F]+)[^0-9a-fA-F]` at `i`. Returns (name, index after terminator).
    fn charref_at(&self, i: usize) -> Option<(String, usize)> {
        let p = i + 2;
        let (q, ok) = match self.ch(p) {
            Some(c) if c.is_ascii_digit() => {
                let mut q = p;
                while self.ch(q).is_some_and(|c| c.is_ascii_digit()) {
                    q += 1;
                }
                (q, q > p)
            }
            Some('x' | 'X') => {
                let mut q = p + 1;
                while self.ch(q).is_some_and(is_ascii_hex) {
                    q += 1;
                }
                (q, q > p + 1)
            }
            _ => return None,
        };
        if !ok {
            return None;
        }
        match self.ch(q) {
            Some(c) if !is_ascii_hex(c) => Some((self.text(p, q), q + 1)),
            _ => None,
        }
    }

    /// `&#(?:[0-9]|[xX][0-9a-fA-F])` at `i`
    fn incomplete_charref_at(&self, i: usize) -> bool {
        match self.ch(i + 2) {
            Some(c) if c.is_ascii_digit() => true,
            Some('x' | 'X') => self.ch(i + 3).is_some_and(is_ascii_hex),
            _ => false,
        }
    }

    /// `&([a-zA-Z][-.a-zA-Z0-9]*)[^a-zA-Z0-9]` at `i`. Returns (name, index after terminator).
    fn entityref_at(&self, i: usize) -> Option<(String, usize)> {
        let n = self.raw.len();
        if !self.ch(i + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
            return None;
        }
        let run = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '.';
        let mut q = i + 2;
        while q < n && run(self.raw[q]) {
            q += 1;
        }
        if q < n {
            // raw[q] is outside the run, so it is never alphanumeric
            return Some((self.text(i + 1, q), q + 1));
        }
        // end of input: the regex backtracks to the last '-' or '.' inside the run
        let r = (i + 2..q).rev().find(|&r| matches!(self.raw[r], '-' | '.'))?;
        Some((self.text(i + 1, r), r + 1))
    }

    /// `&[a-zA-Z#]` at `i`, given that `&#` was already ruled out
    fn incomplete_entity_at(&self, i: usize) -> bool {
        self.ch(i + 1).is_some_and(|c| c.is_ascii_alphabetic())
    }

    // ----- python HTMLParser methods ----------------------------------------

    /// python `parse_starttag`: returns the end of the tag, or None when incomplete
    fn parse_starttag(&mut self, i: usize) -> Option<usize> {
        let endpos = self.check_for_whole_start_tag(i)?;
        let (tag, mut k) = self.tagfind(i + 1);
        let tag = tag.to_lowercase();
        let mut attrs: Vec<(String, String)> = Vec::new();
        while k < endpos {
            let Some((name, value, next)) = self.attrfind(k) else { break };
            let value = match value {
                None => String::new(),
                Some(v) => unescape_attr_value(&unquote(&v)),
            };
            attrs.push((name.to_lowercase(), value));
            k = next;
        }
        let rest = py_strip(&self.text(k, endpos)).to_string();
        if rest != ">" && rest != "/>" {
            // not a tag after all: the whole run is text
            let data = self.text(i, endpos);
            self.handle_data(&data);
            return Some(endpos);
        }
        if rest == "/>" {
            self.handle_startendtag(&tag, attrs);
        } else {
            self.handle_starttag(&tag, attrs, true);
            if CDATA_ELEMENTS.contains(&tag.as_str()) || tag == "plaintext" {
                self.set_cdata_mode(&tag, false);
            } else if RCDATA_ELEMENTS.contains(&tag.as_str()) {
                self.set_cdata_mode(&tag, true);
            }
        }
        Some(endpos)
    }

    /// python `parse_endtag`
    fn parse_endtag(&mut self, i: usize) -> Option<usize> {
        self.find_char(i + 2, '>')?;
        if !self.endtagopen(i) {
            if self.ch(i + 2) == Some('>') {
                return Some(i + 3);
            }
            return self.parse_bogus_comment(i);
        }
        let j = self.locatetagend(i + 2);
        if self.ch(j - 1) != Some('>') {
            return None;
        }
        let (name, _) = self.tagfind(i + 2);
        self.handle_endtag(&name.to_lowercase());
        self.clear_cdata_mode();
        Some(j)
    }

    /// python `parse_bogus_comment`
    fn parse_bogus_comment(&mut self, i: usize) -> Option<usize> {
        let pos = self.find_char(i + 2, '>')?;
        self.skip_node();
        Some(pos + 1)
    }

    /// python `parse_comment` (`--!?>`, or the abrupt `-?>` form)
    fn parse_comment(&mut self, i: usize) -> Option<usize> {
        let n = self.raw.len();
        for p in i + 4..n {
            if self.raw[p] == '-' && self.ch(p + 1) == Some('-') {
                if self.ch(p + 2) == Some('>') {
                    self.skip_node();
                    return Some(p + 3);
                }
                if self.ch(p + 2) == Some('!') && self.ch(p + 3) == Some('>') {
                    self.skip_node();
                    return Some(p + 4);
                }
            }
        }
        let end = match (self.ch(i + 4), self.ch(i + 5)) {
            (Some('>'), _) => i + 5,
            (Some('-'), Some('>')) => i + 6,
            _ => return None,
        };
        self.skip_node();
        Some(end)
    }

    /// python `parse_pi`
    fn parse_pi(&mut self, i: usize) -> Option<usize> {
        let pos = self.find_char(i + 2, '>')?;
        self.skip_node();
        Some(pos + 1)
    }

    /// python `parse_html_declaration` (for `<!` that is not a comment)
    fn parse_html_declaration(&mut self, i: usize) -> Option<usize> {
        if self.starts_with(i, "<![CDATA[") {
            let j = self.find_str(i + 9, "]]>")?;
            let data = self.text(i + 3, j);
            self.unknown_decl(data);
            Some(j + 3)
        } else if self.starts_with_ci(i, "<!doctype") {
            let gt = self.find_char(i + 9, '>')?;
            self.skip_node();
            Some(gt + 1)
        } else {
            self.parse_bogus_comment(i)
        }
    }

    /// The `if k < 0` arm of python `goahead` when `end` is set: an unterminated construct at
    /// end of input is reported as data, a comment, or dropped.
    fn finish_incomplete(&mut self, i: usize) -> Result<(), String> {
        let n = self.raw.len();
        if self.starttagopen(i) {
            // an unterminated start tag is dropped
        } else if self.starts_with(i, "</") {
            if i + 2 == n {
                self.handle_data("</");
            } else if self.endtagopen(i) {
                // an unterminated end tag is dropped
            } else {
                self.skip_node();
            }
        } else if self.starts_with(i, "<!--") {
            self.skip_node();
        } else if self.starts_with(i, "<![CDATA[") {
            let data = self.text(i + 3, n);
            self.unknown_decl(data);
        } else if self.starts_with_ci(i, "<!doctype") {
            self.skip_node();
        } else if self.starts_with(i, "<!") {
            self.skip_node();
        } else if self.starts_with(i, "<?") {
            self.skip_node();
        } else {
            return Err("we should not get here!".to_string());
        }
        Ok(())
    }

    /// python `goahead(end)` with `convert_charrefs=False`, resuming from `self.pos`
    fn goahead(&mut self, end: bool) -> Result<(), String> {
        let n = self.raw.len();
        let mut i = self.pos;
        while i < n {
            let j = match self.next_interesting(i) {
                Some(j) => j,
                None => {
                    if self.cdata_elem.is_some() {
                        break;
                    }
                    n
                }
            };
            if i < j {
                let data = self.text(i, j);
                self.handle_data(&data);
            }
            i = j;
            if i == n {
                break;
            }
            if self.ch(i) == Some('<') {
                let k = if self.starttagopen(i) {
                    self.parse_starttag(i)
                } else if self.starts_with(i, "</") {
                    self.parse_endtag(i)
                } else if self.starts_with(i, "<!--") {
                    self.parse_comment(i)
                } else if self.starts_with(i, "<?") {
                    self.parse_pi(i)
                } else if self.starts_with(i, "<!") {
                    self.parse_html_declaration(i)
                } else if i + 1 < n || end {
                    self.handle_data("<");
                    Some(i + 1)
                } else {
                    break;
                };
                i = match k {
                    Some(k) => k,
                    None => {
                        if !end {
                            break;
                        }
                        self.finish_incomplete(i)?;
                        n
                    }
                };
            } else if self.starts_with(i, "&#") {
                if let Some((name, after)) = self.charref_at(i) {
                    self.handle_charref(&name);
                    // the terminator is only consumed when it is ';'
                    i = if self.ch(after - 1) == Some(';') { after } else { after - 1 };
                    continue;
                }
                if self.incomplete_charref_at(i) {
                    if end {
                        let rest = self.text(i + 2, n);
                        self.handle_charref(&rest);
                        i = n;
                    }
                    break;
                } else if i + 3 < n {
                    self.handle_data("&#");
                    i += 2;
                } else {
                    break;
                }
            } else if self.ch(i) == Some('&') {
                if let Some((name, after)) = self.entityref_at(i) {
                    self.handle_entityref(&name);
                    i = if self.ch(after - 1) == Some(';') { after } else { after - 1 };
                    continue;
                }
                if self.incomplete_entity_at(i) {
                    if end {
                        let rest = self.text(i + 1, n);
                        self.handle_entityref(&rest);
                        i = n;
                    }
                    break;
                } else if i + 1 < n {
                    self.handle_data("&");
                    i += 1;
                } else {
                    break;
                }
            } else {
                return Err("we should not get here!".to_string());
            }
        }
        if end && i < n {
            let rest = self.text(i, n);
            self.handle_data(&rest);
            i = n;
        }
        self.pos = i;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// character references
// ---------------------------------------------------------------------------

/// python `int(s, base)` for the forms that can reach here: surrounding whitespace, an optional
/// `0x` prefix for base 16, and single underscores between digits. Saturates on overflow.
fn py_int(s: &str, base: u32) -> Option<u64> {
    let mut t = s.trim_matches(py_isspace);
    if base == 16 {
        t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    }
    if t.is_empty() || t.starts_with('_') || t.ends_with('_') || t.contains("__") {
        return None;
    }
    let digits: String = t.chars().filter(|&c| c != '_').collect();
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
        return None;
    }
    Some(digits.chars().fold(0u64, |acc, c| {
        acc.saturating_mul(base as u64).saturating_add(c.to_digit(base).unwrap_or(0) as u64)
    }))
}

/// bs4 `UnicodeDammit.numeric_character_reference`
fn numeric_reference(v: u64) -> String {
    const REPLACEMENT: &str = "\u{fffd}";
    if v == 0 || v > 0x10_ffff || (0xd800..=0xdfff).contains(&v) {
        return REPLACEMENT.to_string();
    }
    let Some(c) = char::from_u32(v as u32) else {
        return REPLACEMENT.to_string();
    };
    if (0x80..=0x9f).contains(&v) {
        // windows-1252 for the bytes the table covers; undefined bytes map to the C1 code point
        return encoding_rs::WINDOWS_1252.decode_without_bom_handling(&[v as u8]).0.into_owned();
    }
    c.to_string()
}

/// bs4 `BeautifulSoupHTMLParser._dereference_numeric_character_reference`: (dereferenced, extra)
fn deref_numeric(name: &str) -> (String, String) {
    let (body, base) = match name.strip_prefix(['x', 'X']) {
        Some(b) => (b, 16),
        None => (name, 10),
    };
    if let Some(v) = py_int(body, base) {
        return (numeric_reference(v), String::new());
    }
    // reg.search(name): ^([0-9]+)(.*) or, for hex, ^([0-9a-f]+)(.*). `.` stops at a newline.
    let is_run = |c: char| c.is_ascii_digit() || (base == 16 && ('a'..='f').contains(&c));
    let run = body.chars().take_while(|&c| is_run(c)).count();
    if run == 0 {
        return (String::new(), body.to_string());
    }
    let digits: String = body.chars().take(run).collect();
    let extra: String = body.chars().skip(run).take_while(|&c| c != '\n').collect();
    let v = py_int(&digits, base).unwrap_or(u64::MAX);
    (numeric_reference(v), extra)
}

// ---------------------------------------------------------------------------
// attribute values
// ---------------------------------------------------------------------------

/// python `_unescape_attrvalue` with `attr_charref`
/// (`&(#[0-9]+|#[xX][0-9a-fA-F]+|[a-zA-Z][a-zA-Z0-9]*)[;=]?`)
fn unescape_attr_value(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] != '&' {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let at = |k: usize| c.get(k).copied();
        if at(i + 1) == Some('#') {
            let mut k = i + 2;
            while at(k).is_some_and(|x| x.is_ascii_digit()) {
                k += 1;
            }
            let body_end = if k > i + 2 {
                Some(k)
            } else if at(i + 2).is_some_and(|x| x == 'x' || x == 'X') {
                let mut m = i + 3;
                while at(m).is_some_and(is_ascii_hex) {
                    m += 1;
                }
                (m > i + 3).then_some(m)
            } else {
                None
            };
            match body_end {
                Some(e) => {
                    let end = if matches!(at(e), Some(';' | '=')) { e + 1 } else { e };
                    let r: String = c[i..end].iter().collect();
                    out.push_str(&html_unescape(&r));
                    i = end;
                }
                None => {
                    out.push('&');
                    i += 1;
                }
            }
            continue;
        }
        if at(i + 1).is_some_and(|x| x.is_ascii_alphabetic()) {
            let mut k = i + 2;
            while at(k).is_some_and(|x| x.is_ascii_alphanumeric()) {
                k += 1;
            }
            let end = if matches!(at(k), Some(';' | '=')) { k + 1 } else { k };
            let r: String = c[i..end].iter().collect();
            let name: String = c[i + 1..end].iter().collect();
            if !r.ends_with('=') && HTML5_KEYS.contains(name.as_str()) {
                out.push_str(&html_unescape(&r));
            } else {
                out.push_str(&r);
            }
            i = end;
            continue;
        }
        out.push('&');
        i += 1;
    }
    out
}

/// python `parse_starttag`: strip one matching pair of quotes from a raw attribute value
fn unquote(v: &str) -> String {
    let c: Vec<char> = v.chars().collect();
    let quoted = !c.is_empty() && (c[0] == '\'' || c[0] == '"') && c[c.len() - 1] == c[0];
    if quoted {
        if c.len() >= 2 {
            c[1..c.len() - 1].iter().collect()
        } else {
            String::new()
        }
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(html: &str, tag: &str) -> Vec<String> {
        let doc = parse(html).unwrap();
        doc.elements(|n| n == tag).into_iter().map(|id| doc.get_text(id)).collect()
    }

    #[test]
    fn nested_paragraphs_are_not_auto_closed() {
        // html.parser keeps the nesting, so the outer p includes the inner text
        assert_eq!(texts("<p>1<p>2</p></p>", "p"), vec!["12", "2"]);
    }

    #[test]
    fn cdata_sections_are_text() {
        assert_eq!(texts("<p>a<![CDATA[xy]]>b</p>", "p"), vec!["axyb"]);
    }

    #[test]
    fn title_is_rcdata_with_entities_decoded() {
        assert_eq!(texts("<title>A<b>B</b></title>", "title"), vec!["A<b>B</b>"]);
    }

    #[test]
    fn whitespace_only_runs_collapse_to_newline_or_space() {
        assert_eq!(texts("<p>a<b>x</b>\n\n\n<i>y</i></p>", "p"), vec!["ax\ny"]);
    }

    #[test]
    fn unknown_entity_drops_the_semicolon() {
        assert_eq!(texts("<p>x&foo;y &amp &#65 z&#x42;</p>", "p"), vec!["x&fooy & A zB"]);
    }

    #[test]
    fn numeric_references_follow_bs4() {
        assert_eq!(texts("<p>&#128;&#0;&#xD800;</p>", "p"), vec!["\u{20ac}\u{fffd}\u{fffd}"]);
    }

    #[test]
    fn comments_split_text_but_are_not_text() {
        assert_eq!(texts("<p>a<!--c-->b</p>", "p"), vec!["ab"]);
    }

    #[test]
    fn script_content_is_raw_and_excluded_from_plain_text() {
        let doc = parse("<script>var a;</script><p>q</p>").unwrap();
        let script = doc.elements(|n| n == "script")[0];
        assert_eq!(doc.get_text(script), "var a;");
        assert_eq!(doc.get_text(doc.elements(|n| n == "p")[0]), "q");
    }

    #[test]
    fn void_elements_close_immediately() {
        assert_eq!(texts("<p>x<br>y</br>z</p>", "p"), vec!["xyz"]);
    }

    #[test]
    fn attribute_values_are_unescaped_like_python() {
        let doc = parse(r#"<a href="/a?x=1&amp;y=2&copy=3" href="/b">l</a>"#).unwrap();
        let a = doc.elements(|n| n == "a")[0];
        assert_eq!(doc.attr(a, "href"), Some("/b"));
        let doc = parse(r#"<img alt="&amp;&copy&notit;">"#).unwrap();
        let img = doc.elements(|n| n == "img")[0];
        assert_eq!(doc.attr(img, "alt"), Some("&\u{a9}&notit;"));
    }
}
