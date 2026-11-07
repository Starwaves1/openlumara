//! Small re-implementations of the python `os`, `os.path`, `glob`, `fnmatch` and `shutil`
//! behaviour the dev modules rely on, including python-style error messages
//! (`[Errno 2] No such file or directory: '/x'`).

use std::io::{self, Read, Write};
use std::ops::ControlFlow;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// python `str(OSError)`: `[Errno N] strerror: 'path'`
pub fn os_err(e: &io::Error, path: &str) -> String {
    match e.raw_os_error() {
        Some(code) => format!("[Errno {}] {}: {}", code, strerror(code), py_quote(path)),
        None => e.to_string(),
    }
}

/// python `str(OSError)` for two-path errors (rename, link, ...)
pub fn os_err2(e: &io::Error, src: &str, dst: &str) -> String {
    match e.raw_os_error() {
        Some(code) => format!("[Errno {}] {}: {} -> {}", code, strerror(code), py_quote(src), py_quote(dst)),
        None => e.to_string(),
    }
}

pub fn strerror(code: i32) -> String {
    let s = io::Error::from_raw_os_error(code).to_string();
    match s.rfind(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// python repr() of a str
pub fn py_quote(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        let cp = c as u32;
        match c {
            c if c == quote || c == '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ if cp < 0x20 || cp == 0x7f => out.push_str(&format!("\\x{:02x}", cp)),
            _ if cp < 0x7f => out.push(c),
            // python escapes non-printable code points (Cc, Cf, Co, Cn, Zl, Zp, Zs)
            _ if !is_printable(c) => {
                if cp <= 0xff {
                    out.push_str(&format!("\\x{:02x}", cp));
                } else if cp <= 0xffff {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    out.push_str(&format!("\\U{:08x}", cp));
                }
            }
            _ => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// python `str.isprintable()` for one non-ascii code point
fn is_printable(c: char) -> bool {
    static NON_PRINTABLE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = NON_PRINTABLE.get_or_init(|| {
        regex::Regex::new(r"[\p{Cc}\p{Cf}\p{Co}\p{Cn}\p{Zl}\p{Zp}\p{Zs}]").expect("static unicode class regex")
    });
    !re.is_match(c.encode_utf8(&mut [0u8; 4]))
}

/// python `open(path, 'r', encoding='utf-8').read()`: strict utf-8 decoding plus universal
/// newline translation. Errors are python-formatted.
pub fn read_text(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| os_err(&e, path))?;
    decode_utf8(&bytes).map(|s| universal_newlines(&s))
}

/// strict utf-8 decode with python's UnicodeDecodeError message
pub fn decode_utf8(bytes: &[u8]) -> Result<String, String> {
    std::str::from_utf8(bytes).map(str::to_string).map_err(|e| decode_error(bytes, &e))
}

/// python's UnicodeDecodeError text: a one-byte span is `byte 0xNN in position N`, anything
/// longer is `bytes in position S-E` with the full span (`error_len`, or the rest of the input
/// when the sequence is cut off at the end).
fn decode_error(bytes: &[u8], e: &std::str::Utf8Error) -> String {
    let pos = e.valid_up_to();
    let (span, reason) = match e.error_len() {
        Some(n) => {
            let b = bytes[pos];
            let reason = if (0x80..0xc2).contains(&b) || b >= 0xf5 { "invalid start byte" } else { "invalid continuation byte" };
            (n, reason)
        }
        None => (bytes.len() - pos, "unexpected end of data"),
    };
    if span == 1 {
        format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: {}", bytes[pos], pos, reason)
    } else {
        format!("'utf-8' codec can't decode bytes in position {}-{}: {}", pos, pos + span - 1, reason)
    }
}

pub fn is_decode_error(msg: &str) -> bool {
    msg.starts_with("'utf-8' codec can't decode")
}

/// text-mode newline translation (`\r\n` and `\r` -> `\n`)
pub fn universal_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// python `for line in open(p, encoding='utf-8')`. Python decodes the file in 8 KiB chunks and
/// yields each line once its newline has been decoded, so a bad byte only fails the chunk that
/// holds it. Returns the lines completed before that point and the decode error that stopped the
/// read (if any). A failure to open or read the file is an `Err`.
pub fn stream_lines(path: &str) -> Result<(Vec<String>, Option<String>), String> {
    const CHUNK: usize = 8192;
    let bytes = std::fs::read(path).map_err(|e| os_err(&e, path))?;
    let mut splitter = LineSplitter::default();
    for chunk in bytes.chunks(CHUNK) {
        if let Err(msg) = splitter.push(chunk, false) {
            return Ok((splitter.lines, Some(msg)));
        }
    }
    if let Err(msg) = splitter.push(&[], true) {
        return Ok((splitter.lines, Some(msg)));
    }
    Ok((splitter.finish(), None))
}

/// incremental utf-8 decode + universal newlines + line split (python TextIOWrapper)
#[derive(Default)]
struct LineSplitter {
    pending: Vec<u8>,
    cur: String,
    cr: bool,
    lines: Vec<String>,
}

impl LineSplitter {
    fn push(&mut self, data: &[u8], last: bool) -> Result<(), String> {
        self.pending.extend_from_slice(data);
        let text = decode_pending(&mut self.pending, last)?;
        self.feed(&text);
        Ok(())
    }

    fn feed(&mut self, text: &str) {
        for c in text.chars() {
            if self.cr {
                // a '\r' ends a line; a '\n' right after it is part of the same newline
                self.cr = false;
                self.newline();
                if c == '\n' {
                    continue;
                }
            }
            match c {
                '\r' => self.cr = true,
                '\n' => self.newline(),
                _ => self.cur.push(c),
            }
        }
    }

    fn newline(&mut self) {
        self.cur.push('\n');
        self.lines.push(std::mem::take(&mut self.cur));
    }

    fn finish(mut self) -> Vec<String> {
        if self.cr {
            self.newline();
        }
        if !self.cur.is_empty() {
            self.lines.push(self.cur);
        }
        self.lines
    }
}

/// decodes the buffered bytes; an incomplete sequence at the end is kept for the next chunk
/// unless this is the last chunk
fn decode_pending(pending: &mut Vec<u8>, last: bool) -> Result<String, String> {
    match std::str::from_utf8(pending) {
        Ok(s) => {
            let out = s.to_string();
            pending.clear();
            Ok(out)
        }
        Err(e) if e.error_len().is_none() && !last => {
            let head: Vec<u8> = pending.drain(..e.valid_up_to()).collect();
            String::from_utf8(head).map_err(|e| e.to_string())
        }
        Err(e) => Err(decode_error(pending, &e)),
    }
}

/// python `readlines()` restricted to text-mode lines (`f.readlines()`)
pub fn readlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c == '\n' {
            out.push(&s[start..=i]);
            start = i + 1;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// python `str.isspace()` for one code point: `char::is_whitespace` plus U+001C-U+001F
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// python `str.rstrip()` (whitespace)
pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(is_py_space)
}

/// os.path.expanduser
pub fn expanduser(p: &str) -> String {
    if !p.starts_with('~') {
        return p.to_string();
    }
    let rest_start = p.find('/').unwrap_or(p.len());
    let user = &p[1..rest_start];
    let home = if user.is_empty() {
        match std::env::var("HOME") {
            Ok(h) => Some(h),
            // posixpath: no $HOME -> pwd.getpwuid(os.getuid()).pw_dir
            Err(_) => unsafe {
                let pw = libc::getpwuid(libc::getuid());
                if pw.is_null() {
                    None
                } else {
                    Some(std::ffi::CStr::from_ptr((*pw).pw_dir).to_string_lossy().to_string())
                }
            },
        }
    } else {
        // ~user: look up the passwd database
        let cuser = std::ffi::CString::new(user).ok();
        cuser.and_then(|c| unsafe {
            let pw = libc::getpwnam(c.as_ptr());
            if pw.is_null() {
                None
            } else {
                Some(std::ffi::CStr::from_ptr((*pw).pw_dir).to_string_lossy().to_string())
            }
        })
    };
    match home {
        Some(h) => {
            // posixpath: userhome.rstrip(sep)
            let h = h.trim_end_matches('/').to_string();
            let joined = format!("{}{}", h, &p[rest_start..]);
            if joined.is_empty() {
                "/".into()
            } else {
                joined
            }
        }
        None => p.to_string(),
    }
}

/// os.path.basename
pub fn basename(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

/// os.path.dirname
pub fn dirname(p: &str) -> String {
    let i = p.rfind('/').map(|i| i + 1).unwrap_or(0);
    let head = &p[..i];
    if !head.is_empty() && head.chars().any(|c| c != '/') {
        head.trim_end_matches('/').to_string()
    } else {
        head.to_string()
    }
}

/// os.path.split
pub fn split(p: &str) -> (String, String) {
    let i = p.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (head, tail) = (&p[..i], &p[i..]);
    let head = if !head.is_empty() && head.chars().any(|c| c != '/') { head.trim_end_matches('/') } else { head };
    (head.to_string(), tail.to_string())
}

/// os.path.join (two components)
pub fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        return b.to_string();
    }
    if a.is_empty() || a.ends_with('/') {
        format!("{}{}", a, b)
    } else {
        format!("{}/{}", a, b)
    }
}

/// os.path.splitext
pub fn splitext(p: &str) -> (String, String) {
    let sep = p.rfind('/').map(|i| i + 1).unwrap_or(0);
    let name = &p[sep..];
    if let Some(dot) = name.rfind('.') {
        // leading dots don't count
        if name[..dot].chars().any(|c| c != '.') {
            let i = sep + dot;
            return (p[..i].to_string(), p[i..].to_string());
        }
    }
    (p.to_string(), String::new())
}

/// os.path.normpath (posix)
pub fn normpath(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let initial = if p.starts_with("//") && !p.starts_with("///") {
        2
    } else if p.starts_with('/') {
        1
    } else {
        0
    };
    let mut out: Vec<&str> = Vec::new();
    for comp in p.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial == 0 && out.is_empty()) || out.last() == Some(&"..") {
            out.push(comp);
        } else if !out.is_empty() {
            out.pop();
        }
    }
    let s = format!("{}{}", "/".repeat(initial), out.join("/"));
    if s.is_empty() {
        ".".into()
    } else {
        s
    }
}

