//! Port of modules/coder/module.py: sandboxed file tools for the AI, plus the webui
//! extension routes (file tree/editor/search and git panel).

#[path = "../../../../generated/modules/coder.rs"]
mod meta;
pub mod syntax;

use crate::pyos;
use anyhow::anyhow;
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::{py_str, truthy};
use ol_core::registry::{ModuleDescriptor, Registry};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

const REGEX_BACKTRACK_LIMIT: usize = 1_000_000;
const BUILTIN_TEMPLATES_PATH: &str = "modules/coder/templates";
/// files the webui editor refuses to open (binary / huge)
const UI_MAX_READ_BYTES: u64 = 1_000_000;

pub fn register(r: &mut Registry) {
    r.add_module(ModuleDescriptor {
        name: "coder",
        class_name: meta::CLASS_NAME,
        doc: meta::DOC,
        settings: meta::settings,
        unsafe_: meta::UNSAFE,
        category: meta::CATEGORY,
        factory: |base| Arc::new(Coder::new(base)),
    });
}

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

/// compile a user regex like python's `regex.compile(pattern, flags)`
fn compile_regex(pattern: &str, ignore_case: bool) -> R<fancy_regex::Regex> {
    let mut b = fancy_regex::RegexBuilder::new(pattern);
    b.case_insensitive(ignore_case);
    b.backtrack_limit(REGEX_BACKTRACK_LIMIT);
    b.build().map_err(|e| e.to_string())
}

/// `pattern.search(line)`; lines keep their trailing newline in python, where `$` also
/// matches just before it - search without it so anchors behave the same.
fn re_search(re: &fancy_regex::Regex, line: &str) -> R<bool> {
    let l = line.strip_suffix('\n').unwrap_or(line);
    re.is_match(l).map_err(|e| e.to_string())
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

/// run git; returns (returncode, raw stdout)
async fn run_git_bytes(root: &str, args: &[String], timeout: u64) -> (i32, Vec<u8>) {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args).current_dir(root).kill_on_drop(true).stdin(std::process::Stdio::null());
    match tokio::time::timeout(Duration::from_secs(timeout), cmd.output()).await {
        Err(_) => (124, b"git command timed out".to_vec()),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => (127, b"git executable not found".to_vec()),
        Ok(Err(e)) => (1, e.to_string().into_bytes()),
        Ok(Ok(out)) => (out.status.code().unwrap_or(-1), out.stdout),
    }
}

/// run git; returns (ok, output)
async fn run_git(root: &str, args: &[&str], accept: &[i32]) -> (bool, String) {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args).current_dir(root).kill_on_drop(true).stdin(std::process::Stdio::null());
    let out = match tokio::time::timeout(Duration::from_secs(15), cmd.output()).await {
        Err(_) => return (false, "git command timed out".into()),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return (false, "git executable not found".into()),
        Ok(Err(e)) => return (false, e.to_string()),
        Ok(Ok(o)) => o,
    };
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
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

