//! Port of modules/coder/module.py: sandboxed file tools for the AI, plus the webui
//! extension routes (file tree/editor/search and git panel).

#[path = "../../../../generated/modules/coder.rs"]
pub mod meta;
pub mod syntax;

use crate::pyos;
use anyhow::anyhow;
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::{py_str, truthy};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

const REGEX_BACKTRACK_LIMIT: usize = 1_000_000;
const BUILTIN_TEMPLATES_PATH: &str = "modules/coder/templates";
/// files the webui editor refuses to open (binary / huge)
const UI_MAX_READ_BYTES: u64 = 1_000_000;

pub struct Coder {
    base: ModuleBase,
    disabled: Mutex<Vec<String>>,
}

type R<T> = Result<T, String>;

fn err(msg: impl Into<String>) -> anyhow::Error {
    anyhow!(msg.into())
}

/// `(body or {}).get(key, "")` as a string
fn sget(m: &Map<String, Value>, key: &str) -> String {
    match m.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => py_str(v),
    }
}

/// python truthiness of a str-or-other value fetched with a "" default
fn present(m: &Map<String, Value>, key: &str) -> bool {
    m.get(key).map(truthy).unwrap_or(false)
}

/// python `base64.b64decode(s)` on a str (validate=False). `None` = python raises.
/// non-ASCII text is a ValueError; characters outside the alphabet are skipped; '=' is
/// skipped too and only decides the end: a trailing quad must be completed by padding.
/// non-zero trailing bits are accepted. checked against python 3.14 (see tests).
fn py_b64decode(s: &str) -> Option<Vec<u8>> {
    if !s.is_ascii() {
        return None;
    }
    let mut out = Vec::new();
    let mut quad_pos = 0u8;
    let mut left = 0u8;
    let mut pads = 0u8;
    for c in s.bytes() {
        if c == b'=' {
            pads += 1;
            continue;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        pads = 0;
        match quad_pos {
            0 => left = v,
            1 => {
                out.push((left << 2) | (v >> 4));
                left = v & 0x0f;
            }
            2 => {
                out.push((left << 4) | (v >> 2));
                left = v & 0x03;
            }
            _ => out.push((left << 6) | v),
        }
        quad_pos = (quad_pos + 1) % 4;
    }
    match quad_pos {
        0 => Some(out),
        1 => None,
        q if q + pads >= 4 => Some(out),
        _ => None,
    }
}

const PARSE_FAILED_MSG: &str = "Failed to parse the file for syntax errors. For safety, the file was not written to disk.";

/// python `_check_syntax`: the errors that block a write (empty = the write may go ahead).
/// Languages whose grammar is not compiled in pass, see `syntax.rs`.
fn check_syntax(code: &str, file_path: &str) -> Vec<String> {
    let Some(lang) = syntax::detect_language_from_path(file_path) else { return Vec::new() };
    if matches!(lang, "vimdoc" | "html" | "htm") || !syntax::is_supported(lang) {
        return Vec::new();
    }
    gate_errors(syntax::diagnostics(code, lang))
}

/// python: a falsy tree-sitter result is reported as the single parse-failure error
fn gate_errors(diags: Option<Vec<syntax::Diagnostic>>) -> Vec<String> {
    match diags {
        None => vec![PARSE_FAILED_MSG.to_string()],
        Some(diags) => diags.iter().map(|d| format!("Line {}, Col {}: {}", d.start_line + 1, d.start_column + 1, d.message)).collect(),
    }
}

/// compile a user regex like python's `regex.compile(pattern, flags)`
fn compile_regex(pattern: &str, ignore_case: bool) -> R<fancy_regex::Regex> {
    let mut b = fancy_regex::RegexBuilder::new(pattern);
    b.case_insensitive(ignore_case);
    b.backtrack_limit(REGEX_BACKTRACK_LIMIT);
    b.build().map_err(|e| e.to_string())
}

/// `pattern.search(line)` where `line` keeps its trailing newline, as python's readlines and
/// line iteration do: patterns such as `return\s` or `\n` can consume it. python's `$` also
/// matches just before that newline, which fancy_regex's `$` does not, so the line also counts
/// when the pattern matches it without the newline.
fn re_search(re: &fancy_regex::Regex, line: &str) -> R<bool> {
    if re.is_match(line).map_err(|e| e.to_string())? {
        return Ok(true);
    }
    match line.strip_suffix('\n') {
        Some(l) => re.is_match(l).map_err(|e| e.to_string()),
        None => Ok(false),
    }
}

/// python list slicing helper for possibly negative start
fn py_slice<T: Clone>(v: &[T], start: i64, end: Option<i64>) -> Vec<T> {
    let n = v.len() as i64;
    let norm = |i: i64| -> i64 {
        if i < 0 {
            (n + i).max(0)
        } else {
            i.min(n)
        }
    };
    let s = norm(start);
    let e = end.map(norm).unwrap_or(n);
    if s >= e {
        return Vec::new();
    }
    v[s as usize..e as usize].to_vec()
}

fn context_lists(all_lines: &[&str], line_num: usize, context: i64) -> (Vec<Value>, Vec<Value>) {
    let mut context_lines: Vec<String> = Vec::new();
    let start = (line_num as i64 - 1 - context).max(0);
    let end = (all_lines.len() as i64).min(line_num as i64 + context);
    let mut i = start;
    while i < end {
        if (i + 1) as usize != line_num {
            context_lines.push(pyos::rstrip(all_lines[i as usize]).to_string());
        }
        i += 1;
    }
    let before = py_slice(&context_lines, 0, Some(context));
    let after = if context > 0 { py_slice(&context_lines, -context, None) } else { Vec::new() };
    (before.into_iter().map(Value::String).collect(), after.into_iter().map(Value::String).collect())
}

/// python's `os.path` error for a None root (`_git_root` is None when no sandbox_paths are set)
const NONE_ROOT_ERR: &str = "expected str, bytes or os.PathLike object, not NoneType";

/// python `subprocess.run(cwd=None)`: no root runs git in the server's own working directory
fn git_command(root: Option<&str>, args: &[impl AsRef<std::ffi::OsStr>]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args).kill_on_drop(true).stdin(std::process::Stdio::null());
    if let Some(root) = root {
        cmd.current_dir(root);
    }
    cmd
}

/// run git; returns (returncode, raw stdout)
async fn run_git_bytes(root: Option<&str>, args: &[String], timeout: u64) -> (i32, Vec<u8>) {
    let mut cmd = git_command(root, args);
    match tokio::time::timeout(Duration::from_secs(timeout), cmd.output()).await {
        Err(_) => (124, b"git command timed out".to_vec()),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => (127, b"git executable not found".to_vec()),
        Ok(Err(e)) => (1, e.to_string().into_bytes()),
        Ok(Ok(out)) => (out.status.code().unwrap_or(-1), out.stdout),
    }
}

/// run git; returns (ok, output)
async fn run_git(root: Option<&str>, args: &[&str], accept: &[i32]) -> (bool, String) {
    let mut cmd = git_command(root, args);
    let out = match tokio::time::timeout(Duration::from_secs(15), cmd.output()).await {
        Err(_) => return (false, "git command timed out".into()),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return (false, "git executable not found".into()),
        Ok(Err(e)) => return (false, e.to_string()),
        Ok(Ok(o)) => o,
    };
    // python runs this in text mode: strict utf-8 (stdout is decoded before stderr, and a
    // decode error wins over the return code) and universal newlines
    let decode = |bytes: &[u8]| pyos::decode_utf8(bytes).map(|s| pyos::universal_newlines(&s));
    let stdout = match decode(&out.stdout) {
        Ok(s) => s.trim().to_string(),
        Err(e) => return (false, e),
    };
    let stderr = match decode(&out.stderr) {
        Ok(s) => s.trim().to_string(),
        Err(e) => return (false, e),
    };
    let rc = out.status.code().unwrap_or(-1);
    if !accept.contains(&rc) {
        let msg = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("git {} failed (code {})", args.first().copied().unwrap_or(""), rc)
        };
        return (false, msg);
    }
    (true, if stdout.is_empty() { stderr } else { stdout })
}

async fn git(root: Option<&str>, args: &[&str]) -> (bool, String) {
    run_git(root, args, &[0]).await
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// `git status --porcelain=v1 -z` parser (see python _parse_porcelain_z)
pub fn parse_porcelain_z(raw: &[u8]) -> Vec<Value> {
    let decode = |d: &[u8]| String::from_utf8_lossy(d).to_string();
    let fields: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
    let mut entries = Vec::new();
    let mut idx = 0;
    while idx < fields.len() {
        let field = fields[idx];
        idx += 1;
        if field.is_empty() {
            continue;
        }
        if field.len() < 3 || field[2] != b' ' {
            continue;
        }
        let x = field[0] as char;
        let y = field[1] as char;
        let mut path = decode(&field[3..]);
        if (matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C')) && idx < fields.len() {
            let newp = fields[idx];
            idx += 1;
            if !newp.is_empty() {
                path = decode(newp);
            }
        }
        entries.push(json!({
            "path": path,
            "xy": format!("{}{}", x, y),
            "staged": !matches!(x, ' ' | '?'),
            "untracked": x == '?' && y == '?',
        }));
    }
    entries
}

fn parse_log(raw: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(raw);
    let mut entries = Vec::new();
    for line in py_splitlines(&text) {
        if line.trim().is_empty() {
            continue;
        }
        let (h, msg) = match line.split_once('\t') {
            Some((h, m)) => (h, m),
            None => (line, ""),
        };
        entries.push(json!({"hash": h.trim(), "msg": msg}));
    }
    entries
}

/// python str.splitlines() (common line boundaries)
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes: Vec<(usize, char)> = s.char_indices().collect();
    let mut i = 0;
    while i < bytes.len() {
        let (pos, c) = bytes[i];
        let is_break = matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}');
        if is_break {
            out.push(&s[start..pos]);
            let mut next = pos + c.len_utf8();
            if c == '\r' && i + 1 < bytes.len() && bytes[i + 1].1 == '\n' {
                next += 1;
                i += 1;
            }
            start = next;
        }
        i += 1;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// side-by-side rows from a unified diff (python _parse_split_diff)
pub fn parse_split_diff(unified: &str) -> Vec<Value> {
    let hunk_re = regex::Regex::new(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@").unwrap();
    let mut rows: Vec<Value> = Vec::new();
    let mut old_line: i64 = 0;
    let mut new_line: i64 = 0;
    let mut del_buf: Vec<(i64, String)> = Vec::new();
    let mut add_buf: Vec<(i64, String)> = Vec::new();

    fn flush(rows: &mut Vec<Value>, del_buf: &mut Vec<(i64, String)>, add_buf: &mut Vec<(i64, String)>) {
        while !del_buf.is_empty() && !add_buf.is_empty() {
            let d = del_buf.remove(0);
            let a = add_buf.remove(0);
            rows.push(json!({"old_num": d.0, "old_text": d.1, "new_num": a.0, "new_text": a.1, "type": "mod"}));
        }
        for d in del_buf.drain(..) {
            rows.push(json!({"old_num": d.0, "old_text": d.1, "new_num": null, "new_text": null, "type": "del"}));
        }
        for a in add_buf.drain(..) {
            rows.push(json!({"old_num": null, "old_text": null, "new_num": a.0, "new_text": a.1, "type": "add"}));
        }
    }

    for line in unified.split('\n') {
        if let Some(c) = hunk_re.captures(line) {
            flush(&mut rows, &mut del_buf, &mut add_buf);
            old_line = c[1].parse().unwrap_or(0);
            new_line = c[2].parse().unwrap_or(0);
            continue;
        }
        if line.starts_with("---") || line.starts_with("+++") || line.starts_with('\\') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('-') {
            if !add_buf.is_empty() {
                flush(&mut rows, &mut del_buf, &mut add_buf);
            }
            del_buf.push((old_line, rest.to_string()));
            old_line += 1;
        } else if let Some(rest) = line.strip_prefix('+') {
            add_buf.push((new_line, rest.to_string()));
            new_line += 1;
        } else if let Some(rest) = line.strip_prefix(' ') {
            flush(&mut rows, &mut del_buf, &mut add_buf);
            rows.push(json!({"old_num": old_line, "old_text": rest, "new_num": new_line, "new_text": rest, "type": "equal"}));
            old_line += 1;
            new_line += 1;
        }
    }
    flush(&mut rows, &mut del_buf, &mut add_buf);
    rows
}

/// python `int(x)` of a query value
fn py_int(v: Option<&Value>, default: i64) -> anyhow::Result<i64> {
    match v {
        None => Ok(default),
        Some(Value::Number(n)) => Ok(n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64)),
        Some(Value::Bool(b)) => Ok(*b as i64),
        Some(Value::String(s)) => parse_py_int(s).ok_or_else(|| anyhow!("invalid literal for int() with base 10: {}", pyos::py_quote(s))),
        Some(other) => Err(anyhow!("int() argument must be a string, a bytes-like object or a real number, not '{}'", py_type(other))),
    }
}

/// the value of a unicode decimal digit (general category Nd), or None. Nd digits come in
/// contiguous runs of ten, 0 through 9, so the value is the offset from the start of the run
fn unicode_decimal_value(c: char) -> Option<u32> {
    use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};
    let is_nd = |c: char| c.general_category() == GeneralCategory::DecimalNumber;
    if !is_nd(c) {
        return None;
    }
    let mut start = c as u32;
    while start > 0 && char::from_u32(start - 1).is_some_and(is_nd) {
        start -= 1;
    }
    Some((c as u32 - start) % 10)
}

