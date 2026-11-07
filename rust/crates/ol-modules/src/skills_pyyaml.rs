//! PyYAML-compatible reading of SKILL.md frontmatter.
//!
//! python's `yaml.safe_load` is YAML 1.1 with PyYAML's implicit resolvers: `yes`/`on` are True,
//! `010` is octal 8, `1_000` is 1000, `2026-01-01T10:00:00` is a datetime, `.nan` is a float, and
//! duplicate mapping keys keep the last value. serde_yaml is YAML 1.2 and rejects duplicate keys, so
//! the frontmatter is read here instead: tree-sitter-yaml supplies the syntax tree (scalar style,
//! tags, anchors), and every node is constructed the way PyYAML's SafeConstructor builds it.
//!
//! `!!binary`, `!!omap`, `!!pairs` and `!!set` are built as python builds them. Known gap: a set of
//! several elements is listed in insertion order, while python orders it by string hash, which is
//! randomised per process. Custom `!tags` are rejected (python raises for them too).

use crate::util::repr_str;
use num_bigint::BigInt;
use num_traits::{FromPrimitive, Zero};
use ol_core::pyjson::float_repr;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use tree_sitter::Node;

/// a python object as `yaml.safe_load` builds it
#[derive(Clone, Debug, PartialEq)]
pub enum PyObj {
    None,
    Bool(bool),
    Int(BigInt),
    Float(f64),
    Str(String),
    Date(Date),
    DateTime(DateTime),
    List(Vec<PyObj>),
    /// python dict: insertion ordered, keys compared with python equality
    Dict(Vec<(PyObj, PyObj)>),
    /// python set (from `!!set`): unhashable, so never a dict key
    Set(Vec<PyObj>),
    /// python bytes (from `!!binary`)
    Bytes(Vec<u8>),
    /// python tuple (the (key, value) pairs of `!!omap` and `!!pairs`)
    Tuple(Vec<PyObj>),
}

/// python datetime.date
#[derive(Clone, Debug, PartialEq)]
pub struct Date {
    y: i32,
    m: u32,
    d: u32,
}

/// python datetime.datetime; `offset` is the utc offset in seconds when the value has a tzinfo
#[derive(Clone, Debug, PartialEq)]
pub struct DateTime {
    date: Date,
    h: u32,
    mi: u32,
    s: u32,
    us: u32,
    offset: Option<i64>,
}

static BOOL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF)$").expect("bool regex")
});
static FLOAT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$",
    )
    .expect("float regex")
});
static INT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)$")
        .expect("int regex")
});
static TS_DATE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^([0-9]{4})-([0-9]{2})-([0-9]{2})$").expect("timestamp date regex"));
static TS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^([0-9]{4})-([0-9]{1,2})-([0-9]{1,2})(?:[Tt]|[ \t]+)([0-9]{1,2}):([0-9]{2}):([0-9]{2})(?:\.([0-9]*))?(?:[ \t]*(Z|([-+])([0-9]{1,2})(?::([0-9]{2}))?))?$",
    )
    .expect("timestamp regex")
});

impl PyObj {
    /// python `bool(obj)`
    pub fn truthy(&self) -> bool {
        match self {
            PyObj::None => false,
            PyObj::Bool(b) => *b,
            PyObj::Int(i) => !i.is_zero(),
            PyObj::Float(f) => *f != 0.0,
            PyObj::Str(s) => !s.is_empty(),
            PyObj::Date(_) | PyObj::DateTime(_) => true,
            PyObj::List(v) | PyObj::Tuple(v) => !v.is_empty(),
            PyObj::Dict(v) => !v.is_empty(),
            PyObj::Set(v) => !v.is_empty(),
            PyObj::Bytes(v) => !v.is_empty(),
        }
    }

    /// python `str(obj)`
    pub fn py_str(&self) -> String {
        match self {
            PyObj::Str(s) => s.clone(),
            PyObj::Date(d) => format!("{:04}-{:02}-{:02}", d.y, d.m, d.d),
            PyObj::DateTime(dt) => dt.iso(),
            other => other.repr(),
        }
    }

    /// python `repr(obj)`
    pub fn repr(&self) -> String {
        match self {
            PyObj::None => "None".into(),
            PyObj::Bool(true) => "True".into(),
            PyObj::Bool(false) => "False".into(),
            PyObj::Int(i) => i.to_string(),
            PyObj::Float(f) => py_float(*f),
            PyObj::Str(s) => repr_str(s),
            PyObj::Date(d) => format!("datetime.date({}, {}, {})", d.y, d.m, d.d),
            PyObj::DateTime(dt) => dt.repr(),
            PyObj::List(v) => format!("[{}]", v.iter().map(PyObj::repr).collect::<Vec<_>>().join(", ")),
            PyObj::Dict(v) => format!(
                "{{{}}}",
                v.iter().map(|(k, v)| format!("{}: {}", k.repr(), v.repr())).collect::<Vec<_>>().join(", ")
            ),
            PyObj::Set(v) if v.is_empty() => "set()".into(),
            PyObj::Set(v) => format!("{{{}}}", v.iter().map(PyObj::repr).collect::<Vec<_>>().join(", ")),
            PyObj::Bytes(b) => bytes_repr(b),
            PyObj::Tuple(v) => match v.as_slice() {
                [one] => format!("({},)", one.repr()),
                _ => format!("({})", v.iter().map(PyObj::repr).collect::<Vec<_>>().join(", ")),
            },
        }
    }