/// os.path.abspath
pub fn abspath(p: &str) -> String {
    if p.starts_with('/') {
        normpath(p)
    } else {
        let cwd = std::env::current_dir().map(|c| c.to_string_lossy().to_string()).unwrap_or_else(|_| "/".into());
        normpath(&join(&cwd, p))
    }
}

/// os.path.relpath
pub fn relpath(path: &str, start: &str) -> String {
    let a = abspath(start);
    let b = abspath(path);
    let sl: Vec<&str> = a.split('/').filter(|x| !x.is_empty()).collect();
    let pl: Vec<&str> = b.split('/').filter(|x| !x.is_empty()).collect();
    let mut i = 0;
    while i < sl.len() && i < pl.len() && sl[i] == pl[i] {
        i += 1;
    }
    let mut parts: Vec<&str> = vec![".."; sl.len() - i];
    parts.extend_from_slice(&pl[i..]);
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

pub fn isdir(p: &str) -> bool {
    Path::new(p).is_dir()
}
pub fn isfile(p: &str) -> bool {
    Path::new(p).is_file()
}
pub fn exists(p: &str) -> bool {
    Path::new(p).exists()
}
pub fn lexists(p: &str) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// os.listdir (python error messages). Any readdir error fails the whole listing, like python.
pub fn listdir(p: &str) -> Result<Vec<String>, String> {
    // os.listdir('') raises ENOENT (unlike glob, which maps '' to curdir)
    let rd = std::fs::read_dir(p).map_err(|e| os_err(&e, p))?;
    rd.map(|e| e.map(|e| e.file_name().to_string_lossy().to_string()))
        .collect::<io::Result<Vec<_>>>()
        .map_err(|e| os_err(&e, p))
}

/// os.path.exists for a file that must be executable (python `_access_check`)
fn exec_check(p: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(p) else { return false };
    exists(p) && !isdir(p) && unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0
}

/// shutil.which
pub fn which(cmd: &str) -> Option<String> {
    if cmd.contains('/') {
        return exec_check(cmd).then(|| cmd.to_string());
    }
    // shutil.which: PATH unset -> os.defpath / confstr CS_PATH
    let path = std::env::var("PATH").unwrap_or_else(|_| "/bin:/usr/bin".to_string());
    which_in(cmd, &path)
}

/// shutil.which(cmd, path=...) for a bare command name
fn which_in(cmd: &str, path: &str) -> Option<String> {
    // python 3.14: `if not path: return None` (an empty PATH finds nothing, not even in the cwd)
    if path.is_empty() {
        return None;
    }
    // an empty PATH entry is the current directory, but os.path.join('', cmd) is just cmd
    path.split(':').map(|dir| join(dir, cmd)).find(|candidate| exec_check(candidate))
}

/// os.walk (top-down, followlinks=False). `prune` receives the subdirectory names of each
/// directory and can filter them in place (python: `folders[:] = [...]`). The visitor gets
/// (dirpath, dirnames, filenames) and may break out early.
/// the visitor `walk` calls per directory: (dirpath, dirnames, filenames)
pub type WalkVisit<'a, B> = dyn FnMut(&str, &mut Vec<String>, &[String]) -> ControlFlow<B> + 'a;

pub fn walk<B>(top: &str, visit: &mut WalkVisit<'_, B>) -> Option<B> {
    // python os.walk: an error reading this directory abandons it (nothing is yielded for it)
    let Ok(rd) = std::fs::read_dir(top) else { return None };
    let Ok(entries) = rd.collect::<io::Result<Vec<_>>>() else { return None };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut symlinked_dirs = Vec::new();
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let path = e.path();
        // python: an error while stat-ing an entry makes it a non-directory
        let is_dir = path.is_dir();
        if is_dir {
            if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                symlinked_dirs.push(name.clone());
            }
            dirs.push(name);
        } else {
            files.push(name);
        }
    }
    if let ControlFlow::Break(b) = visit(top, &mut dirs, &files) {
        return Some(b);
    }
    for d in dirs {
        if symlinked_dirs.contains(&d) {
            continue;
        }
        let sub = join(top, &d);
        if let Some(b) = walk(&sub, visit) {
            return Some(b);
        }
    }
    None
}

