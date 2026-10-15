//! Small re-implementations of the python `os`, `os.path`, `glob`, `fnmatch` and `shutil`
//! behaviour the dev modules rely on, including python-style error messages
//! (`[Errno 2] No such file or directory: '/x'`).

use std::io;
use std::ops::ControlFlow;
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
    ol_core::pyjson::py_repr(&serde_json::Value::String(s.to_string()))
}

/// python `open(path, 'r', encoding='utf-8').read()`: strict utf-8 decoding plus universal
/// newline translation. Errors are python-formatted.
pub fn read_text(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| os_err(&e, path))?;
    decode_utf8(&bytes).map(|s| universal_newlines(&s))
}

/// strict utf-8 decode with python's UnicodeDecodeError message
pub fn decode_utf8(bytes: &[u8]) -> Result<String, String> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_string()),
        Err(e) => {
            let pos = e.valid_up_to();
            let b = bytes[pos];
            let reason = match e.error_len() {
                None => "unexpected end of data",
                Some(_) if (0x80..0xc2).contains(&b) || b >= 0xf5 => "invalid start byte",
                Some(_) => "invalid continuation byte",
            };
            Err(format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: {}", b, pos, reason))
        }
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

/// python `str.splitlines(keepends=True)` restricted to text-mode lines (`f.readlines()`)
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

/// python `str.rstrip()` (whitespace)
pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(|c: char| c.is_whitespace())
}