    /// python hashability: a dict key or set element. lists, dicts and sets are not, and a tuple is
    /// only when all its items are
    fn hashable(&self) -> bool {
        match self {
            PyObj::List(_) | PyObj::Dict(_) | PyObj::Set(_) => false,
            PyObj::Tuple(v) => v.iter().all(PyObj::hashable),
            _ => true,
        }
    }

    /// `self[key]` for a dict with a str key (python `dict.get`), None for anything else
    pub fn get(&self, key: &str) -> Option<&PyObj> {
        match self {
            PyObj::Dict(pairs) => pairs.iter().find(|(k, _)| matches!(k, PyObj::Str(s) if s == key)).map(|(_, v)| v),
            _ => None,
        }
    }
}

impl DateTime {
    /// python `str(datetime)`: isoformat with a space separator
    fn iso(&self) -> String {
        let mut out = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.date.y, self.date.m, self.date.d, self.h, self.mi, self.s
        );
        if self.us != 0 {
            out.push_str(&format!(".{:06}", self.us));
        }
        if let Some(off) = self.offset {
            out.push_str(&fmt_offset(off));
        }
        out
    }

    /// python `repr(datetime)`
    fn repr(&self) -> String {
        let mut out = format!(
            "datetime.datetime({}, {}, {}, {}, {}",
            self.date.y, self.date.m, self.date.d, self.h, self.mi
        );
        if self.s != 0 || self.us != 0 {
            out.push_str(&format!(", {}", self.s));
        }
        if self.us != 0 {
            out.push_str(&format!(", {}", self.us));
        }
        match self.offset {
            None => {}
            Some(0) => out.push_str(", tzinfo=datetime.timezone.utc"),
            Some(off) => out.push_str(&format!(", tzinfo=datetime.timezone(datetime.timedelta({}))", timedelta_repr(off))),
        }
        out.push(')');
        out
    }
}

/// python `str(float)` / `repr(float)`
fn py_float(f: f64) -> String {
    if f.is_nan() {
        "nan".into()
    } else if f.is_infinite() {
        if f > 0.0 { "inf".into() } else { "-inf".into() }
    } else {
        float_repr(f)
    }
}

/// the utc offset text of python's datetime str: `+HH:MM`, with `:SS` when there are seconds
fn fmt_offset(off: i64) -> String {
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    let mut out = format!("{}{:02}:{:02}", sign, a / 3600, (a % 3600) / 60);
    if a % 60 != 0 {
        out.push_str(&format!(":{:02}", a % 60));
    }
    out
}

/// python `repr(timedelta(seconds=off))`, e.g. `seconds=18000` or `days=-1, seconds=66600`
fn timedelta_repr(off: i64) -> String {
    let days = off.div_euclid(86400);
    let secs = off.rem_euclid(86400);
    let mut parts = Vec::new();
    if days != 0 {
        parts.push(format!("days={}", days));
    }
    if secs != 0 {
        parts.push(format!("seconds={}", secs));
    }
    if parts.is_empty() {
        "0".into()
    } else {
        parts.join(", ")
    }
}

/// python `==` between two dict keys (True == 1 == 1.0 as in python)
fn py_eq(a: &PyObj, b: &PyObj) -> bool {
    match (a, b) {
        (PyObj::None, PyObj::None) => true,
        (PyObj::Str(x), PyObj::Str(y)) => x == y,
        (PyObj::Date(x), PyObj::Date(y)) => x == y,
        (PyObj::DateTime(x), PyObj::DateTime(y)) => x == y,
        (PyObj::Bytes(x), PyObj::Bytes(y)) => x == y,
        (PyObj::Tuple(x), PyObj::Tuple(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        _ => match (num_of(a), num_of(b)) {
            (Some(x), Some(y)) => num_eq(x, y),
            _ => false,
        },
    }
}

enum Num {
    Int(BigInt),
    Float(f64),
}

fn num_of(v: &PyObj) -> Option<Num> {
    match v {
        PyObj::Bool(b) => Some(Num::Int(BigInt::from(u8::from(*b)))),
        PyObj::Int(i) => Some(Num::Int(i.clone())),
        PyObj::Float(f) => Some(Num::Float(*f)),
        _ => None,
    }
}

fn num_eq(a: Num, b: Num) -> bool {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => x == y,
        (Num::Float(x), Num::Float(y)) => x == y,
        (Num::Int(i), Num::Float(f)) | (Num::Float(f), Num::Int(i)) => {
            f.is_finite() && f.fract() == 0.0 && BigInt::from_f64(f).is_some_and(|g| g == i)
        }
    }
}

/// python dict assignment `d[key] = value`: an equal key keeps its position and takes the new value
fn dict_set(dict: &mut Vec<(PyObj, PyObj)>, key: PyObj, value: PyObj) -> Result<(), String> {
    if !key.hashable() {
        return Err("found unhashable key".into());
    }
    match dict.iter_mut().find(|(k, _)| py_eq(k, &key)) {
        Some(slot) => slot.1 = value,
        None => dict.push((key, value)),
    }
    Ok(())
}

/// reads a SKILL.md frontmatter block the way `yaml.safe_load` does. Err means PyYAML raises.
pub fn load(text: &str) -> Result<PyObj, String> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_yaml::LANGUAGE.into()).map_err(|e| e.to_string())?;
    let tree = parser.parse(text, None).ok_or_else(|| "no parse tree".to_string())?;
    let root = tree.root_node();
    if has_syntax_error(root) {
        return Err("syntax error".into());
    }
    // PyYAML's scanner skips only spaces between tokens, so a tab is an error anywhere outside a
    // quoted scalar, a block scalar or a comment (`name:\tfoo`, `name: a\tb`, a trailing tab)
    if tab_outside_verbatim(root, text) {
        return Err("found character '\\t' that cannot start any token".into());
    }
    let mut contents = Vec::new();
    for doc in named_children(root).into_iter().filter(|n| n.kind() == "document") {
        contents.extend(named_children(doc).into_iter().filter(|c| matches!(c.kind(), "flow_node" | "block_node")));
    }
    if contents.len() > 1 {
        return Err("expected a single document in the stream".into());
    }
    let mut loader = Loader { src: text, anchors: HashMap::new() };
    match contents.first() {
        Some(node) => loader.node(*node),
        None => Ok(PyObj::None),
    }
}