// ---------------------------------------------------------------- fnmatch / glob

/// fnmatch.translate -> anchored regex
pub fn fnmatch_translate(pat: &str) -> String {
    let chars: Vec<char> = pat.chars().collect();
    let mut res = String::from("(?s)^");
    let mut i = 0;
    let n = chars.len();
    while i < n {
        let c = chars[i];
        i += 1;
        match c {
            '*' => {
                while i < n && chars[i] == '*' {
                    i += 1;
                }
                res.push_str(".*");
            }
            '?' => res.push('.'),
            '[' => {
                let mut j = i;
                if j < n && chars[j] == '!' {
                    j += 1;
                }
                if j < n && chars[j] == ']' {
                    j += 1;
                }
                while j < n && chars[j] != ']' {
                    j += 1;
                }
                if j >= n {
                    res.push_str("\\[");
                } else {
                    let stuff = py_bracket_stuff(&chars, i, j);
                    i = j + 1;
                    if stuff.is_empty() {
                        // python: empty range never matches
                        res.push_str("(?!)");
                        continue;
                    }
                    if stuff == "!" {
                        res.push('.');
                        continue;
                    }
                    res.push('[');
                    for (idx, ch) in stuff.chars().enumerate() {
                        // only a leading '!' negates in fnmatch; a leading '^' is a literal (python escapes it)
                        if idx == 0 && ch == '!' {
                            res.push('^');
                            continue;
                        }
                        // escape regex set operators the regex crate would interpret; a leading ']' is
                        // a literal in python re but not in the regex crate (`[]-a]` would be `]`, `-`, `a`)
                        if (idx == 0 && (ch == '^' || ch == ']')) || matches!(ch, '[' | '&' | '~' | '|') {
                            res.push('\\');
                        }
                        res.push(ch);
                    }
                    res.push(']');
                }
            }
            other => res.push_str(&regex::escape(&other.to_string())),
        }
    }
    res.push('$');
    res
}

/// The text of a bracket class `pat[i..j]` as CPython 3.14 `fnmatch._translate` builds it:
/// when the class has a '-', the ranges are split into chunks and a reversed range (lo > hi)
/// is merged into its neighbour, so `[z-a]` ends up empty and `[z-ab-c]` becomes `b-c`.
fn py_bracket_stuff(pat: &[char], i: usize, j: usize) -> String {
    let body = &pat[i..j];
    if !body.contains(&'-') {
        return body.iter().collect::<String>().replace('\\', "\\\\");
    }
    let mut chunks: Vec<Vec<char>> = Vec::new();
    let mut start = i;
    let mut k = if pat[i] == '!' { i + 2 } else { i + 1 };
    while let Some(dash) = (k..j).find(|&x| pat[x] == '-') {
        chunks.push(pat[start..dash].to_vec());
        start = dash + 1;
        k = dash + 3;
    }
    let tail = pat[start..j].to_vec();
    if !tail.is_empty() {
        chunks.push(tail);
    } else if let Some(last) = chunks.last_mut() {
        last.push('-');
    }
    // python: "Remove empty ranges -- invalid in RE"
    for k in (1..chunks.len()).rev() {
        if let (Some(&lo_end), Some(&hi_start)) = (chunks[k - 1].last(), chunks[k].first()) {
            if lo_end > hi_start {
                let rest: Vec<char> = chunks[k][1..].to_vec();
                chunks[k - 1].pop();
                chunks[k - 1].extend(rest);
                chunks.remove(k);
            }
        }
    }
    chunks
        .iter()
        .map(|c| c.iter().collect::<String>().replace('\\', "\\\\").replace('-', "\\-"))
        .collect::<Vec<_>>()
        .join("-")
}

pub fn fnmatch(name: &str, pat: &str) -> bool {
    match regex::Regex::new(&fnmatch_translate(pat)) {
        Ok(re) => re.is_match(name),
        Err(_) => false,
    }
}