/// os.path.expanduser
pub fn expanduser(p: &str) -> String {
    if !p.starts_with('~') {
        return p.to_string();
    }
    let rest_start = p.find('/').unwrap_or(p.len());
    let user = &p[1..rest_start];
    let home = if user.is_empty() {
        std::env::var("HOME").ok()
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
            let h = if h.len() > 1 { h.trim_end_matches('/').to_string() } else { h };
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

/// os.listdir (python error messages)
pub fn listdir(p: &str) -> Result<Vec<String>, String> {
    let rd = std::fs::read_dir(if p.is_empty() { "." } else { p }).map_err(|e| os_err(&e, p))?;
    Ok(rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
}

/// shutil.which
pub fn which(cmd: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let check = |p: &Path| p.is_file() && p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false);
    if cmd.contains('/') {
        return check(Path::new(cmd)).then(|| cmd.to_string());
    }
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let candidate = Path::new(dir).join(cmd);
        if check(&candidate) {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

/// os.walk (top-down, followlinks=False). `prune` receives the subdirectory names of each
/// directory and can filter them in place (python: `folders[:] = [...]`). The visitor gets
/// (dirpath, dirnames, filenames) and may break out early.
pub fn walk<B>(
    top: &str,
    visit: &mut dyn FnMut(&str, &mut Vec<String>, &[String]) -> ControlFlow<B>,
) -> Option<B> {
    let entries = match std::fs::read_dir(top) {
        Ok(rd) => rd,
        Err(_) => return None,
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut symlinked_dirs = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let path = e.path();
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
                    let mut stuff: String = chars[i..j].iter().collect();
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
                    stuff = stuff.replace('\\', "\\\\");
                    let negate = stuff.starts_with('!');
                    if negate {
                        stuff = stuff[1..].to_string();
                    }
                    // escape regex set operators the regex crate would interpret
                    let mut esc = String::new();
                    for ch in stuff.chars() {
                        if matches!(ch, '[' | '&' | '~' | '|') {
                            esc.push('\\');
                        }
                        esc.push(ch);
                    }
                    res.push('[');
                    if negate {
                        res.push('^');
                    }
                    res.push_str(&esc);
                    res.push(']');
                }
            }
            other => res.push_str(&regex::escape(&other.to_string())),
        }
    }
    res.push('$');
    res
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
    match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| !dironly || e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
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

fn rlistdir(dirname: &str, dironly: bool, out: &mut Vec<String>, depth: usize) {
    if depth > 64 {
        return;
    }
    for x in glob_listdir(dirname, dironly) {
        if is_hidden(&x) {
            continue;
        }
        out.push(x.clone());
        let path = if dirname.is_empty() { x.clone() } else { join(dirname, &x) };
        let mut sub = Vec::new();
        rlistdir(&path, dironly, &mut sub, depth + 1);
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
    rlistdir(dirname, dironly, &mut out, 0);
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
    if recursive && pathname == "**" && r.first().map(|s| s.is_empty()).unwrap_or(false) {
        r.remove(0);
    }
    r
}

// ---------------------------------------------------------------- shutil

fn copy_file_with_meta(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::copy(src, dst)?;
    if let Ok(meta) = std::fs::metadata(src) {
        if let Ok(f) = std::fs::File::options().write(true).open(dst) {
            let mut times = std::fs::FileTimes::new();
            if let Ok(m) = meta.modified() {
                times = times.set_modified(m);
            }
            if let Ok(a) = meta.accessed() {
                times = times.set_accessed(a);
            }
            let _ = f.set_times(times);
        }
    }
    Ok(())
}

/// shutil.copy2 (dst may be a directory)
pub fn copy2(src: &str, dst: &str) -> Result<(), String> {
    let real_dst = if isdir(dst) { join(dst, basename(src)) } else { dst.to_string() };
    if !exists(src) {
        return Err(format!("[Errno 2] No such file or directory: {}", py_quote(src)));
    }
    copy_file_with_meta(Path::new(src), Path::new(&real_dst)).map_err(|e| os_err(&e, &real_dst))
}

/// shutil.copytree (dst must not exist)
pub fn copytree(src: &str, dst: &str) -> Result<(), String> {
    let entries = std::fs::read_dir(src).map_err(|e| os_err(&e, src))?;
    makedirs(dst, false)?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let s = join(src, &name);
        let d = join(dst, &name);
        if Path::new(&s).is_dir() {
            copytree(&s, &d)?;
        } else {
            copy_file_with_meta(Path::new(&s), Path::new(&d)).map_err(|e| os_err(&e, &s))?;
        }
    }
    Ok(())
}

/// shutil.rmtree
pub fn rmtree(p: &str) -> Result<(), String> {
    std::fs::remove_dir_all(p).map_err(|e| os_err(&e, p))
}

/// shutil.move
pub fn shutil_move(src: &str, dst: &str) -> Result<(), String> {
    let mut real_dst = dst.to_string();
    if isdir(dst) {
        if same_path(src, dst) {
            // python renames (no-op move onto itself)
            return std::fs::rename(src, dst).map_err(|e| os_err2(&e, src, dst));
        }
        real_dst = join(dst, basename(src.trim_end_matches('/')));
        if lexists(&real_dst) {
            return Err(format!("Destination path '{}' already exists", real_dst));
        }
    }
    match std::fs::rename(src, &real_dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            let src_path = Path::new(src);
            if src_path.is_symlink() {
                let target = std::fs::read_link(src).map_err(|e| os_err(&e, src))?;
                std::os::unix::fs::symlink(target, &real_dst).map_err(|e| os_err(&e, &real_dst))?;
                std::fs::remove_file(src).map_err(|e| os_err(&e, src))
            } else if src_path.is_dir() {
                let a = abspath(src);
                let b = abspath(&real_dst);
                if b.starts_with(&format!("{}/", a.trim_end_matches('/'))) {
                    return Err(format!("Cannot move a directory '{}' into itself '{}'.", src, real_dst));
                }
                copytree(src, &real_dst)?;
                rmtree(src)
            } else {
                if !exists(src) {
                    return Err(format!("[Errno 2] No such file or directory: {}", py_quote(src)));
                }
                copy_file_with_meta(src_path, Path::new(&real_dst)).map_err(|e| os_err(&e, &real_dst))?;
                std::fs::remove_file(src).map_err(|e| os_err(&e, src))
            }
        }
    }
}

fn same_path(a: &str, b: &str) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// os.makedirs(path, exist_ok)
pub fn makedirs(p: &str, exist_ok: bool) -> Result<(), String> {
    if !exist_ok && lexists(p) {
        return Err(format!("[Errno 17] File exists: {}", py_quote(p)));
    }
    std::fs::create_dir_all(p).map_err(|e| os_err(&e, p))
}

/// open(path, 'x').write(content)
pub fn create_new(p: &str, content: &[u8]) -> Result<(), String> {
    use std::io::Write;
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
    fn decode_errors() {
        assert_eq!(decode_utf8(b"ab\xff").unwrap_err(), "'utf-8' codec can't decode byte 0xff in position 2: invalid start byte");
        assert_eq!(universal_newlines("a\r\nb\rc"), "a\nb\nc");
        assert_eq!(readlines("a\nb"), vec!["a\n", "b"]);
    }
}