/// python `int(s)` for a str: surrounding whitespace, an optional sign, Unicode decimal digits
/// and single underscores between digits (`int(" ٣_0 ") == 30`). None where python raises.
fn parse_py_int(s: &str) -> Option<i64> {
    // python strips the White_Space code points (int("\u{3000}5") == 5), but not \x1c-\x1f
    let t = s.trim_matches(char::is_whitespace);
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return None;
    }
    let mut value: i64 = 0;
    for c in body.chars().filter(|&c| c != '_') {
        // char::is_numeric is too wide ('½' is numeric but not a digit): only Nd counts
        let d = if c.is_ascii() { c.to_digit(10)? } else { unicode_decimal_value(c)? };
        let d = i64::from(d);
        value = value.checked_mul(10)?;
        value = if neg { value.checked_sub(d)? } else { value.checked_add(d)? };
    }
    Some(value)
}

/// python's `folder_blacklist` on the right of `name in folder_blacklist`: a list or dict tests
/// membership, a str tests substring, and any other value (None, numbers, bools) makes python
/// raise TypeError at the first `in`. `from_value` is the raw config value (glob, folder_grep);
/// `from_value_or_empty` is python's `(config.get(...) or [])`, where falsy values are empty.
#[derive(Debug, PartialEq)]
enum Blacklist {
    Members(Vec<String>),
    Substring(String),
    /// the python type name of a value that `in` cannot test
    NotContainer(&'static str),
}

impl Blacklist {
    fn from_value(v: Option<Value>) -> Self {
        match v {
            Some(Value::String(s)) => Self::Substring(s),
            Some(Value::Array(a)) => Self::Members(a.iter().filter_map(|x| x.as_str().map(String::from)).collect()),
            Some(Value::Object(m)) => Self::Members(m.keys().cloned().collect()),
            Some(other) => Self::NotContainer(py_type(&other)),
            None => Self::NotContainer("NoneType"),
        }
    }

    fn from_value_or_empty(v: Option<Value>) -> Self {
        match v {
            Some(v) if truthy(&v) => Self::from_value(Some(v)),
            _ => Self::Members(Vec::new()),
        }
    }