fn has_magic(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

fn is_hidden(s: &str) -> bool {
    s.starts_with('.')
}

fn glob_listdir(dirname: &str, dironly: bool) -> Vec<String> {
    let dir = if dirname.is_empty() { "." } else { dirname };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    // python _iterdir: an error while iterating ends the listing
    let mut names = Vec::new();
    for e in rd {
        let Ok(e) = e else { break };
        if !dironly || e.path().is_dir() {
            names.push(e.file_name().to_string_lossy().to_string());
        }
    }
    names
}

fn glob1(dirname: &str, pattern: &str, dironly: bool) -> Vec<String> {
    let mut names = glob_listdir(dirname, dironly);
    if !is_hidden(pattern) {
        names.retain(|n| !is_hidden(n));
    }
    let re = regex::Regex::new(&fnmatch_translate(pattern)).ok();
    names.into_iter().filter(|n| re.as_ref().map(|r| r.is_match(n)).unwrap_or(false)).collect()
}

fn glob0(dirname: &str, basename: &str) -> Vec<String> {
    if !basename.is_empty() {
        if lexists(&join(if dirname.is_empty() { "." } else { dirname }, basename)) {
            return vec![basename.to_string()];
        }
    } else if isdir(if dirname.is_empty() { "." } else { dirname }) {
        return vec![basename.to_string()];
    }
    Vec::new()
}

/// python `_rlistdir`: every non-hidden descendant, with no depth limit
fn rlistdir(dirname: &str, dironly: bool, out: &mut Vec<String>) {
    for x in glob_listdir(dirname, dironly) {
        if is_hidden(&x) {
            continue;
        }
        out.push(x.clone());
        let path = if dirname.is_empty() { x.clone() } else { join(dirname, &x) };
        let mut sub = Vec::new();
        rlistdir(&path, dironly, &mut sub);
        for y in sub {
            out.push(join(&x, &y));
        }
    }
}

fn glob2(dirname: &str, dironly: bool) -> Vec<String> {
    let mut out = Vec::new();
    if dirname.is_empty() || isdir(dirname) {
        out.push(String::new());
    }
    rlistdir(dirname, dironly, &mut out);
    out
}

fn iglob(pathname: &str, recursive: bool, dironly: bool) -> Vec<String> {
    let (dirname, basename) = split(pathname);
    if !has_magic(pathname) {
        if !basename.is_empty() {
            if lexists(pathname) {
                return vec![pathname.to_string()];
            }
        } else if isdir(&dirname) {
            return vec![pathname.to_string()];
        }
        return Vec::new();
    }
    if dirname.is_empty() {
        return if recursive && basename == "**" { glob2("", dironly) } else { glob1("", &basename, dironly) };
    }
    let dirs = if dirname != pathname && has_magic(&dirname) { iglob(&dirname, recursive, true) } else { vec![dirname.clone()] };
    let mut out = Vec::new();
    for d in dirs {
        let names = if has_magic(&basename) {
            if recursive && basename == "**" {
                glob2(&d, dironly)
            } else {
                glob1(&d, &basename, dironly)
            }
        } else {
            glob0(&d, &basename)
        };
        for name in names {
            out.push(join(&d, &name));
        }
    }
    out
}

/// glob.glob(pathname, recursive=...)
pub fn glob(pathname: &str, recursive: bool) -> Vec<String> {
    let mut r = iglob(pathname, recursive, false);
    // python iglob drops a leading '' when the pattern starts with '**' (`_isrecursive(pathname[:2])`)
    if ((recursive && pathname.starts_with("**")) || pathname.is_empty()) && r.first().is_some_and(|s| s.is_empty()) {
        r.remove(0);
    }
    r
}

// ---------------------------------------------------------------- shutil

/// python `os.path.samefile`: both paths stat (following symlinks) to the same device and inode
pub fn samefile(a: &str, b: &str) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// os.utime-style timestamp copy (`ns=(atime, mtime)`), optionally on the link itself
fn set_times(path: &str, meta: &std::fs::Metadata, follow: bool) -> io::Result<()> {
    let cpath = std::ffi::CString::new(path).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let times = [
        libc::timespec { tv_sec: meta.atime() as _, tv_nsec: meta.atime_nsec() as _ },
        libc::timespec { tv_sec: meta.mtime() as _, tv_nsec: meta.mtime_nsec() as _ },
    ];
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    if unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// shutil.copystat(src, dst): times first, then the permission bits (python order)
fn copystat(src: &str, dst: &str) -> Result<(), String> {
    let meta = std::fs::metadata(src).map_err(|e| os_err(&e, src))?;
    set_times(dst, &meta, true).map_err(|e| os_err(&e, dst))?;
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(meta.mode() & 0o7777)).map_err(|e| os_err(&e, dst))
}

/// shutil.copyfile + copystat (copy2 for one regular file). Like python, the source is opened
/// first, so a failure opening it is reported against the source path.
fn copy_file_with_meta(src: &str, dst: &str) -> Result<(), String> {
    if samefile(src, dst) {
        return Err(format!("{} and {} are the same file", py_quote(src), py_quote(dst)));
    }
    let src_meta = std::fs::metadata(src).map_err(|e| os_err(&e, src))?;
    // python: open(dir) raises IsADirectoryError (rust would only fail on the first read)
    if src_meta.is_dir() {
        return Err(os_err(&io::Error::from_raw_os_error(libc::EISDIR), src));
    }
    let mut fsrc = std::fs::File::open(src).map_err(|e| os_err(&e, src))?;
    let mut fdst = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)
        .map_err(|e| os_err(&e, dst))?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = fsrc.read(&mut buf).map_err(|e| os_err(&e, src))?;
        if n == 0 {
            break;
        }
        fdst.write_all(&buf[..n]).map_err(|e| os_err(&e, dst))?;
    }
    drop(fdst);
    copystat(src, dst)
}

/// shutil.copy2 (dst may be a directory)
pub fn copy2(src: &str, dst: &str) -> Result<(), String> {
    let real_dst = if isdir(dst) { join(dst, basename(src)) } else { dst.to_string() };
    if !exists(src) {
        return Err(format!("[Errno 2] No such file or directory: {}", py_quote(src)));
    }
    copy_file_with_meta(src, &real_dst)
}

/// how a copytree step failed: a plain OSError message, or shutil.Error's list of
/// (srcname, dstname, message) entries
enum CopyFail {
    Os(String),
    Tree(Vec<(String, String, String)>),
}