struct Loader<'s> {
    src: &'s str,
    /// anchor name -> constructed value (PyYAML aliases share one object)
    anchors: HashMap<String, PyObj>,
}

type Pair<'t> = (Option<Node<'t>>, Option<Node<'t>>);

impl<'s> Loader<'s> {
    fn text(&self, node: Node<'_>) -> &'s str {
        node.utf8_text(self.src.as_bytes()).unwrap_or("")
    }

    /// a flow_node / block_node: its anchor, tag, alias and content
    fn node(&mut self, node: Node<'_>) -> Result<PyObj, String> {
        let mut anchor: Option<&str> = None;
        let mut tag: Option<&str> = None;
        let mut content: Option<Node<'_>> = None;
        for child in named_children(node) {
            match child.kind() {
                "anchor" => anchor = child_of_kind(child, "anchor_name").map(|n| self.text(n)),
                "tag" => tag = Some(self.text(child)),
                "alias" => {
                    let name = child_of_kind(child, "alias_name").map(|n| self.text(n)).unwrap_or("");
                    return self.anchors.get(name).cloned().ok_or_else(|| format!("found undefined alias '{}'", name));
                }
                "comment" => {}
                _ => content = Some(child),
            }
        }
        let value = match content {
            Some(c) => self.content(c, tag)?,
            None => PyObj::None,
        };
        if let Some(name) = anchor {
            self.anchors.insert(name.to_string(), value.clone());
        }
        Ok(value)
    }

    fn content(&mut self, node: Node<'_>, tag: Option<&str>) -> Result<PyObj, String> {
        match node.kind() {
            "plain_scalar" => scalar(fold_plain(self.text(node)), false, tag),
            "double_quote_scalar" | "single_quote_scalar" | "block_scalar" => {
                // a quoted or block scalar is decoded by serde_yaml, whose string rules match YAML's
                let decoded: String = serde_yaml::from_str(self.text(node)).map_err(|e| e.to_string())?;
                scalar(decoded, true, tag)
            }
            "flow_sequence" | "block_sequence" => match tag {
                Some("!!omap") | Some("!!pairs") => self.pairs_list(node),
                _ => {
                    check_collection_tag(tag, "!!seq")?;
                    self.sequence(node)
                }
            },
            "flow_mapping" | "block_mapping" => match tag {
                Some("!!set") => self.set(node),
                _ => {
                    check_collection_tag(tag, "!!map")?;
                    self.mapping(node)
                }
            },
            other => Err(format!("unsupported YAML node {}", other)),
        }
    }

    fn sequence(&mut self, node: Node<'_>) -> Result<PyObj, String> {
        let mut items = Vec::new();
        for child in named_children(node) {
            match child.kind() {
                "comment" => {}
                "block_sequence_item" => {
                    let item = named_children(child).into_iter().find(|c| c.kind() != "comment");
                    items.push(match item {
                        Some(c) => self.node(c)?,
                        None => PyObj::None,
                    });
                }
                "flow_node" | "block_node" => items.push(self.node(child)?),
                "flow_pair" => items.push(PyObj::Dict(self.build_dict(vec![pair_of(child)])?)),
                other => return Err(format!("unexpected {} in a sequence", other)),
            }
        }
        Ok(PyObj::List(items))
    }

    /// `!!omap` and `!!pairs`: a list of (key, value) tuples, each item a sequence entry holding
    /// exactly one mapping pair. PyYAML does not merge `<<` here and keeps repeated keys.
    fn pairs_list(&mut self, node: Node<'_>) -> Result<PyObj, String> {
        let mut items = Vec::new();
        for child in named_children(node) {
            let item = match child.kind() {
                "comment" => continue,
                "block_sequence_item" => match named_children(child).into_iter().find(|c| c.kind() != "comment") {
                    Some(item) => item,
                    None => return Err("expected a single mapping item".into()),
                },
                "flow_node" | "block_node" | "flow_pair" => child,
                other => return Err(format!("unexpected {} in a sequence", other)),
            };
            let entries = mapping_of_item(item)?;
            let [(key, value)] = entries.as_slice() else {
                return Err("expected a single mapping item".into());
            };
            let key = match key {
                Some(k) => self.key(*k)?,
                None => PyObj::None,
            };
            let value = match value {
                Some(v) => self.node(*v)?,
                None => PyObj::None,
            };
            items.push(PyObj::Tuple(vec![key, value]));
        }
        Ok(PyObj::List(items))
    }

    /// `!!set`: the keys of the mapping, the values are ignored (PyYAML `construct_yaml_set`)
    fn set(&mut self, node: Node<'_>) -> Result<PyObj, String> {
        let dict = self.build_dict(mapping_entries(node)?)?;
        Ok(PyObj::Set(dict.into_iter().map(|(k, _)| k).collect()))
    }

    fn mapping(&mut self, node: Node<'_>) -> Result<PyObj, String> {
        Ok(PyObj::Dict(self.build_dict(mapping_entries(node)?)?))
    }

    /// PyYAML `flatten_mapping` + `construct_mapping`: merge keys (`<<`) come first, then the
    /// mapping's own pairs; a repeated key keeps the last value
    fn build_dict(&mut self, pairs: Vec<Pair<'_>>) -> Result<Vec<(PyObj, PyObj)>, String> {
        let mut merged: Vec<(PyObj, PyObj)> = Vec::new();
        let mut own: Vec<(PyObj, PyObj)> = Vec::new();
        for (key, value) in pairs {
            if key.is_some_and(|k| self.bare_plain(k).as_deref() == Some("<<")) {
                let source = match value {
                    Some(v) => self.node(v)?,
                    None => PyObj::None,
                };
                match source {
                    PyObj::Dict(d) => merged.extend(d),
                    PyObj::List(items) => {
                        for item in items.into_iter().rev() {
                            match item {
                                PyObj::Dict(d) => merged.extend(d),
                                _ => return Err("expected a mapping for merging".into()),
                            }
                        }
                    }
                    _ => return Err("expected a mapping or list of mappings for merging".into()),
                }
                continue;
            }
            let key = match key {
                Some(k) => self.key(k)?,
                None => PyObj::None,
            };
            let value = match value {
                Some(v) => self.node(v)?,
                None => PyObj::None,
            };
            own.push((key, value));
        }
        let mut dict = Vec::new();
        for (k, v) in merged.into_iter().chain(own) {
            dict_set(&mut dict, k, v)?;
        }
        Ok(dict)
    }

    /// a mapping key. PyYAML gives a bare `=` key the str tag, unlike a `=` value
    fn key(&mut self, node: Node<'_>) -> Result<PyObj, String> {
        if self.bare_plain(node).as_deref() == Some("=") {
            return Ok(PyObj::Str("=".into()));
        }
        self.node(node)
    }

    /// the folded text of a flow_node / block_node that is a bare plain scalar (no tag, anchor or alias)
    fn bare_plain(&self, node: Node<'_>) -> Option<String> {
        if !matches!(node.kind(), "flow_node" | "block_node") {
            return None;
        }
        let mut content = None;
        for child in named_children(node) {
            match child.kind() {
                "comment" => {}
                "plain_scalar" => content = Some(child),
                _ => return None,
            }
        }
        content.map(|c| fold_plain(self.text(c)))
    }
}

fn pair_of(node: Node<'_>) -> Pair<'_> {
    (node.child_by_field_name("key"), node.child_by_field_name("value"))
}

/// the pairs of a block or flow mapping. a flow mapping entry with no `:` is a key with a None value.
fn mapping_entries(node: Node<'_>) -> Result<Vec<Pair<'_>>, String> {
    let mut pairs = Vec::new();
    for child in named_children(node) {
        match child.kind() {
            "block_mapping_pair" | "flow_pair" => pairs.push(pair_of(child)),
            "flow_node" | "block_node" => pairs.push((Some(child), None)),
            "comment" => {}
            other => return Err(format!("unexpected {} in a mapping", other)),
        }
    }
    Ok(pairs)
}

/// the pairs of one `!!omap` / `!!pairs` item, which must be a mapping (PyYAML `construct_yaml_omap`)
fn mapping_of_item(item: Node<'_>) -> Result<Vec<Pair<'_>>, String> {
    match item.kind() {
        "flow_pair" | "block_mapping_pair" => Ok(vec![pair_of(item)]),
        "flow_node" | "block_node" => match named_children(item).into_iter().find(|c| matches!(c.kind(), "flow_mapping" | "block_mapping")) {
            Some(mapping) => mapping_entries(mapping),
            None => Err("expected a mapping node".into()),
        },
        _ => Err("expected a mapping node".into()),
    }
}

/// true when a tab sits outside every quoted scalar, block scalar and comment of the frontmatter
fn tab_outside_verbatim(root: Node<'_>, text: &str) -> bool {
    let mut verbatim = Vec::new();
    collect_verbatim(root, &mut verbatim);
    text.bytes()
        .enumerate()
        .any(|(i, b)| b == b'\t' && !verbatim.iter().any(|&(start, end)| start <= i && i < end))
}

fn collect_verbatim(node: Node<'_>, out: &mut Vec<(usize, usize)>) {
    if matches!(node.kind(), "double_quote_scalar" | "single_quote_scalar" | "block_scalar" | "comment") {
        out.push((node.start_byte(), node.end_byte()));
        return;
    }
    for i in 0..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            collect_verbatim(child, out);
        }
    }
}

fn check_collection_tag(tag: Option<&str>, want: &str) -> Result<(), String> {
    match tag {
        None | Some("!") => Ok(()),
        Some(t) if t == want => Ok(()),
        Some(t) => Err(format!("could not determine a constructor for the tag '{}'", t)),
    }
}

/// PyYAML's scalar construction: a quoted scalar is a str; a plain scalar takes the implicit
/// resolver, unless an explicit core tag names its type
fn scalar(text: String, quoted: bool, tag: Option<&str>) -> Result<PyObj, String> {
    match tag {
        None | Some("!") => {
            if quoted {
                Ok(PyObj::Str(text))
            } else {
                resolve_plain(&text)
            }
        }
        Some(t) => match t.strip_prefix("!!") {
            Some("str") => Ok(PyObj::Str(text)),
            Some("int") => construct_int(&text),
            Some("float") => construct_float(&text),
            Some("bool") => match text.to_lowercase().as_str() {
                "yes" | "true" | "on" => Ok(PyObj::Bool(true)),
                "no" | "false" | "off" => Ok(PyObj::Bool(false)),
                _ => Err(format!("invalid bool {}", repr_str(&text))),
            },
            Some("null") => Ok(PyObj::None),
            Some("timestamp") => construct_timestamp(&text),
            Some("binary") => decode_binary(&text).map(PyObj::Bytes),
            _ => Err(format!("could not determine a constructor for the tag '{}'", t)),
        },
    }
}

/// PyYAML's implicit resolvers for a plain scalar, in PyYAML's order
fn resolve_plain(text: &str) -> Result<PyObj, String> {
    if matches!(text, "" | "~" | "null" | "Null" | "NULL") {
        return Ok(PyObj::None);
    }
    if BOOL_RE.is_match(text) {
        return Ok(PyObj::Bool(matches!(
            text,
            "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON"
        )));
    }
    if FLOAT_RE.is_match(text) {
        return construct_float(text);
    }
    if INT_RE.is_match(text) {
        return construct_int(text);
    }
    if text == "<<" {
        return Err("could not determine a constructor for the tag 'tag:yaml.org,2002:merge'".into());
    }
    if TS_DATE_RE.is_match(text) || TS_RE.is_match(text) {
        return construct_timestamp(text);
    }
    if text == "=" {
        return Err("could not determine a constructor for the tag 'tag:yaml.org,2002:value'".into());
    }
    Ok(PyObj::Str(text.to_string()))
}

/// PyYAML `construct_yaml_int`
fn construct_int(text: &str) -> Result<PyObj, String> {
    let value = text.replace('_', "");
    let (neg, body) = split_sign(&value);
    let magnitude = if body == "0" {
        BigInt::zero()
    } else if let Some(bin) = body.strip_prefix("0b") {
        parse_int(bin, 2)?
    } else if let Some(hex) = body.strip_prefix("0x") {
        parse_int(hex, 16)?
    } else if body.starts_with('0') {
        // python int(value, 8) accepts an optional 0o prefix
        let oct = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")).unwrap_or(body);
        parse_int(oct, 8)?
    } else if body.contains(':') {
        let mut total = BigInt::zero();
        let mut base = BigInt::from(1);
        for part in body.split(':').rev() {
            total += parse_int(part, 10)? * &base;
            base *= 60;
        }
        total
    } else {
        parse_int(body, 10)?
    };
    Ok(PyObj::Int(if neg { -magnitude } else { magnitude }))
}

fn parse_int(digits: &str, radix: u32) -> Result<BigInt, String> {
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(format!("invalid literal for int() with base {}: {}", radix, repr_str(digits)));
    }
    BigInt::parse_bytes(digits.as_bytes(), radix).ok_or_else(|| format!("invalid int {}", repr_str(digits)))
}

/// PyYAML `construct_yaml_float`
fn construct_float(text: &str) -> Result<PyObj, String> {
    let value = text.replace('_', "").to_lowercase();
    let (neg, body) = split_sign(&value);
    let sign = if neg { -1.0 } else { 1.0 };
    if body == ".inf" {
        return Ok(PyObj::Float(sign * f64::INFINITY));
    }
    if body == ".nan" {
        return Ok(PyObj::Float(f64::NAN));
    }
    if body.contains(':') {
        let mut total = 0.0f64;
        let mut base = 1.0f64;
        for part in body.split(':').rev() {
            let digit: f64 = part.parse().map_err(|_| format!("could not convert string to float: {}", repr_str(part)))?;
            total += digit * base;
            base *= 60.0;
        }
        return Ok(PyObj::Float(sign * total));
    }
    let f: f64 = body.parse().map_err(|_| format!("could not convert string to float: {}", repr_str(body)))?;
    Ok(PyObj::Float(sign * f))
}

fn split_sign(value: &str) -> (bool, &str) {
    if let Some(rest) = value.strip_prefix('-') {
        (true, rest)
    } else {
        (false, value.strip_prefix('+').unwrap_or(value))
    }
}

/// PyYAML `construct_yaml_timestamp`: a date, or a datetime with optional fraction and offset
fn construct_timestamp(text: &str) -> Result<PyObj, String> {
    if let Some(c) = TS_DATE_RE.captures(text) {
        return Ok(PyObj::Date(make_date(group(&c, 1), group(&c, 2), group(&c, 3))?));
    }
    let Some(c) = TS_RE.captures(text) else {
        return Err(format!("could not construct timestamp from {}", repr_str(text)));
    };
    let date = make_date(group(&c, 1), group(&c, 2), group(&c, 3))?;
    let h: u32 = parse_part(group(&c, 4))?;
    let mi: u32 = parse_part(group(&c, 5))?;
    let s: u32 = parse_part(group(&c, 6))?;
    if h > 23 || mi > 59 || s > 59 {
        return Err(format!("time out of range in {}", repr_str(text)));
    }
    // python keeps the first six digits of the fraction and pads them with zeros on the right
    let mut frac: String = group(&c, 7).chars().take(6).collect();
    while frac.len() < 6 {
        frac.push('0');
    }
    let us: u32 = parse_part(&frac)?;
    let offset = match c.get(8).map(|m| m.as_str()) {
        None => None,
        Some("Z") => Some(0),
        Some(_) => {
            let sign = if group(&c, 9) == "-" { -1 } else { 1 };
            let th: i64 = parse_part(group(&c, 10))?;
            let tm: i64 = if c.get(11).is_some() { parse_part(group(&c, 11))? } else { 0 };
            let secs = sign * (th * 3600 + tm * 60);
            if secs.abs() >= 86400 {
                return Err("tzinfo offset must be strictly within a day".into());
            }
            Some(secs)
        }
    };
    Ok(PyObj::DateTime(DateTime { date, h, mi, s, us, offset }))
}

fn group<'a>(c: &regex::Captures<'a>, i: usize) -> &'a str {
    c.get(i).map_or("", |m| m.as_str())
}