    fn contains(&self, name: &str) -> anyhow::Result<bool> {
        match self {
            Self::Members(v) => Ok(v.iter().any(|x| x == name)),
            Self::Substring(s) => Ok(s.contains(name)),
            Self::NotContainer(t) => anyhow::bail!("argument of type '{t}' is not a container or iterable"),
        }
    }
}

/// python `[d for d in dirs if not d.startswith('.') and d not in blacklist]` applied in place.
/// the `in` test only runs for non-hidden names, so the first name whose test raises stops it.
fn prune_dirs(dirs: &mut Vec<String>, blacklist: &Blacklist) -> anyhow::Result<()> {
    let mut kept = Vec::with_capacity(dirs.len());
    for d in dirs.iter() {
        if d.starts_with('.') {
            continue;
        }
        if !blacklist.contains(d)? {
            kept.push(d.clone());
        }
    }
    *dirs = kept;
    Ok(())
}

/// the coder's POST routes that read `(body or {}).get(...)` in python
const BODY_ROUTES: &[&str] = &[
    "save",
    "lint",
    "create",
    "move",
    "delete",
    "duplicate",
    "search",
    "git_stage",
    "git_unstage",
    "git_commit",
    "git_branch",
    "git_push",
    "git_fetch",
    "git_pull",
    "git_branch_create",
    "git_branch_delete",
    "git_resolve",
    "git_revert_file",
    "upload",
];

/// python's `(body or {}).get(...)` in the coder's POST routes: a truthy body that is not a dict
/// makes `.get` raise AttributeError, which the webui reports with that exception text.
/// python refuses in read-only mode before it reads the body (every BODY route except lint and
/// search), so a read-only coder never reaches the error there.
fn body_dict(path: &str, body: Option<Value>, read_only: bool) -> anyhow::Result<Map<String, Value>> {
    let refuses_first = read_only && !matches!(path, "lint" | "search");
    match body {
        Some(Value::Object(m)) => Ok(m),
        Some(v) if truthy(&v) && BODY_ROUTES.contains(&path) && !refuses_first => {
            anyhow::bail!("'{}' object has no attribute 'get'", py_type(&v))
        }
        _ => Ok(Map::new()),
    }
}

fn py_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

impl Coder {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base, disabled: Mutex::new(Vec::new()) }
    }

    fn result(&self, data: impl Into<Value>, success: bool) -> Value {
        self.base.result(data.into(), success)
    }

    fn cfg(&self, key: &str) -> Option<Value> {
        self.base.config.get1(key).filter(|v| !v.is_null())
    }

    fn cfg_bool(&self, key: &str) -> bool {
        self.cfg(key).map(|v| truthy(&v)).unwrap_or(false)
    }

    fn cfg_list(&self, key: &str) -> Vec<String> {
        match self.cfg(key) {
            Some(Value::Array(a)) => a.iter().map(py_str).collect(),
            _ => Vec::new(),
        }
    }

    /// python `config.get("folder_blacklist")` tested with `in` directly (glob, folder_grep)
    fn blacklist(&self) -> Blacklist {
        Blacklist::from_value(self.cfg("folder_blacklist"))
    }

    /// python `config.get("folder_blacklist") or []` (tree, files, search, git_repos)
    fn blacklist_or_empty(&self) -> Blacklist {
        Blacklist::from_value_or_empty(self.cfg("folder_blacklist"))
    }

    fn read_only(&self) -> bool {
        self.cfg_bool("read-only")
    }

    // ---------------- sandbox helpers

    /// sandbox basenames (creates the sandbox folders)
    fn get_sandbox_paths(&self) -> R<Vec<String>> {
        let paths = self.cfg_list("sandbox_paths");
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        for p in &paths {
            let full = pyos::expanduser(p);
            let full = full.trim_end_matches('/');
            pyos::makedirs(full, true)?;
        }
        Ok(paths.iter().map(|p| pyos::basename(p.trim_end_matches('/')).to_string()).collect())
    }

    /// basename'd sandbox -> full path
    fn get_full_sandbox_path(&self, requested: &str) -> R<Option<String>> {
        let paths = self.cfg_list("sandbox_paths");
        if paths.is_empty() {
            return Ok(None);
        }
        for p in &paths {
            if pyos::basename(p.trim_end_matches('/')) == requested.trim_end_matches('/') {
                return Ok(Some(pyos::expanduser(p).trim_end_matches('/').to_string()));
            }
        }
        Err("That sandbox does not exist".into())
    }

    fn full_sandbox(&self, sandbox: &str) -> R<String> {
        self.get_full_sandbox_path(sandbox)?.ok_or_else(|| "'NoneType' object has no attribute 'replace'".to_string())
    }

    fn get_sandbox_subpath(&self, sandbox: &str, requested: &str) -> R<String> {
        let requested = requested.strip_prefix(sandbox).unwrap_or(requested);
        let base = self.full_sandbox(sandbox)?;
        ol_core::paths::sandbox_path(&base, Some(requested)).map_err(|e| e.to_string())
    }

    // ---------------- treesitter helpers


    // ---------------- template helpers

    fn get_template_folders(&self) -> Vec<String> {
        let mut folders = self.cfg_list("template_paths");
        if self.cfg_bool("enable_builtin_templates") {
            folders.insert(0, BUILTIN_TEMPLATES_PATH.to_string());
        }
        folders
    }

    /// python _get_templates (on a listdir error python returns a result dict, whose keys
    /// then get listed by the system prompt - replicated)
    fn get_templates(&self) -> R<Vec<String>> {
        let mut all = Vec::new();
        for folder in self.get_template_folders() {
            // python calls core.get_path outside its try: an error there propagates
            let path = ol_core::paths::get_path(&folder).map_err(|e| e.to_string())?;
            match pyos::listdir(&path) {
                Ok(t) => all.extend(t),
                Err(_) => return Ok(vec!["status".into(), "content".into()]),
            }
        }
        Ok(all)
    }

    // ---------------- tools

    async fn tool_glob(&self, a: &ToolArgs) -> anyhow::Result<Value> {
        let sandbox = a.str_or("sandbox", "");
        let mut pattern = a.str_or("pattern", "");
        let sub_path = a.raw("sub_path").filter(|v| truthy(v)).map(py_str).unwrap_or_else(|| ".".into());
        let max_results = a.i64("max_results").unwrap_or(200);
        let sandbox_path = self.full_sandbox(&sandbox).map_err(err)?;
        let target_path = self.get_sandbox_subpath(&sandbox, &sub_path).map_err(err)?;
        let blacklist = self.blacklist();

        if pattern.starts_with(&sandbox) {
            pattern = pattern[sandbox.len()..].to_string();
        }
        let mut matches = pyos::glob(&pyos::join(&target_path, &pattern), true);
        matches.sort();
        let mut results = Vec::new();
        let mut total: i64 = 0;
        for m in matches {
            let rel = pyos::relpath(&m, &target_path);
            let mut skip = false;
            for part in rel.split('/') {
                match blacklist.contains(part) {
                    Ok(true) => {
                        skip = true;
                        break;
                    }
                    Ok(false) => {}
                    // python: the TypeError is caught by glob's try and returned as result text
                    Err(e) => return Ok(self.result(e.to_string(), false)),
                }
            }
            if skip {
                continue;
            }
            total += 1;
            if total > max_results {
                break;
            }
            if let Err(e) = ol_core::paths::sandbox_path(&sandbox_path, Some(&rel)) {
                return Ok(self.result(e.to_string(), false));
            }
            results.push(Value::String(rel));
        }
        if total > max_results {
            return Ok(self.result(
                json!({
                    "results": results,
                    "truncated": true,
                    "total": total,
                    "note": format!("showing first {} of {} matches - narrow your pattern to see more", max_results, total)
                }),
                true,
            ));
        }
        Ok(self.result(results, true))
    }

    async fn tool_folder_grep(&self, a: &ToolArgs) -> anyhow::Result<Value> {
        let sandbox = a.str_or("sandbox", "");
        let sub_path = a.str_or("sub_path", "");
        let pattern = a.str_or("regex_pattern", "");
        let case_sensitive = a.raw("case_sensitive").map(truthy).unwrap_or(false);
        let context = a.i64("context").unwrap_or(0);
        let file_extensions = a.raw("file_extensions").filter(|v| truthy(v)).cloned();
        let max_matches = a.i64("max_matches").unwrap_or(30);
        let blacklist = self.blacklist();

        if matches!(pattern.trim(), "." | "*") {
            return Ok(self.result("Forbidden pattern detected. Use a more specific search!", false));
        }
        let re = compile_regex(&pattern, !case_sensitive).map_err(err)?;
        let target_path = self.get_sandbox_subpath(&sandbox, &sub_path).map_err(err)?;
        if !pyos::isdir(&target_path) {
            return Ok(self.result("Target path is not a folder. Grep only works on folders.", false));
        }

        let ext_ok = |file: &str| -> bool {
            let Some(exts) = &file_extensions else { return true };
            let (_, ext) = pyos::splitext(file);
            let ext = ext.get(1..).unwrap_or("").to_string();
            match exts {
                Value::Array(a) => a.iter().any(|v| v.as_str() == Some(ext.as_str())),
                Value::String(s) => s.contains(&ext),
                _ => false,
            }
        };

        let mut matches: Vec<Value> = Vec::new();
        let mut walk_err: Option<anyhow::Error> = None;
        let early = pyos::walk(&target_path, &mut |dirpath, folders, files| {
            if let Err(e) = prune_dirs(folders, &blacklist) {
                walk_err = Some(e);
                return ControlFlow::Break(None);
            }
            if matches.len() as i64 >= max_matches {
                return ControlFlow::Break(None);
            }
            for file in files {
                if !ext_ok(file) {
                    continue;
                }
                let filepath = pyos::join(dirpath, file);
                let Ok(content) = pyos::read_text(&filepath) else { continue };
                let all_lines = pyos::readlines(&content);
                for (i, line) in all_lines.iter().enumerate() {
                    let line_num = i + 1;
                    match re_search(&re, line) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        // python: exception inside the per-file try -> skip the file
                        Err(_) => break,
                    }
                    let mut m = Map::new();
                    m.insert("file".into(), json!(pyos::relpath(&filepath, &target_path)));
                    m.insert("line_num".into(), json!(line_num));
                    m.insert("line".into(), json!(pyos::rstrip(line)));
                    if context != 0 {
                        let (before, after) = context_lists(&all_lines, line_num, context);
                        m.insert("context_before".into(), Value::Array(before));
                        m.insert("context_after".into(), Value::Array(after));
                    }
                    matches.push(Value::Object(m));
                    if matches.len() as i64 >= max_matches {
                        return ControlFlow::Break(Some(json!({
                            "matches": matches.clone(),
                            "note": format!("Showing up to {} matches. More results may be available — narrow your search pattern to see them.", max_matches)
                        })));
                    }
                }
            }
            ControlFlow::Continue(())
        });
        if let Some(e) = walk_err {
            return Err(e);
        }
        if let Some(Some(v)) = early {
            return Ok(self.result(v, true));
        }
        if !matches.is_empty() {
            Ok(self.result(matches, true))
        } else {
            Ok(self.result("No matches found", false))
        }
    }

    async fn tool_file_create(&self, a: &ToolArgs) -> anyhow::Result<Value> {
        let sandbox = a.str_or("sandbox", "");
        let path = a.str_or("path", "");
        let content = a.str_or("content", "");
        let target = self.get_sandbox_subpath(&sandbox, &path).map_err(err)?;
        let errors = check_syntax(&content, &target);
        if !errors.is_empty() {
            return Ok(self.result(json!({"errors": errors, "message": "Syntax errors detected! File was not written to disk."}), false));
        }
        let parent = pyos::dirname(&target);
        if !parent.is_empty() {
            pyos::makedirs(&parent, true).map_err(err)?;
        }
        if let Err(e) = pyos::create_new(&target, content.as_bytes()) {
            return Ok(self.result(e, false));
        }
        Ok(self.result(format!("File {} successfully created. Syntax verification passed - no need to verify further.", path), true))
    }

    async fn tool_file_move(&self, a: &ToolArgs) -> anyhow::Result<Value> {
        let sandbox = a.str_or("sandbox", "");
        let orig = self.get_sandbox_subpath(&sandbox, &a.str_or("orig_path", "")).map_err(err)?;
        let target = self.get_sandbox_subpath(&sandbox, &a.str_or("target_path", "")).map_err(err)?;
        if let Err(e) = pyos::shutil_move(&orig, &target) {
            return Ok(self.result(e, false));
        }
        Ok(self.result(format!("File successfully moved: {} -> {}", orig, target), true))
    }

    async fn tool_file_delete(&self, a: &ToolArgs) -> anyhow::Result<Value> {
        let sandbox = a.str_or("sandbox", "");
        let path = a.str_or("path", "");
        let target = self.get_sandbox_subpath(&sandbox, &path).map_err(err)?;
        if let Err(e) = std::fs::remove_file(&target) {
            return Ok(self.result(pyos::os_err(&e, &target), false));
        }
        Ok(self.result(format!("File {} successfully deleted", path), true))
    }

    async fn tool_folder_delete(&self, a: &ToolArgs) -> anyhow::Result<Value> {
        let sandbox = a.str_or("sandbox", "");
        let path = a.str_or("path", "");
        let target = self.get_sandbox_subpath(&sandbox, &path).map_err(err)?;
        if let Err(e) = std::fs::remove_dir(&target) {
            return Ok(self.result(pyos::os_err(&e, &target), false));
        }
        Ok(self.result(format!("Folder {} deleted", path), true))
    }

    async fn tool_file_outline(&self, a: &ToolArgs) -> anyhow::Result<Value> {
        let sandbox = a.str_or("sandbox", "");
        let target = self.get_sandbox_subpath(&sandbox, &a.str_or("path", "")).map_err(err)?;
        let content = match pyos::read_text(&target) {
            Ok(c) => c,
            Err(e) => return Ok(self.result(e, false)),
        };
        let structure = syntax::detect_language_from_path(&target).and_then(|lang| syntax::outline(&content, lang));
        match structure {
            Some(s) => Ok(self.result(s, true)),
            None => Ok(self.result("This file could not be read by the code inspector because it is not a source code file. Use file_read instead", false)),
        }
    }

    async fn tool_file_read(&self, a: &ToolArgs) -> anyhow::Result<Value> {
        let sandbox = a.str_or("sandbox", "");
        let target = self.get_sandbox_subpath(&sandbox, &a.str_or("path", "")).map_err(err)?;
        let content = match pyos::read_text(&target) {
            Ok(c) => c,
            Err(e) => return Ok(self.result(e, false)),
        };
        if content.is_empty() {
            return Ok(self.result("File was empty", false));
        }
        let lines: Vec<&str> = content.split('\n').collect();
        let total = lines.len() as i64;
        let max_lines = self.cfg("global_line_limit").and_then(|v| ol_core::pyjson::as_i64(&v)).unwrap_or(1000);

        let mut line_start = a.i64("line_start");
        let mut line_end = a.i64("line_end");
        if line_start.is_none() && line_end.is_none() {
            if total <= max_lines {
                line_start = Some(1);
                line_end = Some(total);
            } else {
                return Ok(self.result(format!("File is too large to read in full! ({} lines). Read it in chunks.", total), false));
            }
        }
        let mut line_start = line_start.unwrap_or(1) - 1;
        let mut line_end = line_end.unwrap_or(-1);
        if line_start < 0 {
            line_start = 1;
        }
        if line_end == -1 || line_end > total {
            line_end = total;
        } else if line_end < 1 {
            line_end = 1;
        }
        let hit_limit = line_end - line_start > max_lines;
        if hit_limit {
            line_end = line_start + max_lines;
        }
        let partial = py_slice(&lines, line_start, Some(line_end));
        let remaining = total - line_end;
        let truncated = remaining > 0 || hit_limit;
        let mut r = Map::new();
        r.insert("content".into(), json!(partial.join("\n")));
        r.insert("truncated".into(), json!(truncated));
        if truncated {
            let mut note = format!("Showing lines {}-{} of {}. {} lines remaining.", line_start + 1, line_end, total, remaining);
            if hit_limit {
                note.push_str(&format!(" WARNING: request clamped to global line limit of {} lines", max_lines));
            }
            r.insert("truncation_note".into(), json!(note));
        }
        r.insert("success".into(), json!(true));
        Ok(Value::Object(r))
    }

    async fn tool_file_grep(&self, a: &ToolArgs) -> anyhow::Result<Value> {
        let sandbox = a.str_or("sandbox", "");
        let target = self.get_sandbox_subpath(&sandbox, &a.str_or("file_path", "")).map_err(err)?;
        let case_sensitive = a.raw("case_sensitive").map(truthy).unwrap_or(true);
        let context = a.i64("context").unwrap_or(0);
        let max_matches = a.i64("max_matches").unwrap_or(30);
        let re = compile_regex(&a.str_or("regex_pattern", ""), !case_sensitive).map_err(err)?;

        let content = match pyos::read_text(&target) {
            Ok(c) => c,
            Err(e) => return Ok(self.result(e, false)),
        };
        let all_lines = pyos::readlines(&content);
        let mut matches = Vec::new();
        for (i, line) in all_lines.iter().enumerate() {
            let line_num = i + 1;
            match re_search(&re, line) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => return Ok(self.result(e, false)),
            }
            let mut m = Map::new();
            m.insert("line_num".into(), json!(line_num));
            m.insert("line".into(), json!(pyos::rstrip(line)));
            if context != 0 {
                let (before, after) = context_lists(&all_lines, line_num, context);
                m.insert("context_before".into(), Value::Array(before));
                m.insert("context_after".into(), Value::Array(after));
            }
            matches.push(Value::Object(m));
            if matches.len() as i64 >= max_matches {
                return Ok(self.result(
                    json!({
                        "matches": matches,
                        "note": format!("Showing up to {} matches. More results may be available — narrow your search pattern to see them.", max_matches)
                    }),
                    true,
                ));
            }
        }
        if !matches.is_empty() {
            Ok(self.result(matches, true))
        } else {
            Ok(self.result("No matches found", false))
        }
    }

    async fn tool_read_template(&self, a: &ToolArgs) -> anyhow::Result<Value> {
        let name = a.str_or("template_name", "");
        let folders = self.get_template_folders();
        if folders.is_empty() {
            return Err(anyhow!("no template folders were configured! this should never happen, since the builtin one is always included. notify the developer!"));
        }
        for folder in folders {
            let path = ol_core::paths::get_path(&folder)?;
            let templates = match pyos::listdir(&path) {
                Ok(t) => t,
                Err(e) => return Ok(self.result(e, false)),
            };
            if templates.contains(&name) {
                return Ok(match pyos::read_text(&pyos::join(&path, &name)) {
                    Ok(c) => self.result(c, true),
                    Err(e) => self.result(e, false),
                });
            }
        }
        Ok(self.result("Template not found!", false))
    }

    async fn tool_file_edit(&self, a: &ToolArgs) -> anyhow::Result<Value> {
        let sandbox = a.str_or("sandbox", "");
        let path = a.str_or("path", "");
        let original = a.str_or("original_code", "");
        let replacement = a.str_or("replacement_code", "");
        let target = self.get_sandbox_subpath(&sandbox, &path).map_err(err)?;
        let content = match pyos::read_text(&target) {
            Ok(c) => c,
            Err(e) => return Ok(self.result(e, false)),
        };
        if content.is_empty() {
            return Ok(self.result("File is empty!", false));
        }
        let new_content = content.replace(&original, &replacement);
        if new_content == content {
            return Ok(self.result("Failed to match your original code string against the file contents. Try again!", false));
        }
        let errors = check_syntax(&new_content, &target);
        if !errors.is_empty() {
            return Ok(self.result(json!({"errors": errors, "message": "Syntax errors detected! File was not written to disk."}), false));
        }
        if let Err(e) = pyos::write_file(&target, new_content.as_bytes()) {
            return Ok(self.result(e, false));
        }
        Ok(self.result(format!("Successfully edited file {}. Syntax verification passed - no need to verify further.", path), true))
    }

    // ---------------- webui routes

    fn route_sandboxes(&self) -> anyhow::Result<Value> {
        let mode = self.cfg("editor_mode").filter(truthy).map(|v| py_str(&v)).unwrap_or_else(|| "standard".into());
        // python: int(self.config.get("autosave_interval") or 0)
        let autosave = py_int(self.cfg("autosave_interval").filter(truthy).as_ref(), 0)?;
        Ok(json!({
            "sandboxes": self.get_sandbox_paths().map_err(err)?,
            "read_only": self.read_only(),
            "editor_mode": if self.cfg_bool("enable_webui_extension") { mode } else { "standard".into() },
            "autosave_interval": autosave,
        }))
    }

    fn route_tree(&self, q: &Map<String, Value>) -> Value {
        let sandbox = sget(q, "sandbox");
        let rel_path = sget(q, "path");
        if sandbox.is_empty() {
            return self.result("no sandbox given", false);
        }
        let target = match self.get_sandbox_subpath(&sandbox, &rel_path) {
            Ok(t) => t,
            Err(e) => return self.result(e, false),
        };
        if !pyos::isdir(&target) {
            return self.result("not a folder", false);
        }
        let blacklist = self.blacklist_or_empty();
        let names = match pyos::listdir(&target) {
            Ok(n) => n,
            Err(e) => return self.result(e, false),
        };
        // python: `is_dir and name in blacklist` inside the listing try, so a raise becomes result text
        let built: anyhow::Result<Vec<(bool, String, Value)>> = names
            .into_iter()
            .map(|name| {
                let full = pyos::join(&target, &name);
                let is_dir = pyos::isdir(&full);
                let child_rel = pyos::normpath(&pyos::join(&rel_path, &name));
                let blacklisted = is_dir && blacklist.contains(&name)?;
                let v = json!({
                    "name": name,
                    "path": child_rel,
                    "is_dir": is_dir,
                    "hidden": name.starts_with('.'),
                    "blacklisted": blacklisted,
                });
                Ok((is_dir, name.to_lowercase(), v))
            })
            .collect();
        let mut entries = match built {
            Ok(e) => e,
            Err(e) => return self.result(e.to_string(), false),
        };
        entries.sort_by(|a, b| (!a.0, &a.1).cmp(&(!b.0, &b.1)));
        json!({"path": rel_path, "entries": entries.into_iter().map(|e| e.2).collect::<Vec<_>>()})
    }

    fn route_files(&self, q: &Map<String, Value>) -> Value {
        let sandbox = sget(q, "sandbox");
        if sandbox.is_empty() {
            return self.result("no sandbox given", false);
        }
        let target = match self.get_full_sandbox_path(&sandbox) {
            Ok(Some(t)) if pyos::isdir(&t) => t,
            Ok(_) => return self.result("sandbox not found", false),
            Err(e) => return self.result(e, false),
        };
        let blacklist = self.blacklist_or_empty();
        let mut files: Vec<String> = Vec::new();
        // python: the walk sits in a try, so a raise from the blacklist test becomes result text
        let mut walk_err: Option<anyhow::Error> = None;
        pyos::walk::<()>(&target, &mut |dirpath, folders, filenames| {
            if let Err(e) = prune_dirs(folders, &blacklist) {
                walk_err = Some(e);
                return ControlFlow::Break(());
            }
            for name in filenames {
                if name.starts_with('.') {
                    continue;
                }
                files.push(pyos::relpath(&pyos::join(dirpath, name), &target));
            }
            ControlFlow::Continue(())
        });
        if let Some(e) = walk_err {
            return self.result(e.to_string(), false);
        }
        files.sort();
        json!({"files": files})
    }

    fn route_read(&self, q: &Map<String, Value>) -> Value {
        let sandbox = sget(q, "sandbox");
        let rel_path = sget(q, "path");
        if sandbox.is_empty() || rel_path.is_empty() {
            return self.result("sandbox and path are both required", false);
        }
        let target = match self.get_sandbox_subpath(&sandbox, &rel_path) {
            Ok(t) => t,
            Err(e) => return self.result(e, false),
        };
        let size = match std::fs::metadata(&target) {
            Ok(m) => m.len(),
            Err(e) => return self.result(pyos::os_err(&e, &target), false),
        };
        if size > UI_MAX_READ_BYTES {
            return self.result(format!("file is too large to edit in the webui ({} bytes)", size), false);
        }
        let content = match pyos::read_text(&target) {
            Ok(c) => c,
            Err(e) if pyos::is_decode_error(&e) => return self.result("this looks like a binary file and can't be edited here", false),
            Err(e) => return self.result(e, false),
        };
        json!({"path": rel_path, "content": content, "read_only": self.read_only()})
    }

    fn route_save(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let rel_path = sget(b, "path");
        let content = sget(b, "content");
        if sandbox.is_empty() || rel_path.is_empty() {
            return self.result("sandbox and path are both required", false);
        }
        let target = match self.get_sandbox_subpath(&sandbox, &rel_path) {
            Ok(t) => t,
            Err(e) => return self.result(e, false),
        };
        let errors = check_syntax(&content, &target);
        if !errors.is_empty() {
            return self.result(json!({"errors": errors, "message": "Syntax errors detected! File was not written to disk."}), false);
        }
        if let Err(e) = pyos::write_file(&target, content.as_bytes()) {
            return self.result(e, false);
        }
        self.result(format!("saved {}", rel_path), true)
    }

    fn route_lint(&self, b: &Map<String, Value>) -> Value {
        let sandbox = sget(b, "sandbox");
        let rel_path = sget(b, "path");
        let content = sget(b, "content");
        if sandbox.is_empty() || rel_path.is_empty() {
            return self.result("sandbox and path are both required", false);
        }
        let target = match self.get_sandbox_subpath(&sandbox, &rel_path) {
            Ok(t) => t,
            Err(e) => return self.result(e, false),
        };
        let Some(lang) = syntax::detect_language_from_path(&target) else { return json!({"annotations": []}) };
        if matches!(lang, "vimdoc" | "html" | "htm") {
            return json!({"annotations": []});
        }
        let diags = syntax::diagnostics(&content, lang).unwrap_or_default();
        let annotations: Vec<Value> = diags
            .iter()
            .map(|d| {
                json!({
                    "line": d.start_line + 1,
                    "col": d.start_column + 1,
                    "end_line": d.end_line + 1,
                    "end_col": d.end_column + 1,
                    "message": d.message,
                })
            })
            .collect();
        json!({"annotations": annotations})
    }

    fn route_create(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let rel_path = sget(b, "path");
        let is_dir = present(b, "is_dir");
        if sandbox.is_empty() || rel_path.is_empty() {
            return self.result("sandbox and path are both required", false);
        }
        let target = match self.get_sandbox_subpath(&sandbox, &rel_path) {
            Ok(t) => t,
            Err(e) => return self.result(e, false),
        };
        let r = if is_dir {
            pyos::makedirs(&target, false)
        } else {
            let parent = pyos::dirname(&target);
            let mk = if parent.is_empty() { Ok(()) } else { pyos::makedirs(&parent, true) };
            mk.and_then(|_| pyos::create_new(&target, b""))
        };
        if let Err(e) = r {
            return self.result(e, false);
        }
        self.result(format!("created {}", rel_path), true)
    }

    fn route_move(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let src = sget(b, "src");
        let dst = sget(b, "dst");
        if sandbox.is_empty() || src.is_empty() || dst.is_empty() {
            return self.result("sandbox, src and dst are all required", false);
        }
        let (src_path, dst_path) = match (self.get_sandbox_subpath(&sandbox, &src), self.get_sandbox_subpath(&sandbox, &dst)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return self.result(e, false),
        };
        if pyos::exists(&dst_path) {
            return self.result(format!("destination already exists: {}", dst), false);
        }
        let parent = pyos::dirname(&dst_path);
        let r = if parent.is_empty() { Ok(()) } else { pyos::makedirs(&parent, true) };
        if let Err(e) = r.and_then(|_| pyos::shutil_move(&src_path, &dst_path)) {
            return self.result(e, false);
        }
        self.result(format!("moved {} -> {}", src, dst), true)
    }

    fn route_delete(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let rel_path = sget(b, "path");
        if sandbox.is_empty() || rel_path.is_empty() {
            return self.result("sandbox and path are both required", false);
        }
        let target = match self.get_sandbox_subpath(&sandbox, &rel_path) {
            Ok(t) => t,
            Err(e) => return self.result(e, false),
        };
        let r = if pyos::isdir(&target) {
            pyos::rmtree(&target)
        } else {
            std::fs::remove_file(&target).map_err(|e| pyos::os_err(&e, &target))
        };
        if let Err(e) = r {
            return self.result(e, false);
        }
        self.result(format!("deleted {}", rel_path), true)
    }

    fn route_duplicate(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let rel_path = sget(b, "path");
        if sandbox.is_empty() || rel_path.is_empty() {
            return self.result("sandbox and path are both required", false);
        }
        let src = match self.get_sandbox_subpath(&sandbox, &rel_path) {
            Ok(t) => t,
            Err(e) => return self.result(e, false),
        };
        if !pyos::exists(&src) {
            return self.result(format!("not found: {}", rel_path), false);
        }
        let parent_rel = pyos::dirname(&rel_path);
        let base = pyos::basename(&rel_path).to_string();
        let (stem, ext) = if pyos::isdir(&src) { (base.clone(), String::new()) } else { pyos::splitext(&base) };
        let mut n = 1;
        let (new_name, candidate) = loop {
            let suffix = if n == 1 { " copy".to_string() } else { format!(" copy {}", n) };
            let new_name = format!("{}{}{}", stem, suffix, ext);
            let candidate = pyos::join(&pyos::dirname(&src), &new_name);
            if !pyos::exists(&candidate) {
                break (new_name, candidate);
            }
            n += 1;
        };
        let r = if pyos::isdir(&src) { pyos::copytree(&src, &candidate) } else { pyos::copy2(&src, &candidate) };
        if let Err(e) = r {
            return self.result(e, false);
        }
        let new_rel = if parent_rel.is_empty() { new_name } else { format!("{}/{}", parent_rel, new_name) };
        self.result(format!("duplicated to {}", new_rel), true)
    }

    fn route_search(&self, b: &Map<String, Value>) -> anyhow::Result<Value> {
        let sandbox = sget(b, "sandbox");
        let needle = sget(b, "query");
        let use_regex = present(b, "regex");
        let mm = b.get("max_matches").filter(|v| truthy(v));
        let max_matches = py_int(mm, 200)?.min(500);
        if sandbox.is_empty() || needle.is_empty() {
            return Ok(self.result("sandbox and query are both required", false));
        }
        let root = match self.get_full_sandbox_path(&sandbox) {
            Ok(r) => r,
            Err(e) => return Ok(self.result(e, false)),
        };
        let pattern = if use_regex {
            match compile_regex(&needle, true) {
                Ok(p) => Some(p),
                Err(e) => return Ok(self.result(format!("invalid regex: {}", e), false)),
            }
        } else {
            None
        };
        // python: with no sandbox_paths the root is None and os.walk(None) raises outside any
        // try, so the exception escapes the route instead of becoming a result
        let Some(root) = root else {
            return Err(err("expected str, bytes or os.PathLike object, not NoneType"));
        };
        let needle_lower = needle.to_lowercase();
        let blacklist = self.blacklist_or_empty();
        let mut results: Vec<Value> = Vec::new();
        let mut truncated = false;
        // python: the os.walk loop has no try, so a raise from the blacklist test escapes the route
        let mut walk_err: Option<anyhow::Error> = None;
        pyos::walk::<()>(&root, &mut |dirpath, dirnames, filenames| {
            if let Err(e) = prune_dirs(dirnames, &blacklist) {
                walk_err = Some(e);
                return ControlFlow::Break(());
            }
            for fname in filenames {
                if fname.starts_with('.') {
                    continue;
                }
                let fpath = pyos::join(dirpath, fname);
                match std::fs::metadata(&fpath) {
                    Ok(m) if m.len() > UI_MAX_READ_BYTES => continue,
                    Ok(_) => {}
                    Err(_) => continue,
                }
                // python route streams the file (`for line in f`): lines completed before a bad utf-8 chunk still count
                let Ok((lines, _decode_err)) = pyos::stream_lines(&fpath) else { continue };
                // python only breaks the line loop here: the file loop has no truncated
                // check, so later files can still append (and the walk stops per folder)
                for (i, line) in lines.iter().enumerate() {
                    let hit = match &pattern {
                        Some(p) => match re_search(p, line) {
                            Ok(h) => h,
                            Err(_) => continue,
                        },
                        None => line.to_lowercase().contains(&needle_lower),
                    };
                    if hit {
                        let text: String = pyos::rstrip(line).chars().take(300).collect();
                        results.push(json!({"file": pyos::relpath(&fpath, &root), "line": i + 1, "text": text}));
                        if results.len() as i64 >= max_matches {
                            truncated = true;
                            break;
                        }
                    }
                }
            }
            if truncated {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        if let Some(e) = walk_err {
            return Err(e);
        }
        Ok(json!({"results": results, "truncated": truncated}))
    }

    // ---------------- git

    /// python `_git_root`: the folder git runs in, `None` when no sandbox_paths are configured
    /// (python's `_get_full_sandbox_path` returns None and git then runs in the server's cwd)
    fn git_root(&self, sandbox: &str, subpath: &str) -> R<Option<String>> {
        let full = self.get_full_sandbox_path(sandbox)?;
        if subpath.is_empty() {
            return Ok(full);
        }
        // python: core.sandbox_path(None, subpath) fails on `None.replace`
        let base = full.ok_or_else(|| "'NoneType' object has no attribute 'replace'".to_string())?;
        ol_core::paths::sandbox_path(&base, Some(subpath)).map(Some).map_err(|e| e.to_string())
    }

    /// the root as a path for `os.path` calls: python raises TypeError on a None root
    fn root_str(root: &Option<String>) -> R<&str> {
        root.as_deref().ok_or_else(|| NONE_ROOT_ERR.to_string())
    }

    /// python `_check_git_paths`: `os.path.dirname(root)` runs before the loop, so a None root
    /// raises even when there are no paths
    fn check_git_paths(root: Option<&str>, paths: &[String]) -> R<()> {
        let root = root.ok_or_else(|| NONE_ROOT_ERR.to_string())?;
        let sandbox_root = pyos::dirname(root);
        for p in paths {
            let full = pyos::normpath(&pyos::join(root, p));
            if full != sandbox_root && !full.starts_with(&format!("{}/", sandbox_root)) {
                return Err(format!("path escapes the sandbox: {}", p));
            }
        }
        Ok(())
    }

    fn route_git_repos(&self, q: &Map<String, Value>) -> anyhow::Result<Value> {
        let sandbox = sget(q, "sandbox");
        if sandbox.is_empty() {
            return Ok(self.result("no sandbox given", false));
        }
        let root = match self.get_full_sandbox_path(&sandbox) {
            Ok(r) => r,
            Err(e) => return Ok(self.result(e, false)),
        };
        // python: with no sandbox_paths the root is None and os.path.join(None, '.git') raises
        // outside any try, so the exception escapes the route
        let Some(root) = root else {
            return Err(err(NONE_ROOT_ERR));
        };
        let blacklist = self.blacklist_or_empty();
        let max_depth = 6usize;
        let max_repos = 50usize;
        let is_repo = |p: &str| {
            let g = pyos::join(p, ".git");
            pyos::isdir(&g) || pyos::isfile(&g)
        };
        let mut repos: Vec<String> = Vec::new();
        if is_repo(&root) {
            repos.push(String::new());
        }
        // python: the os.walk loop has no try, so a raise from the blacklist test escapes the route
        let mut walk_err: Option<anyhow::Error> = None;
        pyos::walk::<()>(&root, &mut |dirpath, dirnames, _| {
            let rel = pyos::relpath(dirpath, &root);
            let depth = if rel == "." { 0 } else { rel.split('/').count() };
            if let Err(e) = prune_dirs(dirnames, &blacklist) {
                walk_err = Some(e);
                return ControlFlow::Break(());
            }
            // python `depth + 1 <= max_depth` is the same for every child of this folder
            if depth + 1 > max_depth {
                dirnames.clear();
            }
            if depth == 0 || depth > max_depth {
                return ControlFlow::Continue(());
            }
            if is_repo(dirpath) {
                repos.push(rel);
                dirnames.clear();
                if repos.len() >= max_repos {
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        });
        if let Some(e) = walk_err {
            return Err(e);
        }
        Ok(json!({"repos": repos}))
    }

    async fn route_git_status(&self, q: &Map<String, Value>) -> Value {
        let sandbox = sget(q, "sandbox");
        if sandbox.is_empty() {
            return self.result("no sandbox given", false);
        }
        let root = match self.git_root(&sandbox, &sget(q, "subpath")) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let (ok, _) = git(root.as_deref(), &["rev-parse", "--is-inside-work-tree"]).await;
        if !ok {
            return json!({"is_repo": false, "read_only": self.read_only()});
        }
        let (_, branch) = git(root.as_deref(), &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        let (ok_b, b_out) = git(root.as_deref(), &["branch", "--format=%(refname:short)"]).await;
        let branches: Vec<String> = if ok_b { b_out.split('\n').map(|b| b.trim()).filter(|b| !b.is_empty()).map(String::from).collect() } else { Vec::new() };
        let (rc, raw) = run_git_bytes(root.as_deref(), &s(&["status", "--porcelain=v1", "-z"]), 15).await;
        let changes = if rc == 0 { parse_porcelain_z(&raw) } else { Vec::new() };
        json!({"is_repo": true, "branch": branch, "branches": branches, "changes": changes, "read_only": self.read_only()})
    }

    async fn route_git_recent(&self, q: &Map<String, Value>) -> anyhow::Result<Value> {
        let sandbox = sget(q, "sandbox");
        let limit = py_int(q.get("n"), 5)?.min(20);
        if sandbox.is_empty() {
            return Ok(self.result("no sandbox given", false));
        }
        let root = match self.git_root(&sandbox, &sget(q, "subpath")) {
            Ok(r) => r,
            Err(e) => return Ok(self.result(e, false)),
        };
        let (ok, _) = git(root.as_deref(), &["rev-parse", "--is-inside-work-tree"]).await;
        if !ok {
            return Ok(json!({"is_repo": false}));
        }
        let (_, branch) = git(root.as_deref(), &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        let (rc, raw) = run_git_bytes(root.as_deref(), &[String::from("log"), "--pretty=%h%x09%s".into(), format!("-n{}", limit)], 15).await;
        if rc != 0 {
            return Ok(json!({"is_repo": true, "branch": branch, "log": []}));
        }
        Ok(json!({"is_repo": true, "branch": branch, "log": parse_log(&raw)}))
    }

    /// python `for p in paths` over the request's truthy `paths` value: a list (elements must be
    /// str, else os.path.join raises), a string (its characters) or a dict (its keys). any other
    /// value raises when iteration starts. each item is an `Err` when python would raise on it.
    fn py_path_items(paths: &Value) -> R<Vec<R<String>>> {
        let not_pathlike = |v: &Value| -> R<String> {
            Err(format!("join() argument must be str, bytes, or os.PathLike object, not '{}'", py_type(v)))
        };
        Ok(match paths {
            Value::Array(a) => a
                .iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s.clone()),
                    other => not_pathlike(other),
                })
                .collect(),
            Value::String(s) => s.chars().map(|c| Ok(c.to_string())).collect(),
            Value::Object(m) => m.keys().map(|k| Ok(k.clone())).collect(),
            other => return Err(format!("'{}' object is not iterable", py_type(other))),
        })
    }

    /// python `_check_git_paths` + `abs_paths` over the request's `paths`, checked in order:
    /// the first bad item stops with python's error text
    fn checked_git_paths(root: Option<&str>, paths: &Value) -> R<Vec<String>> {
        // python's dirname(root) inside _check_git_paths runs before the paths are iterated
        if root.is_none() {
            return Err(NONE_ROOT_ERR.to_string());
        }
        let mut out = Vec::new();
        for item in Self::py_path_items(paths)? {
            let p = item?;
            Self::check_git_paths(root, std::slice::from_ref(&p))?;
            out.push(p);
        }
        Ok(out)
    }

    async fn route_git_stage(&self, b: &Map<String, Value>, unstage: bool) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        if sandbox.is_empty() || !present(b, "paths") {
            return self.result("sandbox and paths are both required", false);
        }
        let paths_val = b.get("paths").cloned().unwrap_or(Value::Null);
        let root = match self.git_root(&sandbox, &sget(b, "subpath")) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let paths = match Self::checked_git_paths(root.as_deref(), &paths_val) {
            Ok(p) => p,
            Err(e) => return self.result(e, false),
        };
        let abs: Vec<String> = match Self::root_str(&root) {
            Ok(r) => paths.iter().map(|p| pyos::normpath(&pyos::join(r, p))).collect(),
            Err(e) => return self.result(e, false),
        };
        let mut args: Vec<&str> = if unstage { vec!["restore", "--staged", "--"] } else { vec!["add", "--"] };
        args.extend(abs.iter().map(|s| s.as_str()));
        let (ok, out) = git(root.as_deref(), &args).await;
        if !ok {
            return self.result(out, false);
        }
        self.result(if unstage { "unstaged" } else { "staged" }, true)
    }

    async fn route_git_commit(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let message = sget(b, "message").trim().to_string();
        if sandbox.is_empty() || message.is_empty() {
            return self.result("sandbox and message are both required", false);
        }
        let root = match self.git_root(&sandbox, &sget(b, "subpath")) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let (ok, out) = git(root.as_deref(), &["commit", "-m", &message]).await;
        self.result(out, ok)
    }

    async fn route_git_branch(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let name = sget(b, "name").trim().to_string();
        if sandbox.is_empty() || name.is_empty() {
            return self.result("sandbox and name are both required", false);
        }
        let root = match self.git_root(&sandbox, &sget(b, "subpath")) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let (ok, out) = git(root.as_deref(), &["switch", &name]).await;
        if !ok {
            return self.result(out, false);
        }
        self.result(if out.is_empty() { format!("switched to {}", name) } else { out }, true)
    }

    async fn pick_remote(root: Option<&str>) -> Option<String> {
        let (ok, out) = git(root, &["remote"]).await;
        let remotes: Vec<String> = if ok { out.split_whitespace().map(String::from).collect() } else { Vec::new() };
        if remotes.is_empty() {
            return None;
        }
        Some(if remotes.iter().any(|r| r == "origin") { "origin".into() } else { remotes[0].clone() })
    }

    async fn route_git_ahead(&self, q: &Map<String, Value>) -> Value {
        let sandbox = sget(q, "sandbox");
        if sandbox.is_empty() {
            return self.result("no sandbox given", false);
        }
        let root = match self.git_root(&sandbox, &sget(q, "subpath")) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let (ok, branch) = git(root.as_deref(), &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        if !ok {
            return self.result(branch, false);
        }
        if branch == "HEAD" {
            return self.result("detached HEAD: switch to a branch before pushing", false);
        }
        let Some(remote) = Self::pick_remote(root.as_deref()).await else {
            return self.result("this repo has no remotes, nowhere to push to", false);
        };
        let (ok, upstream) = git(root.as_deref(), &["rev-parse", "--abbrev-ref", "@{u}"]).await;
        let upstream = if ok { upstream.trim().to_string() } else { String::new() };
        let has_upstream = upstream.contains('/');
        let (target, (rc, raw)) = if has_upstream {
            (upstream.clone(), run_git_bytes(root.as_deref(), &[String::from("log"), "--pretty=%h%x09%s".into(), format!("{}..HEAD", upstream), "-n100".into()], 15).await)
        } else {
            (format!("{}/{}", remote, branch), run_git_bytes(root.as_deref(), &s(&["log", "--pretty=%h%x09%s", "HEAD", "--not", "--remotes", "-n100"]), 15).await)
        };
        if rc != 0 {
            let t = String::from_utf8_lossy(&raw).trim().to_string();
            return self.result(if t.is_empty() { "git log failed".into() } else { t }, false);
        }
        let ahead = parse_log(&raw);
        let mut behind = 0i64;
        if has_upstream {
            let (ok, out) = git(root.as_deref(), &["rev-list", "--count", &format!("HEAD..{}", upstream)]).await;
            let o = out.trim();
            if ok && !o.is_empty() && o.chars().all(|c| c.is_ascii_digit()) {
                behind = o.parse().unwrap_or(0);
            }
        }
        let (_, raw) = run_git_bytes(root.as_deref(), &s(&["status", "--porcelain=v1"]), 15).await;
        let dirty = !String::from_utf8_lossy(&raw).trim().is_empty();
        json!({
            "branch": branch,
            "remote": remote,
            "upstream": upstream,
            "has_upstream": has_upstream,
            "target": target,
            "ahead": ahead,
            "behind": behind,
            "dirty": dirty,
        })
    }

    async fn route_git_push(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        if sandbox.is_empty() {
            return self.result("no sandbox given", false);
        }
        let root = match self.git_root(&sandbox, &sget(b, "subpath")) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let (ok, branch) = git(root.as_deref(), &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        if !ok || branch == "HEAD" {
            return self.result("detached HEAD: switch to a branch before pushing", false);
        }
        let Some(remote) = Self::pick_remote(root.as_deref()).await else {
            return self.result("this repo has no remotes, nowhere to push to", false);
        };
        let (ok, upstream) = git(root.as_deref(), &["rev-parse", "--abbrev-ref", "@{u}"]).await;
        let upstream = if ok { upstream.trim().to_string() } else { String::new() };
        let args: Vec<String> = if let Some((up_remote, up_branch)) = upstream.split_once('/') {
            vec!["push".into(), up_remote.into(), format!("{}:{}", branch, up_branch)]
        } else {
            vec!["push".into(), "-u".into(), remote, branch]
        };
        let (rc, raw) = run_git_bytes(root.as_deref(), &args, 180).await;
        let out = String::from_utf8_lossy(&raw).trim().to_string();
        if rc != 0 {
            return self.result(if out.is_empty() { "git push failed".into() } else { out }, false);
        }
        self.result(if out.is_empty() { "pushed".into() } else { out }, true)
    }

    async fn route_git_log(&self, q: &Map<String, Value>) -> anyhow::Result<Value> {
        let sandbox = sget(q, "sandbox");
        let limit = py_int(q.get("n"), 20)?.min(100);
        if sandbox.is_empty() {
            return Ok(self.result("no sandbox given", false));
        }
        let root = match self.git_root(&sandbox, &sget(q, "subpath")) {
            Ok(r) => r,
            Err(e) => return Ok(self.result(e, false)),
        };
        let (rc, raw) = run_git_bytes(root.as_deref(), &[String::from("log"), "--pretty=%h%x09%s".into(), format!("-n{}", limit)], 15).await;
        if rc != 0 {
            let t = String::from_utf8_lossy(&raw).trim().to_string();
            return Ok(self.result(if t.is_empty() { "git log failed".into() } else { t }, false));
        }
        Ok(json!({"log": parse_log(&raw)}))
    }

    /// shared argument handling of git_diff / git_split_diff
    async fn diff_raw(&self, q: &Map<String, Value>) -> Result<(i32, Vec<u8>, bool), Value> {
        let sandbox = sget(q, "sandbox");
        let rel_path = sget(q, "path");
        let staged = matches!(sget(q, "staged").as_str(), "1" | "true");
        let untracked = matches!(sget(q, "untracked").as_str(), "1" | "true");
        if sandbox.is_empty() || rel_path.is_empty() {
            return Err(self.result("sandbox and path are both required", false));
        }
        let root = match self.git_root(&sandbox, &sget(q, "subpath")).and_then(|r| Self::check_git_paths(r.as_deref(), std::slice::from_ref(&rel_path)).map(|_| r)) {
            Ok(r) => r,
            Err(e) => return Err(self.result(e, false)),
        };
        let abs = match Self::root_str(&root) {
            Ok(r) => pyos::normpath(&pyos::join(r, &rel_path)),
            Err(e) => return Err(self.result(e, false)),
        };
        let (rc, raw) = if untracked {
            run_git_bytes(root.as_deref(), &[String::from("diff"), "--no-index".into(), "--".into(), "/dev/null".into(), abs], 15).await
        } else if staged {
            run_git_bytes(root.as_deref(), &[String::from("diff"), "--staged".into(), "--".into(), abs], 15).await
        } else {
            run_git_bytes(root.as_deref(), &[String::from("diff"), "--".into(), abs], 15).await
        };
        Ok((rc, raw, untracked))
    }

    async fn route_git_diff(&self, q: &Map<String, Value>) -> Value {
        let (rc, raw, untracked) = match self.diff_raw(q).await {
            Ok(r) => r,
            Err(v) => return v,
        };
        let ok = if untracked { rc == 0 || rc == 1 } else { rc == 0 };
        if !ok {
            let t = String::from_utf8_lossy(&raw).trim().to_string();
            return self.result(if t.is_empty() { format!("git diff failed (code {})", rc) } else { t }, false);
        }
        let text = String::from_utf8_lossy(&raw).to_string();
        json!({"diff": if text.trim().is_empty() { "(no changes)".to_string() } else { text }})
    }

    async fn route_git_split_diff(&self, q: &Map<String, Value>) -> Value {
        let (rc, raw, _) = match self.diff_raw(q).await {
            Ok(r) => r,
            Err(v) => return v,
        };
        if rc != 0 && rc != 1 {
            let t = String::from_utf8_lossy(&raw).trim().to_string();
            return self.result(if t.is_empty() { "git diff failed".into() } else { t }, false);
        }
        json!({"rows": parse_split_diff(&String::from_utf8_lossy(&raw))})
    }

    async fn route_git_fetch(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        if sandbox.is_empty() {
            return self.result("no sandbox given", false);
        }
        let root = match self.git_root(&sandbox, &sget(b, "subpath")) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let (ok, branch) = git(root.as_deref(), &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        if !ok {
            return self.result(branch, false);
        }
        let Some(remote) = Self::pick_remote(root.as_deref()).await else {
            return self.result("this repo has no remotes", false);
        };
        let (rc, raw) = run_git_bytes(root.as_deref(), &[String::from("fetch"), remote, "--prune".into()], 120).await;
        let out = String::from_utf8_lossy(&raw).trim().to_string();
        if rc != 0 {
            return self.result(if out.is_empty() { "git fetch failed".into() } else { out }, false);
        }
        let (ok, upstream) = git(root.as_deref(), &["rev-parse", "--abbrev-ref", "@{u}"]).await;
        let mut incoming = Vec::new();
        if ok && upstream.trim().contains('/') {
            let (rc2, raw2) = run_git_bytes(root.as_deref(), &[String::from("log"), "--pretty=%h%x09%s".into(), format!("HEAD..{}", upstream.trim()), "-n100".into()], 15).await;
            if rc2 == 0 {
                incoming = parse_log(&raw2);
            }
        }
        json!({"output": out, "incoming": incoming, "branch": branch})
    }

    async fn route_git_pull(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        if sandbox.is_empty() {
            return self.result("no sandbox given", false);
        }
        let root = match self.git_root(&sandbox, &sget(b, "subpath")) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let (ok, branch) = git(root.as_deref(), &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        if !ok || branch == "HEAD" {
            return self.result("detached HEAD: switch to a branch before pulling", false);
        }
        let (rc, raw) = run_git_bytes(root.as_deref(), &s(&["pull"]), 120).await;
        let out = String::from_utf8_lossy(&raw).trim().to_string();
        if rc != 0 {
            let (_, craw) = run_git_bytes(root.as_deref(), &s(&["diff", "--name-only", "--diff-filter=U"]), 15).await;
            let craw = String::from_utf8_lossy(&craw).to_string();
            let conflicts: Vec<&str> = py_splitlines(&craw).into_iter().filter(|l| !l.trim().is_empty()).collect();
            if !conflicts.is_empty() {
                return json!({"conflicts": conflicts, "output": out, "pull_failed": true});
            }
            return self.result(if out.is_empty() { "git pull failed".into() } else { out }, false);
        }
        self.result(if out.is_empty() { "pulled".into() } else { out }, true)
    }

    async fn route_git_branch_create(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let name = sget(b, "name").trim().to_string();
        let checkout = b.get("checkout").map(truthy).unwrap_or(true);
        if sandbox.is_empty() || name.is_empty() {
            return self.result("sandbox and name are both required", false);
        }
        let valid = regex::Regex::new(r"^[a-zA-Z0-9._/\-]+\n?$").unwrap();
        if !valid.is_match(&name) {
            return self.result("invalid branch name: use letters, digits, dots, slashes, hyphens, underscores", false);
        }
        let root = match self.git_root(&sandbox, &sget(b, "subpath")) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let (ok, out) = if checkout { git(root.as_deref(), &["switch", "-c", &name]).await } else { git(root.as_deref(), &["branch", &name]).await };
        if !ok {
            return self.result(out, false);
        }
        self.result(if out.is_empty() { format!("created branch {}", name) } else { out }, true)
    }

    async fn route_git_branch_delete(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let name = sget(b, "name").trim().to_string();
        let force = b.get("force").map(truthy).unwrap_or(false);
        if sandbox.is_empty() || name.is_empty() {
            return self.result("sandbox and name are both required", false);
        }
        let root = match self.git_root(&sandbox, &sget(b, "subpath")) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let (_, current) = git(root.as_deref(), &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        if current.trim() == name {
            return self.result("cannot delete the branch you are currently on", false);
        }
        let (ok, out) = git(root.as_deref(), &["branch", if force { "-D" } else { "-d" }, &name]).await;
        if !ok {
            return self.result(out, false);
        }
        self.result(if out.is_empty() { format!("deleted branch {}", name) } else { out }, true)
    }

    async fn route_git_conflicts(&self, q: &Map<String, Value>) -> Value {
        let sandbox = sget(q, "sandbox");
        if sandbox.is_empty() {
            return self.result("no sandbox given", false);
        }
        let root = match self.git_root(&sandbox, &sget(q, "subpath")) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let (rc, raw) = run_git_bytes(root.as_deref(), &s(&["diff", "--name-only", "--diff-filter=U"]), 15).await;
        if rc != 0 {
            return json!({"conflicts": []});
        }
        let text = String::from_utf8_lossy(&raw).to_string();
        let conflicts: Vec<&str> = py_splitlines(&text).into_iter().filter(|l| !l.trim().is_empty()).map(|l| l.trim()).collect();
        json!({"conflicts": conflicts})
    }

    async fn route_git_resolve(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let rel_path = sget(b, "path");
        let strategy = if b.contains_key("strategy") { sget(b, "strategy") } else { "ours".into() };
        if sandbox.is_empty() || rel_path.is_empty() {
            return self.result("sandbox and path are both required", false);
        }
        if !matches!(strategy.as_str(), "ours" | "theirs" | "both") {
            return self.result("strategy must be 'ours', 'theirs', or 'both'", false);
        }
        let root = match self.git_root(&sandbox, &sget(b, "subpath")).and_then(|r| Self::check_git_paths(r.as_deref(), std::slice::from_ref(&rel_path)).map(|_| r)) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let abs = match Self::root_str(&root) {
            Ok(r) => pyos::normpath(&pyos::join(r, &rel_path)),
            Err(e) => return self.result(e, false),
        };
        let (ok, out) = match strategy.as_str() {
            "ours" => git(root.as_deref(), &["checkout", "--ours", "--", &rel_path]).await,
            "theirs" => git(root.as_deref(), &["checkout", "--theirs", "--", &rel_path]).await,
            _ => {
                let re = regex::Regex::new(r"(?s)<<<<<<< [^\n]*\n(.*?)=======\n(.*?)>>>>>>> [^\n]*\n").unwrap();
                match pyos::read_text(&abs) {
                    Ok(content) => {
                        let resolved = re.replace_all(&content, "${1}${2}").to_string();
                        match pyos::write_file(&abs, resolved.as_bytes()) {
                            Ok(()) => (true, "resolved (both)".to_string()),
                            Err(e) => (false, e),
                        }
                    }
                    Err(e) => (false, e),
                }
            }
        };
        if !ok {
            return self.result(out, false);
        }
        git(root.as_deref(), &["add", "--", &abs]).await;
        self.result(format!("resolved {} ({})", rel_path, strategy), true)
    }

    async fn route_git_revert_file(&self, b: &Map<String, Value>) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let rel_path = sget(b, "path");
        if sandbox.is_empty() || rel_path.is_empty() {
            return self.result("sandbox and path are both required", false);
        }
        let root = match self.git_root(&sandbox, &sget(b, "subpath")).and_then(|r| Self::check_git_paths(r.as_deref(), std::slice::from_ref(&rel_path)).map(|_| r)) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let abs = match Self::root_str(&root) {
            Ok(r) => pyos::normpath(&pyos::join(r, &rel_path)),
            Err(e) => return self.result(e, false),
        };
        let (ok, out) = git(root.as_deref(), &["restore", "--source=HEAD", "--staged", "--worktree", "--", &abs]).await;
        if !ok {
            return self.result(out, false);
        }
        self.result(format!("reverted {} to HEAD", rel_path), true)
    }

    fn route_upload(&self, b: &Map<String, Value>) -> anyhow::Result<Value> {
        if self.read_only() {
            return Ok(self.result("coder is in read-only mode", false));
        }
        // python tests the raw values, so 0, false, [] and {} are as missing as ""
        let raw_of = |k: &str| b.get(k).cloned().unwrap_or_else(|| json!(""));
        let (sandbox_v, path_v, filename_v) = (raw_of("sandbox"), raw_of("path"), raw_of("filename"));
        if !truthy(&sandbox_v) || !truthy(&path_v) || !truthy(&filename_v) {
            return Ok(self.result("sandbox, path and filename are all required", false));
        }
        // python: regex.sub(r"[/\\]", "_", filename) raises outside the try
        let Value::String(filename) = &filename_v else {
            anyhow::bail!("a bytes-like object is required, not '{}'", py_type(&filename_v));
        };
        let safe_name = filename.replace(['/', '\\'], "_");
        // python: rel_path.rstrip("/") raises outside the try
        let Value::String(rel_path) = &path_v else {
            anyhow::bail!("'{}' object has no attribute 'rstrip'", py_type(&path_v));
        };
        let target_rel = format!("{}/{}", rel_path.trim_end_matches('/'), safe_name);
        // python: _get_sandbox_subpath runs inside the try, so its errors become the result text
        let Value::String(sandbox) = &sandbox_v else {
            return Ok(self.result(format!("startswith first arg must be str or a tuple of str, not {}", py_type(&sandbox_v)), false));
        };
        let target = match self.get_sandbox_subpath(sandbox, &target_rel) {
            Ok(t) => t,
            Err(e) => return Ok(self.result(e, false)),
        };
        if pyos::exists(&target) {
            return Ok(self.result(format!("file already exists: {}", target_rel), false));
        }
        // python: body.get("content", ""); b64decode raises for any non-str value (None here)
        let content_b64: Option<String> = match b.get("content") {
            None => Some(String::new()),
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => None,
        };
        let raw = match content_b64.as_deref().and_then(py_b64decode) {
            Some(r) => r,
            None => return Ok(self.result("invalid base64 content", false)),
        };
        if raw.len() > 10_000_000 {
            return Ok(self.result("file too large (max 10MB)", false));
        }
        let parent = pyos::dirname(&target);
        let r = if parent.is_empty() { Ok(()) } else { pyos::makedirs(&parent, true) };
        if let Err(e) = r.and_then(|_| pyos::write_file(&target, &raw)) {
            return Ok(self.result(e, false));
        }
        Ok(self.result(format!("uploaded {}", target_rel), true))
    }
}

#[async_trait::async_trait]
impl Module for Coder {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    fn disabled_tools(&self) -> Vec<String> {
        self.disabled.lock().clone()
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "list_sandboxes" => Ok(self.result(self.get_sandbox_paths().map_err(err)?, true)),
            "glob" => self.tool_glob(&args).await,
            "folder_grep" => self.tool_folder_grep(&args).await,
            "file_create" => self.tool_file_create(&args).await,
            "file_move" => self.tool_file_move(&args).await,
            "file_delete" => self.tool_file_delete(&args).await,
            "folder_delete" => self.tool_folder_delete(&args).await,
            "file_outline" => self.tool_file_outline(&args).await,
            "file_read" => self.tool_file_read(&args).await,
            "file_grep" => self.tool_file_grep(&args).await,
            "read_template" => self.tool_read_template(&args).await,
            "file_edit" => self.tool_file_edit(&args).await,
            _ => Err(anyhow!("'Coder' object has no attribute '{}'", method)),
        }
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        let mut d = Vec::new();
        if self.read_only() {
            d.extend(["file_create", "file_move", "file_delete", "file_edit", "folder_delete"].map(String::from));
        }
        if self.cfg_bool("insert_sandbox_paths_into_system_prompt") {
            d.push("list_sandboxes".into());
        }
        d.push("file_outline".into());
        *self.disabled.lock() = d;
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        let mut out: Vec<String> = Vec::new();
        let templates = self.get_templates().map_err(err)?;
        if !templates.is_empty() {
            let list: Vec<String> = templates.iter().map(|t| format!("- {}", t)).collect();
            out.push(format!("## Templates you can read using read_template:\n{}", list.join("\n")));
        }
        if self.cfg_bool("insert_sandbox_paths_into_system_prompt") {
            let sandboxes = self.get_sandbox_paths().map_err(err)?;
            if !sandboxes.is_empty() {
                let list: Vec<String> = sandboxes.iter().map(|p| format!("- {}", p)).collect();
                out.push(format!("## Sandboxes you have access to:\n{}", list.join("\n")));
            }
            if self.cfg_bool("add_sandbox_contents_to_sandbox_list") {
                for sandbox in &sandboxes {
                    let mut s = format!("### Files within sandbox '{}':", sandbox);
                    let full = self.full_sandbox(sandbox).map_err(err)?;
                    let listing = pyos::listdir(&full).map_err(err)?;
                    let mut files = Vec::new();
                    let mut folders = Vec::new();
                    for f in listing {
                        if pyos::isdir(&pyos::join(&full, &f)) {
                            folders.push(f);
                        } else {
                            files.push(f);
                        }
                    }
                    folders.sort();
                    files.sort();
                    if !folders.is_empty() {
                        s.push_str("\n#### Folders:\n");
                        s.push_str(&folders.iter().map(|f| format!("- {}", f)).collect::<Vec<_>>().join("\n"));
                    }
                    if !files.is_empty() {
                        s.push_str("\n#### Files:\n");
                        s.push_str(&files.iter().map(|f| format!("- {}", f)).collect::<Vec<_>>().join("\n"));
                    }
                    out.push(s);
                }
            }
        }
        if self.cfg_bool("use_coding_prompt") && self.cfg_bool("coding_prompt") {
            let prompt = self.cfg("coding_prompt").map(|v| py_str(&v)).unwrap_or_default();
            out.push(format!("## Coding Guidelines\nYOU MUST ALWAYS follow these guidelines while using the coder:\n{}", prompt));
        }
        if self.read_only() {
            out.push("## IMPORTANT: Read-Only Mode\nYour coder module is in read-only mode and you cannot write to files. Provide output code to the user directly in your messages.".into());
        }
        Ok(if out.is_empty() { None } else { Some(out.join("\n\n")) })
    }

    fn commands(&self) -> Vec<CommandSpec> {
        meta::commands().into_iter().map(|(c, _)| c).collect()
    }

    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        if name != "coder" {
            anyhow::bail!("unknown command {}", name);
        }
        if args.is_empty() {
            return Ok(Some("please specify a sub-command (see /help coder)".into()));
        }
        match args[0].as_str() {
            "readonly" => {
                let current = self.read_only();
                self.base.config.set("read-only", json!(!current));
                if let Some(m) = self.base.manager() {
                    m.reload_module(&self.base.name).await;
                }
                Ok(Some(if current { "read-only mode disabled" } else { "read-only mode enabled" }.into()))
            }
            _ => Ok(None),
        }
    }

    fn ui_routes(&self) -> Vec<UiRoute> {
        meta::ui_routes().into_iter().map(|(r, _)| r).collect()
    }

    async fn ui_route(&self, path: &str, _method: &str, body: Option<Value>, query: Map<String, Value>) -> anyhow::Result<Value> {
        let b = body_dict(path, body, self.read_only())?;
        let q = &query;
        Ok(match path {
            "sandboxes" => self.route_sandboxes()?,
            "tree" => self.route_tree(q),
            "files" => self.route_files(q),
            "read" => self.route_read(q),
            "save" => self.route_save(&b),
            "lint" => self.route_lint(&b),
            "create" => self.route_create(&b),
            "move" => self.route_move(&b),
            "delete" => self.route_delete(&b),
            "duplicate" => self.route_duplicate(&b),
            "search" => self.route_search(&b)?,
            "git_repos" => self.route_git_repos(q)?,
            "git_status" => self.route_git_status(q).await,
            "git_recent" => self.route_git_recent(q).await?,
            "git_stage" => self.route_git_stage(&b, false).await,
            "git_unstage" => self.route_git_stage(&b, true).await,
            "git_commit" => self.route_git_commit(&b).await,
            "git_branch" => self.route_git_branch(&b).await,
            "git_ahead" => self.route_git_ahead(q).await,
            "git_push" => self.route_git_push(&b).await,
            "git_log" => self.route_git_log(q).await?,
            "git_diff" => self.route_git_diff(q).await,
            "git_fetch" => self.route_git_fetch(&b).await,
            "git_pull" => self.route_git_pull(&b).await,
            "git_branch_create" => self.route_git_branch_create(&b).await,
            "git_branch_delete" => self.route_git_branch_delete(&b).await,
            "git_conflicts" => self.route_git_conflicts(q).await,
            "git_resolve" => self.route_git_resolve(&b).await,
            "git_revert_file" => self.route_git_revert_file(&b).await,
            "upload" => self.route_upload(&b)?,
            "git_split_diff" => self.route_git_split_diff(q).await,
            _ => anyhow::bail!("route not found: {}", path),
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain() {
        let raw = b" M a.txt\0R  new.txt\0old.txt\0?? u.txt\0";
        let e = parse_porcelain_z(raw);
        assert_eq!(e.len(), 3);
        assert_eq!(e[0], json!({"path": "a.txt", "xy": " M", "staged": false, "untracked": false}));
        assert_eq!(e[1]["path"], "old.txt");
        assert_eq!(e[1]["staged"], true);
        assert_eq!(e[2]["untracked"], true);
    }

    #[test]
    fn split_diff() {
        let d = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let rows = parse_split_diff(d);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1], json!({"old_num": 2, "old_text": "b", "new_num": 2, "new_text": "B", "type": "mod"}));
    }

    #[test]
    fn context_slicing() {
        let lines = vec!["a\n", "b\n", "c\n", "d\n"];
        let (b, a) = context_lists(&lines, 1, 1);
        // python quirk: context_lines[:1] includes the line *after* the match
        assert_eq!(b, vec![json!("b")]);
        assert_eq!(a, vec![json!("b")]);
    }

    fn call(rt: &tokio::runtime::Runtime, c: &Coder, method: &str, args: Value) -> Value {
        let map = args.as_object().cloned().unwrap_or_default();
        rt.block_on(c.call_tool(method, ToolArgs::new(map))).unwrap()
    }

    #[test]
    fn syntax_gate_matches_python_blocking_rules() {
        // valid code passes, broken code is reported with python's "Line N, Col M:" prefix
        assert!(check_syntax("x = 1\n", "a.py").is_empty());
        let errs = check_syntax("def f(:\n    return 1\n", "a.py");
        assert!(!errs.is_empty());
        assert!(errs[0].starts_with("Line 1, Col "), "{:?}", errs);
        // html and plaintext are never checked; neither is a language whose grammar is not compiled in
        assert!(check_syntax("<div>", "a.html").is_empty());
        assert!(check_syntax("notes", "a.txt").is_empty());
        assert!(check_syntax("puts 'x'", "a.rb").is_empty());
        // python's falsy-result branch: the single parse-failure error, verbatim
        assert_eq!(
            gate_errors(None),
            vec!["Failed to parse the file for syntax errors. For safety, the file was not written to disk.".to_string()]
        );
    }

    #[test]
    fn upload_base64_matches_python_b64decode() {
        // expected values are python 3.14 `base64.b64decode(s)` results (None = raises)
        let cases: [(&str, Option<&[u8]>); 14] = [
            ("aGk=", Some(b"hi")),
            ("QR==", Some(b"A")),
            ("QUJD=REVG", Some(b"ABCDEF")),
            ("QUJ=", Some(b"AB")),
            ("=QUJD", Some(b"ABC")),
            ("QQ==QQ==", Some(b"A\x04\x10")),
            ("QQ=QQ", Some(b"A\x04\x10")),
            ("QUJD\n RE VG", Some(b"ABCDEF")),
            ("QQ=", None),
            ("Q==", None),
            ("QUJ=RE", None),
            ("QUJDRA=A", None),
            ("QUJ\n", None),
            ("\u{e9}", None),
        ];
        for (input, want) in cases {
            assert_eq!(py_b64decode(input).as_deref(), want, "input {:?}", input);
        }
    }

    #[test]
    fn regex_line_match_matches_python() {
        // lines keep their '\n' in python: `\s` and `\n` can consume it, `$` matches before it
        let line = "return\n";
        assert!(re_search(&compile_regex("return\\s", false).unwrap(), line).unwrap());
        assert!(re_search(&compile_regex("return$", false).unwrap(), line).unwrap());
        assert!(re_search(&compile_regex("a\\n", false).unwrap(), "a\n").unwrap());
        assert!(!re_search(&compile_regex("return\\s", false).unwrap(), "return").unwrap());
        assert!(!re_search(&compile_regex("^$", false).unwrap(), line).unwrap());
    }

    #[test]
    fn git_path_items_match_python_iteration() {
        // python `for p in paths`: a non-iterable raises at the start of the loop
        let err = Coder::py_path_items(&json!(5)).err();
        assert_eq!(err.as_deref(), Some("'int' object is not iterable"));
        // list elements must be str for os.path.join
        let items = Coder::py_path_items(&json!(["a.txt", 1])).unwrap();
        assert_eq!(items[0].as_deref(), Ok("a.txt"));
        assert_eq!(items[1].as_ref().err().map(String::as_str), Some("join() argument must be str, bytes, or os.PathLike object, not 'int'"));
        // a dict iterates its keys, a string its characters
        let mut keys: Vec<String> = Coder::py_path_items(&json!({"x.rs": 1})).unwrap().into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(keys.pop().as_deref(), Some("x.rs"));
        let chars: Vec<String> = Coder::py_path_items(&json!("ab")).unwrap().into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(chars, vec!["a".to_string(), "b".to_string()]);
        // the escape check runs in order and reports python's message
        let root = "/x/sb/repo";
        let ok = Coder::checked_git_paths(Some(root), &json!(["a.txt"])).unwrap();
        assert_eq!(ok, vec!["a.txt".to_string()]);
        let esc = Coder::checked_git_paths(Some(root), &json!(["../../etc"])).err();
        assert_eq!(esc.as_deref(), Some("path escapes the sandbox: ../../etc"));
    }

    #[cfg(unix)]
    #[test]
    fn git_output_is_decoded_strictly_like_python_text_mode() {
        use std::os::unix::ffi::OsStrExt;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let git_in = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&root).args(args).status().unwrap();
        assert!(git_in(&["init", "-q"]).success());
        std::fs::write(dir.path().join(std::ffi::OsStr::from_bytes(b"\xff.txt")), "x").unwrap();
        assert!(git_in(&["add", "--", "."]).success());
        // `ls-files -z` prints the raw name bytes: python's text-mode decode raises, and that
        // error is what the route reports
        let (ok, out) = rt.block_on(run_git(Some(&root), &["ls-files", "-z"], &[0]));
        assert!(!ok);
        assert_eq!(out, "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte");
        // without a root, git runs in the server's own working directory (python cwd=None)
        let (ok, out) = rt.block_on(run_git(None, &["--version"], &[0]));
        assert!(ok && out.starts_with("git version"), "{out}");
    }

    #[test]
    fn none_root_raises_python_os_path_error() {
        // python: with no sandbox_paths `_check_git_paths` fails in os.path.dirname even when
        // there is nothing to check
        assert_eq!(Coder::check_git_paths(None, &[]).err().as_deref(), Some(NONE_ROOT_ERR));
        assert_eq!(Coder::checked_git_paths(None, &json!([])).err().as_deref(), Some(NONE_ROOT_ERR));
        assert_eq!(Coder::root_str(&None).err().as_deref(), Some(NONE_ROOT_ERR));
        assert_eq!(Coder::root_str(&Some("/x".into())).unwrap(), "/x");
    }

    #[test]
    fn blacklist_follows_python_in_operator() {
        // python `name in folder_blacklist`: list = element equality, str = substring, dict = keys
        let list = Blacklist::from_value(Some(json!(["venv", 5])));
        assert!(list.contains("venv").unwrap());
        assert!(!list.contains("5").unwrap());
        let text = Blacklist::from_value(Some(json!("venv")));
        assert!(text.contains("env").unwrap());
        assert!(!text.contains("node_modules").unwrap());
        let dict = Blacklist::from_value(Some(json!({"venv": true})));
        assert!(dict.contains("venv").unwrap());
        assert!(!dict.contains("ven").unwrap());
        // `(config.get(..) or [])`: falsy values are an empty list, so nothing matches
        for v in [json!(""), json!(0), json!(false), json!([]), json!({}), Value::Null] {
            assert!(!Blacklist::from_value_or_empty(Some(v)).contains("anything").unwrap());
        }
        assert!(!Blacklist::from_value_or_empty(None).contains("anything").unwrap());
        // a bare `in` (glob, folder_grep) keeps falsy values: "" is a substring test, and
        // None/0/false raise TypeError like python
        assert!(!Blacklist::from_value(Some(json!(""))).contains("anything").unwrap());
        assert!(Blacklist::from_value(Some(json!(""))).contains("").unwrap());
        assert!(!Blacklist::from_value(Some(json!([]))).contains("anything").unwrap());
    }

    #[test]
    fn blacklist_non_container_raises_like_python() {
        // python 3.14: `"x" in 5` -> TypeError "argument of type 'int' is not a container or iterable"
        let cases = [
            (json!(5), "int"),
            (json!(0), "int"),
            (json!(1.5), "float"),
            (json!(0.0), "float"),
            (json!(true), "bool"),
            (json!(false), "bool"),
        ];
        for (v, ty) in cases {
            let expected = format!("argument of type '{ty}' is not a container or iterable");
            let bare = Blacklist::from_value(Some(v.clone())).contains("src").unwrap_err();
            assert_eq!(bare.to_string(), expected);
            // truthy non-containers raise through `or []` too; falsy ones are empty there
            let or_empty = Blacklist::from_value_or_empty(Some(v.clone())).contains("src");
            if truthy(&v) {
                assert_eq!(or_empty.unwrap_err().to_string(), expected);
            } else {
                assert!(!or_empty.unwrap());
            }
        }
        // a bare None (missing config) raises at the first `in`, not an empty list
        let none = Blacklist::from_value(None).contains("src").unwrap_err();
        assert_eq!(none.to_string(), "argument of type 'NoneType' is not a container or iterable");
    }

    #[test]
    fn prune_dirs_only_tests_non_hidden_names() {
        // hidden names short-circuit before `in`, so they are pruned without raising
        let bad = Blacklist::from_value(Some(json!(5)));
        let mut hidden = vec![".git".to_string()];
        prune_dirs(&mut hidden, &bad).unwrap();
        assert!(hidden.is_empty(), "hidden folders are pruned without an `in` test");
        let mut visible = vec!["src".to_string()];
        assert_eq!(
            prune_dirs(&mut visible, &bad).unwrap_err().to_string(),
            "argument of type 'int' is not a container or iterable"
        );
        // with a container the blacklisted and hidden names are removed in order
        let list = Blacklist::from_value(Some(json!(["venv"])));
        let mut dirs = vec![".git".to_string(), "venv".to_string(), "src".to_string()];
        prune_dirs(&mut dirs, &list).unwrap();
        assert_eq!(dirs, vec!["src".to_string()]);
    }

    /// one test owns the process-wide config (set_for_tests), so every tool check lives here
    #[test]
    fn tool_return_strings_match_python() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sb");
        std::fs::create_dir_all(&root).unwrap();
        ol_core::config::set_for_tests(json!({"modules": {"settings": {"coder": {
            "sandbox_paths": [root.to_string_lossy()],
            "enable_builtin_templates": false,
            "template_paths": [],
            "folder_blacklist": ["venv", "__pycache__"],
            "global_line_limit": 1000,
            "read-only": false,
        }}}}));
        let c = Coder::new(ModuleBase::detached("coder"));

        // syntax gate: the file must not be written
        let v = call(&rt, &c, "file_create", json!({"sandbox": "sb", "path": "bad.py", "content": "def f(:\n    return 1\n"}));
        assert_eq!(v["status"], "error");
        assert_eq!(v["content"]["message"], "Syntax errors detected! File was not written to disk.");
        assert!(!root.join("bad.py").exists());

        let v = call(&rt, &c, "file_create", json!({"sandbox": "sb", "path": "a.py", "content": "x = 1\n# foo bar\n"}));
        assert_eq!(v["status"], "success");
        assert_eq!(v["content"], "File a.py successfully created. Syntax verification passed - no need to verify further.");

        // plain read of a small file: raw dict, no status wrapper
        let v = call(&rt, &c, "file_read", json!({"sandbox": "sb", "path": "a.py"}));
        assert_eq!(v["content"], "x = 1\n# foo bar\n");
        assert_eq!(v["truncated"], false);
        assert_eq!(v["success"], true);

        let v = call(&rt, &c, "file_grep", json!({"sandbox": "sb", "file_path": "a.py", "regex_pattern": "foo"}));
        assert_eq!(v["status"], "success");
        assert_eq!(v["content"][0]["line_num"], 2);
        assert_eq!(v["content"][0]["line"], "# foo bar");

        let v = call(&rt, &c, "folder_grep", json!({"sandbox": "sb", "sub_path": "", "regex_pattern": "zzz"}));
        assert_eq!(v["status"], "error");
        assert_eq!(v["content"], "No matches found");

        let v = call(&rt, &c, "file_edit", json!({"sandbox": "sb", "path": "a.py", "original_code": "x = 1", "replacement_code": "x = 2"}));
        assert_eq!(v["content"], "Successfully edited file a.py. Syntax verification passed - no need to verify further.");
        assert_eq!(std::fs::read_to_string(root.join("a.py")).unwrap(), "x = 2\n# foo bar\n");

        // webui route: flat search results, python shape
        let out = rt
            .block_on(c.ui_route("search", "POST", Some(json!({"sandbox": "sb", "query": "FOO"})), Map::new()))
            .unwrap();
        assert_eq!(out["truncated"], false);
        assert_eq!(out["results"][0], json!({"file": "a.py", "line": 2, "text": "# foo bar"}));

        // webui upload: body .get first, then raw truthiness, regex.sub on filename, rstrip on path
        let up = |body: Value| rt.block_on(c.ui_route("upload", "POST", Some(body), Map::new()));
        assert_eq!(up(json!("x")).err().map(|e| e.to_string()).as_deref(), Some("'str' object has no attribute 'get'"));
        assert_eq!(up(json!([1])).err().map(|e| e.to_string()).as_deref(), Some("'list' object has no attribute 'get'"));
        let v = up(json!({"sandbox": 0, "path": "a", "filename": "f"})).unwrap();
        assert_eq!(v["content"], "sandbox, path and filename are all required");
        let v = up(json!({"sandbox": "sb", "path": [], "filename": "f"})).unwrap();
        assert_eq!(v["content"], "sandbox, path and filename are all required");
        let v = up(json!({"sandbox": 5, "path": "a", "filename": "f"})).unwrap();
        assert_eq!(v["content"], "startswith first arg must be str or a tuple of str, not int");
        let e = up(json!({"sandbox": "sb", "path": "a", "filename": 5})).err().map(|e| e.to_string());
        assert_eq!(e.as_deref(), Some("a bytes-like object is required, not 'int'"));
        let e = up(json!({"sandbox": "sb", "path": 5, "filename": "f"})).err().map(|e| e.to_string());
        assert_eq!(e.as_deref(), Some("'int' object has no attribute 'rstrip'"));
        let v = up(json!({"sandbox": "sb", "path": "up/", "filename": "a/b.txt", "content": "aGk="})).unwrap();
        assert_eq!(v["content"], "uploaded up/a_b.txt");
        assert_eq!(std::fs::read_to_string(root.join("up").join("a_b.txt")).unwrap(), "hi");
        // no sandbox_paths: python's _git_root is None (git runs in the server's cwd), and
        // the repo listing dies on os.path.join(None, '.git') outside any try
        ol_core::config::set_for_tests(json!({"modules": {"settings": {"coder": {
            "sandbox_paths": [],
            "folder_blacklist": [],
            "read-only": false,
        }}}}));
        assert!(matches!(c.git_root("sb", ""), Ok(None)));
        assert_eq!(c.git_root("sb", "repo").err().as_deref(), Some("'NoneType' object has no attribute 'replace'"));
        let q = Map::from_iter([("sandbox".to_string(), json!("sb"))]);
        assert_eq!(c.route_git_repos(&q).err().map(|e| e.to_string()).as_deref(), Some(NONE_ROOT_ERR));
    }

    #[test]
    fn py_int_string_parsing_matches_python_int() {
        let ok = |s: &str| py_int(Some(&json!(s)), 0).ok();
        let bad = |s: &str| py_int(Some(&json!(s)), 0).err().map(|e| e.to_string());
        // unicode decimal digits (Nd), including a non-BMP run
        assert_eq!(ok(" \u{663} "), Some(3));
        assert_eq!(ok("\u{661}\u{662}\u{663}"), Some(123));
        assert_eq!(ok("\u{663}\u{663}\u{663}\u{663}\u{663}"), Some(33333));
        assert_eq!(ok("\u{ff15}"), Some(5));
        assert_eq!(ok("\u{1d7d5}"), Some(7));
        assert_eq!(ok("\u{663}_\u{660}"), Some(30));
        // single underscores between digits, signs, unicode whitespace
        assert_eq!(ok(" 1_000 "), Some(1000));
        assert_eq!(ok("+5"), Some(5));
        assert_eq!(ok("-0012"), Some(-12));
        assert_eq!(ok("\u{3000}5"), Some(5));
        assert_eq!(ok("-9223372036854775808"), Some(i64::MIN));
        // numeric but not decimal digits, and everything python rejects
        for s in ["\u{bd}", "1__0", "_1", "1_", "", "-", "\u{1c}5", "1 2"] {
            assert_eq!(bad(s).as_deref(), Some(format!("invalid literal for int() with base 10: {}", pyos::py_quote(s)).as_str()), "{s:?}");
        }
    }

    #[test]
    fn body_dict_refuses_non_dict_bodies_like_python_get() {
        let str_err = |path: &str, read_only: bool| body_dict(path, Some(json!("x")), read_only).err().map(|e| e.to_string());
        assert_eq!(str_err("upload", false).as_deref(), Some("'str' object has no attribute 'get'"));
        assert_eq!(body_dict("upload", Some(json!([1])), false).err().map(|e| e.to_string()).as_deref(), Some("'list' object has no attribute 'get'"));
        // falsy bodies are `or {}`
        assert!(body_dict("upload", Some(json!(0)), false).unwrap().is_empty());
        assert!(body_dict("upload", None, false).unwrap().is_empty());
        // read-only refusal comes first, except for lint and search (no read-only check in python)
        assert!(body_dict("upload", Some(json!("x")), true).unwrap().is_empty());
        assert!(body_dict("save", Some(json!("x")), true).unwrap().is_empty());
        assert_eq!(str_err("search", true).as_deref(), Some("'str' object has no attribute 'get'"));
        assert_eq!(str_err("lint", true).as_deref(), Some("'str' object has no attribute 'get'"));
        // routes that never read the body are unaffected
        assert!(body_dict("tree", Some(json!("x")), false).unwrap().is_empty());
    }
}