/// shutil.copytree(src, dst, symlinks=...) (dst must not exist). Like python, a failing entry
/// does not stop the copy; the failures are collected and reported together at the end.
fn copytree_impl(src: &str, dst: &str, symlinks: bool) -> Result<(), CopyFail> {
    let names = listdir(src).map_err(CopyFail::Os)?;
    makedirs(dst, false).map_err(CopyFail::Os)?;
    let mut errors = Vec::new();
    for name in names {
        let srcname = join(src, &name);
        let dstname = join(dst, &name);
        match copy_tree_entry(&srcname, &dstname, symlinks) {
            Ok(()) => {}
            Err(CopyFail::Tree(inner)) => errors.extend(inner),
            Err(CopyFail::Os(msg)) => errors.push((srcname, dstname, msg)),
        }
    }
    if let Err(msg) = copystat(src, dst) {
        errors.push((src.to_string(), dst.to_string(), msg));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(CopyFail::Tree(errors))
    }
}

/// one entry of copytree: symlinks are recreated as links when `symlinks` is set, directories
/// recurse, everything else is copied with copy2
fn copy_tree_entry(srcname: &str, dstname: &str, symlinks: bool) -> Result<(), CopyFail> {
    let link_meta = std::fs::symlink_metadata(srcname).ok().filter(|m| m.file_type().is_symlink());
    if let (true, Some(meta)) = (symlinks, link_meta) {
        let linkto = std::fs::read_link(srcname).map_err(|e| CopyFail::Os(os_err(&e, srcname)))?;
        let target = linkto.to_string_lossy().to_string();
        std::os::unix::fs::symlink(&linkto, dstname).map_err(|e| CopyFail::Os(os_err2(&e, &target, dstname)))?;
        set_times(dstname, &meta, false).map_err(|e| CopyFail::Os(os_err(&e, dstname)))?;
        return Ok(());
    }
    if Path::new(srcname).is_dir() {
        copytree_impl(srcname, dstname, symlinks)
    } else {
        copy2(srcname, dstname).map_err(CopyFail::Os)
    }
}

/// python `str(shutil.Error(list))`: the repr of the list of (src, dst, msg) tuples
fn shutil_error_repr(errors: &[(String, String, String)]) -> String {
    let items: Vec<String> = errors
        .iter()
        .map(|(a, b, c)| format!("({}, {}, {})", py_quote(a), py_quote(b), py_quote(c)))
        .collect();
    format!("[{}]", items.join(", "))
}

fn copytree_with(src: &str, dst: &str, symlinks: bool) -> Result<(), String> {
    match copytree_impl(src, dst, symlinks) {
        Ok(()) => Ok(()),
        Err(CopyFail::Os(msg)) => Err(msg),
        Err(CopyFail::Tree(errors)) => Err(shutil_error_repr(&errors)),
    }
}

/// shutil.copytree(src, dst) with the default symlinks=False
pub fn copytree(src: &str, dst: &str) -> Result<(), String> {
    copytree_with(src, dst, false)
}

/// shutil.rmtree
pub fn rmtree(p: &str) -> Result<(), String> {
    // shutil.rmtree refuses symlinks instead of unlinking them; CPython 3.14 raises it with errno None
    match std::fs::symlink_metadata(p) {
        Err(e) => return Err(os_err(&e, p)),
        Ok(m) if m.file_type().is_symlink() => return Err(format!("[Errno None] None: {}", py_quote(p))),
        Ok(_) => {}
    }
    rmtree_walk(p, true)
}

/// One directory of CPython 3.14's fd-based rmtree walk (`_rmtree_safe_fd_step`), run with the
/// default onexc, which re-raises. That re-raise carries `filename` = the directory whose step
/// failed, so the message names that directory (a file that cannot be unlinked is reported as
/// its parent), not the file itself. FileNotFoundError is ignored below the top-level path.
fn rmtree_walk(path: &str, top: bool) -> Result<(), String> {
    let fail = |e: io::Error| -> Result<(), String> {
        if !top && e.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(os_err(&e, path))
        }
    };
    let entries = match std::fs::read_dir(path) {
        Ok(rd) => match rd.collect::<io::Result<Vec<_>>>() {
            Ok(v) => v,
            Err(e) => return fail(e),
        },
        Err(e) => return fail(e),
    };
    // python unlinks this directory's files first, then walks the subdirectories last-pushed-first
    let mut subdirs = Vec::new();
    for entry in entries {
        let full = join(path, &entry.file_name().to_string_lossy());
        // DirEntry::file_type does not follow symlinks, so a link to a directory is unlinked
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            subdirs.push(full);
        } else if let Err(e) = std::fs::remove_file(&full) {
            if e.kind() != io::ErrorKind::NotFound {
                return Err(os_err(&e, path));
            }
        }
    }
    for sub in subdirs.iter().rev() {
        rmtree_walk(sub, false)?;
    }
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e) => fail(e),
    }
}

/// shutil.move
pub fn shutil_move(src: &str, dst: &str) -> Result<(), String> {
    let mut real_dst = dst.to_string();
    if isdir(dst) {
        // python: `_samefile(src, dst) and not os.path.islink(src)`; a link to dst is moved into dst
        if samefile(src, dst) && !Path::new(src).is_symlink() {
            // python renames (no-op move onto itself)
            return std::fs::rename(src, dst).map_err(|e| os_err2(&e, src, dst));
        }
        real_dst = join(dst, basename(src.trim_end_matches('/')));
        // python os.path.exists: a dangling symlink at the destination does not count
        if exists(&real_dst) {
            return Err(format!("Destination path '{}' already exists", real_dst));
        }
    }
    match std::fs::rename(src, &real_dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            let src_path = Path::new(src);
            if src_path.is_symlink() {
                let target = std::fs::read_link(src).map_err(|e| os_err(&e, src))?;
                let target_str = target.to_string_lossy().to_string();
                std::os::unix::fs::symlink(&target, &real_dst).map_err(|e| os_err2(&e, &target_str, &real_dst))?;
                std::fs::remove_file(src).map_err(|e| os_err(&e, src))
            } else if src_path.is_dir() {
                // shutil._destinsrc(src, dst): both sides get a trailing sep, then startswith
                let with_sep = |p: String| if p.ends_with('/') { p } else { format!("{}/", p) };
                if with_sep(abspath(dst)).starts_with(&with_sep(abspath(src))) {
                    return Err(format!("Cannot move a directory '{}' into itself '{}'.", src, dst));
                }
                // python: copytree(..., symlinks=True) keeps links as links
                copytree_with(src, &real_dst, true)?;
                rmtree(src)
            } else {
                copy2(src, &real_dst)?;
                std::fs::remove_file(src).map_err(|e| os_err(&e, src))
            }
        }
    }
}