fn parse_part<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.parse::<T>().map_err(|_| format!("invalid number {}", repr_str(s)))
}

/// a calendar date with python's range checks (year 1..=9999, real month and day)
fn make_date(y: &str, m: &str, d: &str) -> Result<Date, String> {
    let (y, m, d): (i32, u32, u32) = (parse_part(y)?, parse_part(m)?, parse_part(d)?);
    if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return Err(format!("date out of range: {:04}-{:02}-{:02}", y, m, d));
    }
    Ok(Date { y, m, d })
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        2 => {
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// PyYAML `construct_yaml_binary`: `base64.decodebytes` of the scalar's ascii text. Whitespace and
/// other non-alphabet characters are skipped, data must be a whole number of quads once the `=`
/// padding is counted, and any alphabet character after the padding is an error.
fn decode_binary(text: &str) -> Result<Vec<u8>, String> {
    if !text.is_ascii() {
        return Err("'ascii' codec can't encode character".into());
    }
    let (data_part, pad_part) = match text.find('=') {
        Some(i) => (&text[..i], &text[i..]),
        None => (text, ""),
    };
    if pad_part.bytes().any(|b| b != b'=' && b64_value(b).is_some()) {
        return Err("Incorrect padding".into());
    }
    let data: Vec<u8> = data_part.bytes().filter_map(b64_value).collect();
    let pads = pad_part.bytes().filter(|&b| b == b'=').count();
    let rem = data.len() % 4;
    if rem == 1 {
        return Err("Invalid base64-encoded string: number of data characters cannot be 1 more than a multiple of 4".into());
    }
    if rem != 0 && pads < 4 - rem {
        return Err("Incorrect padding".into());
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 2);
    for quad in data.chunks(4) {
        let acc = quad.iter().enumerate().fold(0u32, |acc, (i, &v)| acc | (u32::from(v) << (18 - 6 * i)));
        let bytes = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        out.extend_from_slice(&bytes[..quad.len() - 1]);
    }
    Ok(out)
}

/// the value of one base64 alphabet character
fn b64_value(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// python `repr(bytes)`: single quotes unless the value holds a single quote and no double quote
fn bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') { '"' } else { '\'' };
    let mut out = String::from("b");
    out.push(quote);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            c if c == quote as u8 => {
                out.push('\\');
                out.push(quote);
            }
            0x20..=0x7e => out.push(c as char),
            _ => out.push_str(&format!("\\x{:02x}", c)),
        }
    }
    out.push(quote);
    out
}