async fn git(root: &str, args: &[&str]) -> (bool, String) {
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
        if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            if idx < fields.len() {
                let newp = fields[idx];
                idx += 1;
                if !newp.is_empty() {
                    path = decode(newp);
                }
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
        Some(Value::String(s)) => s.trim().parse::<i64>().map_err(|_| anyhow!("invalid literal for int() with base 10: {}", pyos::py_quote(s))),
        Some(other) => Err(anyhow!("int() argument must be a string, a bytes-like object or a real number, not '{}'", py_type(other))),
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

    fn blacklist(&self) -> Vec<String> {
        self.cfg_list("folder_blacklist")
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

    fn check_syntax(&self, code: &str, file_path: &str) -> Vec<String> {
        let Some(lang) = syntax::detect_language_from_path(file_path) else { return Vec::new() };
        if matches!(lang, "vimdoc" | "html" | "htm") {
            return Vec::new();
        }
        match syntax::diagnostics(code, lang) {
            // language not compiled in: like an unsupported language in python, pass
            None => Vec::new(),
            Some(diags) => diags.iter().map(|d| format!("Line {}, Col {}: {}", d.start_line + 1, d.start_column + 1, d.message)).collect(),
        }
    }

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
    fn get_templates(&self) -> Vec<String> {
        let mut all = Vec::new();
        for folder in self.get_template_folders() {
            let path = match ol_core::paths::get_path(&folder) {
                Ok(p) => p,
                Err(_) => return vec!["status".into(), "content".into()],
            };
            match pyos::listdir(&path) {
                Ok(t) => all.extend(t),
                Err(_) => return vec!["status".into(), "content".into()],
            }
        }
        all
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
            if rel.split('/').any(|p| blacklist.iter().any(|b| b == p)) {
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
        let early = pyos::walk(&target_path, &mut |dirpath, folders, files| {
            folders.retain(|d| !d.starts_with('.') && !blacklist.contains(d));
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
        let errors = self.check_syntax(&content, &target);
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
        let errors = self.check_syntax(&new_content, &target);
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
        let autosave = self.cfg("autosave_interval").filter(truthy).and_then(|v| ol_core::pyjson::as_f64(&v)).map(|f| f as i64).unwrap_or(0);
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
        let blacklist = self.blacklist();
        let names = match pyos::listdir(&target) {
            Ok(n) => n,
            Err(e) => return self.result(e, false),
        };
        let mut entries: Vec<(bool, String, Value)> = names
            .into_iter()
            .map(|name| {
                let full = pyos::join(&target, &name);
                let is_dir = pyos::isdir(&full);
                let child_rel = pyos::normpath(&pyos::join(&rel_path, &name));
                let v = json!({
                    "name": name,
                    "path": child_rel,
                    "is_dir": is_dir,
                    "hidden": name.starts_with('.'),
                    "blacklisted": is_dir && blacklist.contains(&name),
                });
                (is_dir, name.to_lowercase(), v)
            })
            .collect();
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
        let blacklist = self.blacklist();
        let mut files: Vec<String> = Vec::new();
        pyos::walk::<()>(&target, &mut |dirpath, folders, filenames| {
            folders.retain(|d| !d.starts_with('.') && !blacklist.contains(d));
            for name in filenames {
                if name.starts_with('.') {
                    continue;
                }
                files.push(pyos::relpath(&pyos::join(dirpath, name), &target));
            }
            ControlFlow::Continue(())
        });
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
        let errors = self.check_syntax(&content, &target);
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
        let root = match self.full_sandbox(&sandbox) {
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
        let needle_lower = needle.to_lowercase();
        let blacklist = self.blacklist();
        let mut results: Vec<Value> = Vec::new();
        let mut truncated = false;
        pyos::walk::<()>(&root, &mut |dirpath, dirnames, filenames| {
            dirnames.retain(|d| !d.starts_with('.') && !blacklist.contains(d));
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
                let Ok(content) = pyos::read_text(&fpath) else { continue };
                for (i, line) in pyos::readlines(&content).iter().enumerate() {
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
                if truncated {
                    break;
                }
            }
            if truncated {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        Ok(json!({"results": results, "truncated": truncated}))
    }

    // ---------------- git

    fn git_root(&self, sandbox: &str, subpath: &str) -> R<String> {
        let full = self.full_sandbox(sandbox)?;
        if subpath.is_empty() {
            return Ok(full);
        }
        ol_core::paths::sandbox_path(&full, Some(subpath)).map_err(|e| e.to_string())
    }

    fn check_git_paths(root: &str, paths: &[String]) -> R<()> {
        let sandbox_root = pyos::dirname(root);
        for p in paths {
            let full = pyos::normpath(&pyos::join(root, p));
            if full != sandbox_root && !full.starts_with(&format!("{}/", sandbox_root)) {
                return Err(format!("path escapes the sandbox: {}", p));
            }
        }
        Ok(())
    }

    fn route_git_repos(&self, q: &Map<String, Value>) -> Value {
        let sandbox = sget(q, "sandbox");
        if sandbox.is_empty() {
            return self.result("no sandbox given", false);
        }
        let root = match self.full_sandbox(&sandbox) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let blacklist = self.blacklist();
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
        pyos::walk::<()>(&root, &mut |dirpath, dirnames, _| {
            let rel = pyos::relpath(dirpath, &root);
            let depth = if rel == "." { 0 } else { rel.split('/').count() };
            dirnames.retain(|d| !d.starts_with('.') && !blacklist.contains(d) && depth + 1 <= max_depth);
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
        json!({"repos": repos})
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
        let (ok, _) = git(&root, &["rev-parse", "--is-inside-work-tree"]).await;
        if !ok {
            return json!({"is_repo": false, "read_only": self.read_only()});
        }
        let (_, branch) = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        let (ok_b, b_out) = git(&root, &["branch", "--format=%(refname:short)"]).await;
        let branches: Vec<String> = if ok_b { b_out.split('\n').map(|b| b.trim()).filter(|b| !b.is_empty()).map(String::from).collect() } else { Vec::new() };
        let (rc, raw) = run_git_bytes(&root, &s(&["status", "--porcelain=v1", "-z"]), 15).await;
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
        let (ok, _) = git(&root, &["rev-parse", "--is-inside-work-tree"]).await;
        if !ok {
            return Ok(json!({"is_repo": false}));
        }
        let (_, branch) = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        let (rc, raw) = run_git_bytes(&root, &[String::from("log"), "--pretty=%h%x09%s".into(), format!("-n{}", limit)], 15).await;
        if rc != 0 {
            return Ok(json!({"is_repo": true, "branch": branch, "log": []}));
        }
        Ok(json!({"is_repo": true, "branch": branch, "log": parse_log(&raw)}))
    }

    fn body_paths(b: &Map<String, Value>) -> Vec<String> {
        match b.get("paths") {
            Some(Value::Array(a)) => a.iter().map(py_str).collect(),
            Some(Value::String(s)) => s.chars().map(|c| c.to_string()).collect(),
            _ => Vec::new(),
        }
    }

    async fn route_git_stage(&self, b: &Map<String, Value>, unstage: bool) -> Value {
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let paths = Self::body_paths(b);
        if sandbox.is_empty() || !present(b, "paths") {
            return self.result("sandbox and paths are both required", false);
        }
        let root = match self.git_root(&sandbox, &sget(b, "subpath")).and_then(|r| Self::check_git_paths(&r, &paths).map(|_| r)) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let abs: Vec<String> = paths.iter().map(|p| pyos::normpath(&pyos::join(&root, p))).collect();
        let mut args: Vec<&str> = if unstage { vec!["restore", "--staged", "--"] } else { vec!["add", "--"] };
        args.extend(abs.iter().map(|s| s.as_str()));
        let (ok, out) = git(&root, &args).await;
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
        let (ok, out) = git(&root, &["commit", "-m", &message]).await;
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
        let (ok, out) = git(&root, &["switch", &name]).await;
        if !ok {
            return self.result(out, false);
        }
        self.result(if out.is_empty() { format!("switched to {}", name) } else { out }, true)
    }

    async fn pick_remote(root: &str) -> Option<String> {
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
        let (ok, branch) = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        if !ok {
            return self.result(branch, false);
        }
        if branch == "HEAD" {
            return self.result("detached HEAD: switch to a branch before pushing", false);
        }
        let Some(remote) = Self::pick_remote(&root).await else {
            return self.result("this repo has no remotes, nowhere to push to", false);
        };
        let (ok, upstream) = git(&root, &["rev-parse", "--abbrev-ref", "@{u}"]).await;
        let upstream = if ok { upstream.trim().to_string() } else { String::new() };
        let has_upstream = upstream.contains('/');
        let (target, (rc, raw)) = if has_upstream {
            (upstream.clone(), run_git_bytes(&root, &[String::from("log"), "--pretty=%h%x09%s".into(), format!("{}..HEAD", upstream), "-n100".into()], 15).await)
        } else {
            (format!("{}/{}", remote, branch), run_git_bytes(&root, &s(&["log", "--pretty=%h%x09%s", "HEAD", "--not", "--remotes", "-n100"]), 15).await)
        };
        if rc != 0 {
            let t = String::from_utf8_lossy(&raw).trim().to_string();
            return self.result(if t.is_empty() { "git log failed".into() } else { t }, false);
        }
        let ahead = parse_log(&raw);
        let mut behind = 0i64;
        if has_upstream {
            let (ok, out) = git(&root, &["rev-list", "--count", &format!("HEAD..{}", upstream)]).await;
            let o = out.trim();
            if ok && !o.is_empty() && o.chars().all(|c| c.is_ascii_digit()) {
                behind = o.parse().unwrap_or(0);
            }
        }
        let (_, raw) = run_git_bytes(&root, &s(&["status", "--porcelain=v1"]), 15).await;
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
        let (ok, branch) = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        if !ok || branch == "HEAD" {
            return self.result("detached HEAD: switch to a branch before pushing", false);
        }
        let Some(remote) = Self::pick_remote(&root).await else {
            return self.result("this repo has no remotes, nowhere to push to", false);
        };
        let (ok, upstream) = git(&root, &["rev-parse", "--abbrev-ref", "@{u}"]).await;
        let upstream = if ok { upstream.trim().to_string() } else { String::new() };
        let args: Vec<String> = if let Some((up_remote, up_branch)) = upstream.split_once('/') {
            vec!["push".into(), up_remote.into(), format!("{}:{}", branch, up_branch)]
        } else {
            vec!["push".into(), "-u".into(), remote, branch]
        };
        let (rc, raw) = run_git_bytes(&root, &args, 180).await;
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
        let (rc, raw) = run_git_bytes(&root, &[String::from("log"), "--pretty=%h%x09%s".into(), format!("-n{}", limit)], 15).await;
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
        let root = match self.git_root(&sandbox, &sget(q, "subpath")).and_then(|r| Self::check_git_paths(&r, std::slice::from_ref(&rel_path)).map(|_| r)) {
            Ok(r) => r,
            Err(e) => return Err(self.result(e, false)),
        };
        let abs = pyos::normpath(&pyos::join(&root, &rel_path));
        let (rc, raw) = if untracked {
            run_git_bytes(&root, &[String::from("diff"), "--no-index".into(), "--".into(), "/dev/null".into(), abs], 15).await
        } else if staged {
            run_git_bytes(&root, &[String::from("diff"), "--staged".into(), "--".into(), abs], 15).await
        } else {
            run_git_bytes(&root, &[String::from("diff"), "--".into(), abs], 15).await
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
        let (ok, branch) = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        if !ok {
            return self.result(branch, false);
        }
        let Some(remote) = Self::pick_remote(&root).await else {
            return self.result("this repo has no remotes", false);
        };
        let (rc, raw) = run_git_bytes(&root, &[String::from("fetch"), remote, "--prune".into()], 120).await;
        let out = String::from_utf8_lossy(&raw).trim().to_string();
        if rc != 0 {
            return self.result(if out.is_empty() { "git fetch failed".into() } else { out }, false);
        }
        let (ok, upstream) = git(&root, &["rev-parse", "--abbrev-ref", "@{u}"]).await;
        let mut incoming = Vec::new();
        if ok && upstream.trim().contains('/') {
            let (rc2, raw2) = run_git_bytes(&root, &[String::from("log"), "--pretty=%h%x09%s".into(), format!("HEAD..{}", upstream.trim()), "-n100".into()], 15).await;
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
        let (ok, branch) = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        if !ok || branch == "HEAD" {
            return self.result("detached HEAD: switch to a branch before pulling", false);
        }
        let (rc, raw) = run_git_bytes(&root, &s(&["pull"]), 120).await;
        let out = String::from_utf8_lossy(&raw).trim().to_string();
        if rc != 0 {
            let (_, craw) = run_git_bytes(&root, &s(&["diff", "--name-only", "--diff-filter=U"]), 15).await;
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
        let (ok, out) = if checkout { git(&root, &["switch", "-c", &name]).await } else { git(&root, &["branch", &name]).await };
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
        let (_, current) = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        if current.trim() == name {
            return self.result("cannot delete the branch you are currently on", false);
        }
        let (ok, out) = git(&root, &["branch", if force { "-D" } else { "-d" }, &name]).await;
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
        let (rc, raw) = run_git_bytes(&root, &s(&["diff", "--name-only", "--diff-filter=U"]), 15).await;
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
        let root = match self.git_root(&sandbox, &sget(b, "subpath")).and_then(|r| Self::check_git_paths(&r, std::slice::from_ref(&rel_path)).map(|_| r)) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let abs = pyos::normpath(&pyos::join(&root, &rel_path));
        let (ok, out) = match strategy.as_str() {
            "ours" => git(&root, &["checkout", "--ours", "--", &rel_path]).await,
            "theirs" => git(&root, &["checkout", "--theirs", "--", &rel_path]).await,
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
        git(&root, &["add", "--", &abs]).await;
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
        let root = match self.git_root(&sandbox, &sget(b, "subpath")).and_then(|r| Self::check_git_paths(&r, std::slice::from_ref(&rel_path)).map(|_| r)) {
            Ok(r) => r,
            Err(e) => return self.result(e, false),
        };
        let abs = pyos::normpath(&pyos::join(&root, &rel_path));
        let (ok, out) = git(&root, &["restore", "--source=HEAD", "--staged", "--worktree", "--", &abs]).await;
        if !ok {
            return self.result(out, false);
        }
        self.result(format!("reverted {} to HEAD", rel_path), true)
    }

    fn route_upload(&self, b: &Map<String, Value>) -> Value {
        use base64::Engine;
        if self.read_only() {
            return self.result("coder is in read-only mode", false);
        }
        let sandbox = sget(b, "sandbox");
        let rel_path = sget(b, "path");
        let content_b64 = sget(b, "content");
        let filename = sget(b, "filename");
        if sandbox.is_empty() || rel_path.is_empty() || filename.is_empty() {
            return self.result("sandbox, path and filename are all required", false);
        }
        let safe_name = filename.replace(['/', '\\'], "_");
        let target_rel = if rel_path.is_empty() { safe_name } else { format!("{}/{}", rel_path.trim_end_matches('/'), safe_name) };
        let target = match self.get_sandbox_subpath(&sandbox, &target_rel) {
            Ok(t) => t,
            Err(e) => return self.result(e, false),
        };
        if pyos::exists(&target) {
            return self.result(format!("file already exists: {}", target_rel), false);
        }
        // python b64decode (non-validating): drop characters outside the alphabet
        let cleaned: String = content_b64.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')).collect();
        let raw = match base64::engine::general_purpose::STANDARD.decode(cleaned.as_bytes()) {
            Ok(r) => r,
            Err(_) => return self.result("invalid base64 content", false),
        };
        if raw.len() > 10_000_000 {
            return self.result("file too large (max 10MB)", false);
        }
        let parent = pyos::dirname(&target);
        let r = if parent.is_empty() { Ok(()) } else { pyos::makedirs(&parent, true) };
        if let Err(e) = r.and_then(|_| pyos::write_file(&target, &raw)) {
            return self.result(e, false);
        }
        self.result(format!("uploaded {}", target_rel), true)
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
        let templates = self.get_templates();
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
        let b: Map<String, Value> = body.and_then(|v| v.as_object().cloned()).unwrap_or_default();
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
            "git_repos" => self.route_git_repos(q),
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
            "upload" => self.route_upload(&b),
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
}