/// os.makedirs(path, exist_ok). Mirrors CPython's recursion: the error names the path of the
/// mkdir that failed, so `file/sub/x` with `file` a regular file reports `file/sub` (ENOTDIR).
pub fn makedirs(p: &str, exist_ok: bool) -> Result<(), String> {
    makedirs_at(p, exist_ok).map_err(|(e, path)| os_err(&e, &path))
}

/// returns the io error together with the path that the failing mkdir was called with
fn makedirs_at(name: &str, exist_ok: bool) -> Result<(), (io::Error, String)> {
    let (mut head, mut tail) = split(name);
    if tail.is_empty() {
        (head, tail) = split(&head);
    }
    if !head.is_empty() && !tail.is_empty() && !exists(&head) {
        match makedirs_at(&head, exist_ok) {
            // python ignores a FileExistsError while creating the parent (race defeat)
            Err((e, _)) if e.kind() == io::ErrorKind::AlreadyExists => {}
            other => other?,
        }
        // xxx/newdir/. exists if xxx/newdir exists
        if tail == "." {
            return Ok(());
        }
    }
    match std::fs::create_dir(name) {
        Ok(()) => Ok(()),
        Err(e) if !exist_ok || !isdir(name) => Err((e, name.to_string())),
        Err(_) => Ok(()),
    }
}

/// open(path, 'x').write(content)
pub fn create_new(p: &str, content: &[u8]) -> Result<(), String> {
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(p).map_err(|e| os_err(&e, p))?;
    f.write_all(content).map_err(|e| os_err(&e, p))
}

/// open(path, 'w').write(content)
pub fn write_file(p: &str, content: &[u8]) -> Result<(), String> {
    std::fs::write(p, content).map_err(|e| os_err(&e, p))
}