/// PyYAML's plain-scalar folding: a line break becomes a space, and each blank line a newline.
/// PyYAML strips only spaces around a line break, so a non-breaking space stays in the scalar.
fn fold_plain(text: &str) -> String {
    let mut out = String::new();
    let mut pending_breaks = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        let t = line.trim_matches(' ');
        if i == 0 {
            out.push_str(t);
        } else if t.is_empty() {
            pending_breaks += 1;
        } else {
            if pending_breaks == 0 {
                out.push(' ');
            } else {
                out.extend(std::iter::repeat_n('\n', pending_breaks));
            }
            pending_breaks = 0;
            out.push_str(t);
        }
    }
    out
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    (0..node.named_child_count() as u32).filter_map(|i| node.named_child(i)).collect()
}

fn child_of_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    named_children(node).into_iter().find(|c| c.kind() == kind)
}

fn has_syntax_error(node: Node<'_>) -> bool {
    if node.is_error() || node.is_missing() {
        return true;
    }
    (0..node.child_count() as u32).filter_map(|i| node.child(i)).any(has_syntax_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the python `str()` of `meta["name"]` for a frontmatter block, or None when python would
    /// fall back to the folder name (not a dict, no name, or a falsy name)
    fn name_of(block: &str) -> Option<String> {
        let meta = load(block).ok()?;
        let name = meta.get("name")?;
        name.truthy().then(|| name.py_str())
    }

    #[test]
    fn yaml11_bool_words_resolve_like_pyyaml() {
        assert_eq!(name_of("name: yes").as_deref(), Some("True"));
        assert_eq!(name_of("name: on").as_deref(), Some("True"));
        assert_eq!(name_of("name: no"), None);
        assert_eq!(name_of("name: off"), None);
        assert_eq!(name_of("name: \"yes\"").as_deref(), Some("yes"));
    }

    #[test]
    fn yaml11_numbers_and_strings_match_pyyaml() {
        assert_eq!(name_of("name: 010").as_deref(), Some("8"));
        assert_eq!(name_of("name: 1_000").as_deref(), Some("1000"));
        assert_eq!(name_of("name: 1e3").as_deref(), Some("1e3"));
        assert_eq!(name_of("name: 1.0e+3").as_deref(), Some("1000.0"));
        assert_eq!(name_of("name: .nan").as_deref(), Some("nan"));
        assert_eq!(name_of("name: -.inf").as_deref(), Some("-inf"));
        assert_eq!(name_of("name: 1:20").as_deref(), Some("80"));
        assert_eq!(name_of("name: 0_"), None);
    }

    #[test]
    fn timestamps_become_python_datetimes() {
        assert_eq!(name_of("name: 2026-01-01").as_deref(), Some("2026-01-01"));
        assert_eq!(name_of("name: 2026-01-01 10:00:00").as_deref(), Some("2026-01-01 10:00:00"));
        assert_eq!(name_of("name: 2026-01-01 10:00").as_deref(), Some("2026-01-01 10:00"));
        assert_eq!(name_of("name: 2026-01-01T10:00:00.5Z").as_deref(), Some("2026-01-01 10:00:00.500000+00:00"));
        assert_eq!(name_of("name: 2026-01-01 10:00:00 -05:30").as_deref(), Some("2026-01-01 10:00:00-05:30"));
        // an impossible date makes PyYAML raise, so the whole frontmatter is dropped
        assert_eq!(load("name: 2026-02-30").ok(), None);
    }

    #[test]
    fn duplicate_keys_keep_the_last_value() {
        assert_eq!(name_of("name: First\nother: x\nname: Second").as_deref(), Some("Second"));
        assert_eq!(name_of("name: a\nmeta: {k: 1, k: 2}").as_deref(), Some("a"));
    }

    #[test]
    fn non_scalar_names_use_python_repr() {
        assert_eq!(name_of("name: [a, b]").as_deref(), Some("['a', 'b']"));
        assert_eq!(name_of("name: {a: 1, a: 2}").as_deref(), Some("{'a': 2}"));
        assert_eq!(name_of("name: []"), None);
    }

    #[test]
    fn non_mapping_and_broken_frontmatter_have_no_meta() {
        assert_eq!(load("- a").map(|v| v.truthy()).ok(), Some(true));
        assert_eq!(name_of("- a"), None);
        assert_eq!(name_of("name: ["), None);
        assert_eq!(name_of("name: <<"), None);
        assert_eq!(name_of("name: x\n---\nname: y"), None);
    }

    #[test]
    fn merge_and_equal_keys_follow_pyyaml() {
        assert_eq!(name_of("base: &b {name: From Base}\n<<: *b").as_deref(), Some("From Base"));
        assert_eq!(name_of("=: x\nname: y").as_deref(), Some("y"));
        assert_eq!(name_of("name: x\nother: =").as_deref(), None);
    }

    #[test]
    fn tabs_are_scanner_errors_outside_quotes_and_comments() {
        assert_eq!(name_of("name:\tfoo"), None);
        assert_eq!(name_of("name: a\tb"), None);
        assert_eq!(name_of("name: a \tb"), None);
        assert_eq!(name_of("name: foo\t"), None);
        assert_eq!(name_of("name: \"a\tb\"").as_deref(), Some("a\tb"));
        assert_eq!(name_of("name: x # a\tb").as_deref(), Some("x"));
    }

    #[test]
    fn only_spaces_are_folded_out_of_plain_scalars() {
        // PyYAML keeps a non-breaking space at a line end and inside a folded line
        assert_eq!(name_of("name: ca\u{a0}\nother: 1").as_deref(), Some("ca\u{a0}"));
        assert_eq!(name_of("name: foo \u{a0}\n bar").as_deref(), Some("foo \u{a0} bar"));
    }

    #[test]
    fn binary_set_omap_and_pairs_build_like_pyyaml() {
        assert_eq!(name_of("name: !!binary aGk=").as_deref(), Some("b'hi'"));
        assert_eq!(name_of("name: !!binary aG==").as_deref(), Some("b'h'"));
        assert_eq!(name_of("name: !!binary J2E=").as_deref(), Some("b\"'a\""));
        assert_eq!(name_of("name: !!binary \"\""), None);
        assert_eq!(name_of("name: !!binary aGk"), None);
        assert_eq!(name_of("name: !!binary aGé"), None);
        assert_eq!(name_of("name: !!set {a: null}").as_deref(), Some("{'a'}"));
        assert_eq!(name_of("name: !!set {}"), None);
        assert_eq!(name_of("name: !!set {[a]: null}"), None);
        assert_eq!(name_of("name: !!omap [a: 1, b: 2]").as_deref(), Some("[('a', 1), ('b', 2)]"));
        assert_eq!(name_of("name: !!pairs [a: 1, a: 2]").as_deref(), Some("[('a', 1), ('a', 2)]"));
        assert_eq!(name_of("name: !!omap []"), None);
        assert_eq!(name_of("name: !!omap [a]"), None);
        assert_eq!(name_of("name: !!omap {a: 1}"), None);
    }

    /// expected `str(meta["name"])` (None = folder name) for frontmatter blocks, generated by running
    /// `yaml.safe_load` (PyYAML 6.0.3) with the same `meta.get("name")` rule as python `_scan`
    const PYYAML_REFERENCE: &[(&str, Option<&str>)] = &[
        ("name: yes", Some("True")),
        ("name: no", None),
        ("name: on", Some("True")),
        ("name: off", None),
        ("name: Yes", Some("True")),
        ("name: NO", None),
        ("name: True", Some("True")),
        ("name: false", None),
        ("name: 010", Some("8")),
        ("name: 0_", None),
        ("name: -0", None),
        ("name: 08", Some("08")),
        ("name: 1_000", Some("1000")),
        ("name: 1__0", Some("10")),
        ("name: 0x1F", Some("31")),
        ("name: 0b101", Some("5")),
        ("name: 0o17", Some("0o17")),
        ("name: 1e3", Some("1e3")),
        ("name: 1.0e+3", Some("1000.0")),
        ("name: 1.0e3", Some("1.0e3")),
        ("name: 1.", Some("1.0")),
        ("name: .5", Some("0.5")),
        ("name: 0.0", None),
        ("name: -0.", None),
        ("name: .nan", Some("nan")),
        ("name: .NaN", Some("nan")),
        ("name: +.inf", Some("inf")),
        ("name: -.inf", Some("-inf")),
        ("name: 1:20", Some("80")),
        ("name: 1:60", Some("1:60")),
        ("name: 1.5:30", Some("1.5:30")),
        ("name: 1:20.5", Some("80.5")),
        ("name: 2026-01-01", Some("2026-01-01")),
        ("name: 2026-1-1", Some("2026-1-1")),
        ("name: 2026-01-01 10:00", Some("2026-01-01 10:00")),
        ("name: 2026-01-01 10:00:00", Some("2026-01-01 10:00:00")),
        ("name: 2026-01-01T10:00:00Z", Some("2026-01-01 10:00:00+00:00")),
        ("name: 2026-01-01 10:00:00.12", Some("2026-01-01 10:00:00.120000")),
        ("name: 2026-01-01T10:00:00+05:30", Some("2026-01-01 10:00:00+05:30")),
        ("name: 2026-01-01 10:00:00 -05:30", Some("2026-01-01 10:00:00-05:30")),
        ("name: 2026-02-30", None),
        ("name: 2026-01-01T25:00:00", None),
        ("name: ~", None),
        ("name: null", None),
        ("name: Null", None),
        ("name: ''", None),
        ("name: \"\"", None),
        ("name:", None),
        ("name: \"yes\"", Some("yes")),
        ("name: 'no'", Some("no")),
        ("name: \"1_000\"", Some("1_000")),
        ("name: !!str 5", Some("5")),
        ("name: !!int 5", Some("5")),
        ("name: !!int \"5\"", Some("5")),
        ("name: !!int abc", None),
        ("name: !!int 0o17", Some("15")),
        ("name: !!bool yes", Some("True")),
        ("name: !!bool maybe", None),
        ("name: !!null x", None),
        ("name: !!float 1", Some("1.0")),
        ("name: !! 5", None),
        ("name: ! 5", Some("5")),
        ("name: ! bar", Some("bar")),
        ("name: !foo bar", None),
        ("name: !!seq 5", None),
        ("name: <<", None),
        ("name: =", None),
        ("=: x\nname: y", Some("y")),
        ("name: x\nother: =", None),
        ("name: x\nother: <<", None),
        ("<<: {name: From}", Some("From")),
        ("base: &b {name: From Base}\n<<: *b", Some("From Base")),
        ("name: [a, b]", Some("['a', 'b']")),
        ("name: [a, [b]]", Some("['a', ['b']]")),
        ("name: {a: 1}", Some("{'a': 1}")),
        ("name: []", None),
        ("name: {}", None),
        ("name: [1, 2, 1.0, true, yes, 1_000]", Some("[1, 2, 1.0, True, True, 1000]")),
        ("name: {a: 1, a: 2}", Some("{'a': 2}")),
        ("name: {1: a, true: b}", Some("{1: 'b'}")),
        ("name: [a: 1]", Some("[{'a': 1}]")),
        ("name: {a}", Some("{'a': None}")),
        ("name: [1, 2", None),
        ("name: First\nother: x\nname: Second", Some("Second")),
        ("name: a\nmeta: {k: 1, k: 2}", Some("a")),
        ("name: a\nmeta: {k: 1, k: 2}\nname: ''", None),
        ("name: foo\n  bar", Some("foo bar")),
        ("name: foo\n\n  bar", Some("foo\nbar")),
        ("name: 'it''s'", Some("it's")),
        ("name: \"x\\ty\"", Some("x\ty")),
        ("name: |\n  hi\n  there", Some("hi\nthere")),
        ("name: >-\n  fo\n  ld", Some("fo ld")),
        ("name: x # comment", Some("x")),
        ("name: &a yes\nother: *a", Some("True")),
        ("name: *nope", None),
        ("name: &a 1\nother: *a", Some("1")),
        ("a: 1\nb:", None),
        ("- a", None),
        ("x", None),
        ("name: x\n---\nname: y", None),
        ("name: x\n...", Some("x")),
        ("name: 2026-01-01 10:00:00\nother: x", Some("2026-01-01 10:00:00")),
        ("name: \"2026-01-01\"", Some("2026-01-01")),
        ("name: .inf", Some("inf")),
        ("name: -1_0.5", Some("-10.5")),
        ("name: 1_0.5e+2", Some("1050.0")),
    ];

    #[test]
    fn name_matches_pyyaml_reference_table() {
        for (block, expected) in PYYAML_REFERENCE {
            assert_eq!(name_of(block).as_deref(), *expected, "frontmatter {:?}", block);
        }
    }
}