pub fn path_buf(p: &str) -> PathBuf {
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers() {
        assert_eq!(split("/a/b/c"), ("/a/b".into(), "c".into()));
        assert_eq!(split("/a/b/"), ("/a/b".into(), "".into()));
        assert_eq!(split("/"), ("/".into(), "".into()));
        assert_eq!(dirname("a"), "");
        assert_eq!(splitext("a/b.tar.gz"), ("a/b.tar".into(), ".gz".into()));
        assert_eq!(splitext(".bashrc"), (".bashrc".into(), "".into()));
        assert_eq!(normpath("a/../../b/./c//"), "../b/c");
        assert_eq!(normpath("/../a"), "/a");
        assert_eq!(relpath("/a/b/c", "/a"), "b/c");
        assert_eq!(relpath("/a/", "/a"), ".");
        assert_eq!(relpath("/x/y", "/a/b"), "../../x/y");
        assert_eq!(join("/a", "/b"), "/b");
        assert_eq!(join("/a/", "b"), "/a/b");
        assert_eq!(strerror(2), "No such file or directory");
    }

    #[test]
    fn fnmatch_works() {
        assert!(fnmatch("a.py", "*.py"));
        assert!(!fnmatch("a.pyc", "*.py"));
        assert!(fnmatch("ab", "a?"));
        assert!(fnmatch("a1", "a[0-9]"));
        assert!(!fnmatch("a1", "a[!0-9]"));
        assert!(fnmatch("[x", "[x"));
    }

    #[test]
    fn fnmatch_reversed_ranges_match_cpython() {
        // expected matches taken from CPython 3.14 fnmatch.fnmatch over the same names
        let names = ["x", "zx", "ax", "a", "b", "c", "d", "e", "-", "0", "1", "z", "]", "!", "\\", "\\x", "bx", "cx", "ex"];
        let cases: &[(&str, &[&str])] = &[
            ("[z-a]x", &[]),
            ("[!z-a]x", &["zx", "ax", "\\x", "bx", "cx", "ex"]),
            ("[a-c-e]", &["a", "b", "c", "e", "-"]),
            ("[z-ab-c]", &["b", "c"]),
            ("[a-]", &["a", "-"]),
            ("[]-a]", &["a", "]"]),
            ("[!-a]", &["x", "b", "c", "d", "e", "0", "1", "z", "]", "!", "\\"]),
            ("[\\-a]", &["a", "]", "\\"]),
            ("[a-c]", &["a", "b", "c"]),
            ("[--0]", &["-", "0"]),
            ("[c-a-]", &["-"]),
            ("[ab-]", &["a", "b", "-"]),
        ];
        for (pat, expect) in cases {
            let got: Vec<&str> = names.iter().copied().filter(|n| fnmatch(n, pat)).collect();
            assert_eq!(&got[..], *expect, "pattern {}", pat);
        }
    }

    #[test]
    fn glob_works() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        std::fs::create_dir_all(format!("{}/src/sub", root)).unwrap();
        std::fs::create_dir_all(format!("{}/.hidden", root)).unwrap();
        std::fs::write(format!("{}/a.py", root), "").unwrap();
        std::fs::write(format!("{}/src/b.py", root), "").unwrap();
        std::fs::write(format!("{}/src/sub/c.py", root), "").unwrap();
        std::fs::write(format!("{}/.hidden/d.py", root), "").unwrap();
        let mut r: Vec<String> = glob(&join(&root, "**/*.py"), true).iter().map(|m| relpath(m, &root)).collect();
        r.sort();
        assert_eq!(r, vec!["a.py", "src/b.py", "src/sub/c.py"]);
        let mut r: Vec<String> = glob(&join(&root, "**"), true).iter().map(|m| relpath(m, &root)).collect();
        r.sort();
        assert_eq!(r, vec![".", "a.py", "src", "src/b.py", "src/sub", "src/sub/c.py"]);
        let r: Vec<String> = glob(&join(&root, "*.py"), true).iter().map(|m| relpath(m, &root)).collect();
        assert_eq!(r, vec!["a.py"]);
        let r: Vec<String> = glob(&join(&root, "src/b.py"), true).iter().map(|m| relpath(m, &root)).collect();
        assert_eq!(r, vec!["src/b.py"]);
    }

    #[test]
    fn glob_recursive_trailing_slash_drops_leading_empty() {
        // python: glob('**/', recursive=True) never yields the '' for the current directory
        let r = glob("**/", true);
        assert!(r.iter().all(|s| !s.is_empty()), "{:?}", r);
        assert!(r.contains(&"src/".to_string()), "{:?}", r);
    }

    #[test]
    fn python_error_strings() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        let missing = join(&root, "nope.txt");
        assert_eq!(read_text(&missing).unwrap_err(), format!("[Errno 2] No such file or directory: {}", py_quote(&missing)));
        assert_eq!(copy2(&missing, &join(&root, "x")).unwrap_err(), format!("[Errno 2] No such file or directory: {}", py_quote(&missing)));
        assert_eq!(listdir("").unwrap_err(), "[Errno 2] No such file or directory: ''");
        makedirs(&root, true).unwrap();
        assert_eq!(makedirs(&root, false).unwrap_err(), format!("[Errno 17] File exists: {}", py_quote(&root)));
        let f = join(&root, "a.txt");
        create_new(&f, b"hi").unwrap();
        assert_eq!(create_new(&f, b"x").unwrap_err(), format!("[Errno 17] File exists: {}", py_quote(&f)));
        assert_eq!(read_text(&f).unwrap(), "hi");
    }

    #[test]
    fn move_into_self_and_fnmatch_caret() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        let src = join(&root, "dir");
        makedirs(&join(&src, "inner"), false).unwrap();
        let dst = join(&src, "inner");
        assert_eq!(
            shutil_move(&src, &dst).unwrap_err(),
            format!("Cannot move a directory '{}' into itself '{}'.", src, dst)
        );
        // only '!' negates in fnmatch; '^' is a literal inside brackets
        assert!(fnmatch("^", "[^a]"));
        assert!(fnmatch("a", "[^a]"));
        assert!(!fnmatch("b", "[^a]"));
        assert!(fnmatch("b", "[!a]"));
    }

    #[test]
    fn decode_errors() {
        assert_eq!(decode_utf8(b"ab\xff").unwrap_err(), "'utf-8' codec can't decode byte 0xff in position 2: invalid start byte");
        assert_eq!(universal_newlines("a\r\nb\rc"), "a\nb\nc");
        assert_eq!(readlines("a\nb"), vec!["a\n", "b"]);
    }

    #[test]
    fn decode_error_spans_match_python() {
        // a multi-byte bad sequence is reported as a range, like python's UnicodeDecodeError
        assert_eq!(
            decode_utf8(b"a\xe4\xb8").unwrap_err(),
            "'utf-8' codec can't decode bytes in position 1-2: unexpected end of data"
        );
        assert_eq!(
            decode_utf8(b"\xe4\xb8A").unwrap_err(),
            "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte"
        );
        assert_eq!(decode_utf8(b"ab\xe2\x82(").unwrap_err(), "'utf-8' codec can't decode bytes in position 2-3: invalid continuation byte");
    }

    #[test]
    fn py_repr_escapes_nonprintable() {
        assert_eq!(py_quote("a\u{200b}b"), "'a\\u200bb'");
        assert_eq!(py_quote("x\u{85}y"), "'x\\x85y'");
        assert_eq!(py_quote("it's"), "\"it's\"");
        assert_eq!(py_quote("é"), "'é'");
    }

    #[test]
    fn rstrip_strips_python_whitespace_controls() {
        assert_eq!(rstrip("ab\u{1c} "), "ab");
        assert_eq!(rstrip("ab\u{1f}"), "ab");
        assert_eq!(rstrip("ab\u{a0}"), "ab");
    }

    #[test]
    fn stream_lines_keeps_lines_before_a_bad_chunk() {
        let d = tempfile::tempdir().unwrap();
        let f = join(&d.path().to_string_lossy(), "big.txt");
        // the first 8 KiB decode fine; the bad byte sits in the second chunk
        let mut data = b"hello\r\n".to_vec();
        while data.len() < 9000 {
            data.extend_from_slice(b"a\n");
        }
        data.push(0xff);
        std::fs::write(&f, &data).unwrap();
        let (lines, err) = stream_lines(&f).unwrap();
        assert_eq!(lines[0], "hello\n");
        assert!(lines.len() > 1);
        let msg = err.unwrap();
        assert!(msg.starts_with("'utf-8' codec can't decode byte 0xff in position "), "{}", msg);
        assert!(msg.ends_with(": invalid start byte"), "{}", msg);
        std::fs::write(&f, b"a\r\nb").unwrap();
        assert_eq!(stream_lines(&f).unwrap(), (vec!["a\n".to_string(), "b".to_string()], None));
    }

    #[test]
    fn stream_lines_lone_cr_around_a_bad_chunk_matches_python() {
        // python 3.14 `for line in open(p, encoding="utf-8")` on these files
        let d = tempfile::tempdir().unwrap();
        let f = join(&d.path().to_string_lossy(), "cr.txt");
        // a bad chunk is dropped whole, lone CR included: python yields no lines from it
        std::fs::write(&f, b"x\na\rb\r\xff").unwrap();
        let (lines, err) = stream_lines(&f).unwrap();
        assert!(lines.is_empty());
        assert_eq!(err.as_deref(), Some("'utf-8' codec can't decode byte 0xff in position 6: invalid start byte"));
        // a CR ending the last good 8 KiB chunk is held back (python gives 4095 lines, not 4096)
        let mut data = (b"a\rb\r".to_vec().into_iter().chain(b"c\n".repeat(5000))).take(8191).collect::<Vec<u8>>();
        data.push(b'\r');
        data.push(0xff);
        std::fs::write(&f, &data).unwrap();
        let (lines, err) = stream_lines(&f).unwrap();
        assert_eq!(lines.len(), 4095);
        assert_eq!(&lines[..2], &["a\n".to_string(), "b\n".to_string()]);
        assert_eq!(lines.last().map(String::as_str), Some("c\n"));
        assert_eq!(err.as_deref(), Some("'utf-8' codec can't decode byte 0xff in position 0: invalid start byte"));
    }

    #[test]
    fn copy2_same_file_and_dir_source_errors() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        let f = join(&root, "keep.txt");
        write_file(&f, b"precious").unwrap();
        assert_eq!(copy2(&f, &f).unwrap_err(), format!("{} and {} are the same file", py_quote(&f), py_quote(&f)));
        assert_eq!(read_text(&f).unwrap(), "precious");
        let dir = join(&root, "sub");
        makedirs(&dir, false).unwrap();
        let target = join(&root, "copy.txt");
        assert_eq!(copy2(&dir, &target).unwrap_err(), format!("[Errno 21] Is a directory: {}", py_quote(&dir)));
        // a failure creating the destination is reported against the destination
        let nested = join(&root, "nope/x.txt");
        assert_eq!(copy2(&f, &nested).unwrap_err(), format!("[Errno 2] No such file or directory: {}", py_quote(&nested)));
    }

    #[test]
    fn copytree_collects_errors_and_keeps_copying() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        let src = join(&root, "src");
        let dst = join(&root, "dst");
        makedirs(&join(&src, "sub"), false).unwrap();
        write_file(&join(&src, "good.txt"), b"g").unwrap();
        write_file(&join(&src, "sub/inner.txt"), b"i").unwrap();
        // a dangling link cannot be copied as file content, so it fails
        std::os::unix::fs::symlink(join(&root, "missing"), join(&src, "bad")).unwrap();
        std::fs::set_permissions(join(&src, "sub"), std::fs::Permissions::from_mode(0o750)).unwrap();
        let err = copytree(&src, &dst).unwrap_err();
        let bad_src = join(&src, "bad");
        let bad_dst = join(&dst, "bad");
        let msg = format!("[Errno 2] No such file or directory: {}", py_quote(&bad_src));
        assert_eq!(err, format!("[({}, {}, {})]", py_quote(&bad_src), py_quote(&bad_dst), py_quote(&msg)));
        assert_eq!(read_text(&join(&dst, "good.txt")).unwrap(), "g");
        assert_eq!(read_text(&join(&dst, "sub/inner.txt")).unwrap(), "i");
        let mode = std::fs::metadata(join(&dst, "sub")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750);
    }

    #[test]
    fn copytree_symlinks_mode_recreates_links() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        let src = join(&root, "src");
        makedirs(&src, false).unwrap();
        write_file(&join(&src, "real.txt"), b"r").unwrap();
        std::os::unix::fs::symlink("real.txt", join(&src, "link")).unwrap();
        let kept = join(&root, "kept");
        copytree_with(&src, &kept, true).unwrap();
        assert_eq!(std::fs::read_link(join(&kept, "link")).unwrap(), Path::new("real.txt"));
        let followed = join(&root, "followed");
        copytree(&src, &followed).unwrap();
        assert!(!std::fs::symlink_metadata(join(&followed, "link")).unwrap().file_type().is_symlink());
        assert_eq!(read_text(&join(&followed, "link")).unwrap(), "r");
    }

    #[test]
    fn rmtree_refuses_symlink_with_python_message() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        let real = join(&root, "real");
        makedirs(&real, false).unwrap();
        let link = join(&root, "link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(rmtree(&link).unwrap_err(), format!("[Errno None] None: {}", py_quote(&link)));
        assert!(isdir(&real));
    }

    #[test]
    fn rmtree_unlinks_symlink_inside_tree_and_keeps_target() {
        // python: a symlink found while walking is unlinked, never followed
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        let real = join(&root, "real");
        makedirs(&real, false).unwrap();
        std::fs::write(join(&real, "keep"), b"").unwrap();
        let tree = join(&root, "tree");
        makedirs(&tree, false).unwrap();
        std::os::unix::fs::symlink(&real, join(&tree, "link")).unwrap();
        rmtree(&tree).unwrap();
        assert!(!lexists(&tree));
        assert!(exists(&join(&real, "keep")));
    }

    #[test]
    fn rmtree_error_names_the_directory_that_failed() {
        // python 3.14 on linux: EACCES unlinking 'f' inside 'sub' is raised with filename 'sub'
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        let sub = join(&root, "sub");
        makedirs(&sub, false).unwrap();
        std::fs::write(join(&sub, "f"), b"").unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();
        let res = rmtree(&root);
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(res.unwrap_err(), format!("[Errno 13] Permission denied: {}", py_quote(&sub)));
    }

    #[test]
    fn makedirs_error_names_the_failing_ancestor() {
        // python: os.makedirs('file/sub/x') reports the mkdir of 'file/sub', not the leaf
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        let file = join(&root, "file");
        std::fs::write(&file, b"").unwrap();
        let deep = join(&file, "sub/x");
        assert_eq!(makedirs(&deep, true).unwrap_err(), format!("[Errno 20] Not a directory: {}", py_quote(&join(&file, "sub"))));
        let shallow = join(&file, "x");
        assert_eq!(makedirs(&shallow, true).unwrap_err(), format!("[Errno 20] Not a directory: {}", py_quote(&shallow)));
        // exist_ok=False on an existing leaf is EEXIST; exist_ok=True on a dir is fine
        let dir = join(&root, "dir");
        makedirs(&dir, false).unwrap();
        assert_eq!(makedirs(&dir, false).unwrap_err(), format!("[Errno 17] File exists: {}", py_quote(&dir)));
        makedirs(&dir, true).unwrap();
        makedirs(&join(&dir, "a/b/."), false).unwrap();
        assert!(isdir(&join(&dir, "a/b")));
    }

    #[test]
    fn which_in_matches_python_path_rules() {
        // python 3.14 shutil.which: an empty PATH returns None; an empty entry searches the cwd
        // as a bare name (os.path.join('', cmd) == cmd)
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        let tool = join(&root, "pyos-which-tool");
        std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(which_in("pyos-which-tool", ""), None);
        assert_eq!(which_in("pyos-which-tool", &root), Some(tool.clone()));
        assert_eq!(which_in("pyos-which-tool", "/nonexistent-dir"), None);
        // a path with a slash bypasses PATH and checks the path itself
        assert_eq!(which("/nonexistent-dir/tool"), None);
    }

    #[test]
    fn move_link_into_its_own_target_dir_moves_the_link() {
        // python: shutil.move(link, dir) with link -> dir skips the rename-onto-self branch
        // (`_samefile and not islink`) and moves the link to dir/basename(link)
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().to_string();
        let dir = join(&root, "D");
        makedirs(&dir, false).unwrap();
        let link = join(&root, "L");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        shutil_move(&link, &dir).unwrap();
        assert!(Path::new(&join(&dir, "L")).is_symlink());
        assert!(!lexists(&link));
    }
}
