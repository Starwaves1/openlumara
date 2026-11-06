//! Port of modules/file_manager/module.py: the `FileManager` module, which gives the AI full
//! access to the filesystem (list, read, read_binary, write, append, mkdir, remove, move, copy,
//! get_info) and can put system information into the end-of-context prompt.
//!
//! Python's `os`, `pathlib` and `shutil` behaviour (path normalisation, `str(OSError)` messages,
//! `shutil.move` / `shutil.copytree` / `shutil.copy2` semantics) lives in `util.rs`.

#[path = "../../../generated/modules/file_manager.rs"]
pub mod meta;

use crate::util::{
    os_err, os_err2, os_err_bare, py_abspath, py_basename, py_join, py_name, py_absolute, py_parent, py_path, py_type_name,
    same_file, universal_newlines,
};
use anyhow::anyhow;
use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use serde_json::{json, Map, Value};
use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::sync::Arc;

pub struct FileManager {
    base: ModuleBase,
}

/// python `_verify_path`: `None` when the path passes, otherwise the failure reason
fn verify_path(path: &str, should_be_dir: bool, should_be_file: bool) -> Option<&'static str> {
    let p = py_path(path);
    let p = Path::new(&p);
    if !p.exists() {
        return Some("path does not exist");
    }
    if should_be_dir && !p.is_dir() {
        return Some("target path is not a directory");
    }
    if should_be_file && !p.is_file() {
        return Some("target path is not a file");
    }
    None
}

/// the two ways `open(path, 'r', encoding='utf-8').read()` can fail in python
enum ReadErr {
    /// UnicodeDecodeError
    NotText,
    /// any OSError, already formatted as `str(e)`
    Io(String),
}

fn read_text(path: &str) -> Result<String, ReadErr> {
    let bytes = fs::read(path).map_err(|e| ReadErr::Io(os_err(&e, path)))?;
    let text = String::from_utf8(bytes).map_err(|_| ReadErr::NotText)?;
    Ok(universal_newlines(&text))
}

/// python `shutil.copystat(src, dst, follow_symlinks=follow)`: atime and mtime, then extended
/// attributes, then mode (a followed link only, since Linux cannot chmod a symlink)
fn copystat(src: &str, dst: &str, follow: bool) -> Result<(), String> {
    let md = if follow { fs::metadata(src) } else { fs::symlink_metadata(src) }.map_err(|e| os_err(&e, src))?;
    set_times(dst, &md, follow)?;
    copy_xattrs(src, dst, follow)?;
    if follow {
        fs::set_permissions(dst, md.permissions()).map_err(|e| os_err(&e, dst))?;
    }
    Ok(())
}

fn set_times(dst: &str, md: &fs::Metadata, follow: bool) -> Result<(), String> {
    let c = CString::new(dst).map_err(|_| format!("embedded null byte in path {}", crate::util::py_quote(dst)))?;
    let times = [
        libc::timespec { tv_sec: md.atime() as libc::time_t, tv_nsec: md.atime_nsec() as libc::c_long },
        libc::timespec { tv_sec: md.mtime() as libc::time_t, tv_nsec: md.mtime_nsec() as libc::c_long },
    ];
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    // SAFETY: `c` is a valid NUL-terminated string and `times` has the two timespecs utimensat reads
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), flags) };
    if rc != 0 {
        return Err(os_err(&io::Error::last_os_error(), dst));
    }
    Ok(())
}

/// errnos python's `shutil._copyxattr` ignores on each attribute get/set (other errors propagate)
const XATTR_SKIP_ATTR: [i32; 5] = [libc::EPERM, libc::ENOTSUP, libc::ENODATA, libc::EINVAL, libc::EACCES];
/// errnos python's `shutil._copyxattr` ignores when the attribute list itself cannot be read
const XATTR_SKIP_LIST: [i32; 3] = [libc::ENOTSUP, libc::ENODATA, libc::EINVAL];

fn errno_in(e: &io::Error, list: &[i32]) -> bool {
    e.raw_os_error().is_some_and(|code| list.contains(&code))
}

fn list_xattrs(path: &CStr, follow: bool) -> io::Result<Vec<CString>> {
    loop {
        // SAFETY: a null buffer with size 0 only asks the kernel for the required size
        let size = unsafe {
            if follow {
                libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0)
            } else {
                libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0)
            }
        };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` is valid for `buf.len()` bytes, which is what the size argument states
        let n = unsafe {
            if follow {
                libc::listxattr(path.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len())
            } else {
                libc::llistxattr(path.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len())
            }
        };
        if n >= 0 {
            buf.truncate(n as usize);
            // names are NUL-terminated and never contain NUL themselves
            return Ok(buf
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .filter_map(|s| CString::new(s).ok())
                .collect());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ERANGE) {
            return Err(err);
        }
    }
}

fn get_xattr(path: &CStr, name: &CStr, follow: bool) -> io::Result<Vec<u8>> {
    loop {
        // SAFETY: a null value with size 0 only asks the kernel for the required size
        let size = unsafe {
            if follow {
                libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0)
            } else {
                libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0)
            }
        };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` is valid for `buf.len()` bytes, which is what the size argument states
        let n = unsafe {
            if follow {
                libc::getxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            } else {
                libc::lgetxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            }
        };
        if n >= 0 {
            buf.truncate(n as usize);
            return Ok(buf);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ERANGE) {
            return Err(err);
        }
    }
}

fn set_xattr(path: &CStr, name: &CStr, value: &[u8], follow: bool) -> io::Result<()> {
    // SAFETY: `value` is valid for `value.len()` bytes and the name/path are NUL-terminated
    let rc = unsafe {
        if follow {
            libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr() as *const libc::c_void, value.len(), 0)
        } else {
            libc::lsetxattr(path.as_ptr(), name.as_ptr(), value.as_ptr() as *const libc::c_void, value.len(), 0)
        }
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// python `shutil._copyxattr`: copy every extended attribute, skipping the ones it cannot set
fn copy_xattrs(src: &str, dst: &str, follow: bool) -> Result<(), String> {
    let cs = CString::new(src).map_err(|_| format!("embedded null byte in path {}", crate::util::py_quote(src)))?;
    let cd = CString::new(dst).map_err(|_| format!("embedded null byte in path {}", crate::util::py_quote(dst)))?;
    let names = match list_xattrs(&cs, follow) {
        Ok(names) => names,
        Err(e) if errno_in(&e, &XATTR_SKIP_LIST) => return Ok(()),
        Err(e) => return Err(os_err(&e, src)),
    };
    for name in names {
        let value = match get_xattr(&cs, &name, follow) {
            Ok(v) => v,
            Err(e) if errno_in(&e, &XATTR_SKIP_ATTR) => continue,
            Err(e) => return Err(os_err(&e, src)),
        };
        match set_xattr(&cd, &name, &value, follow) {
            Ok(()) => {}
            Err(e) if errno_in(&e, &XATTR_SKIP_ATTR) => {}
            Err(e) => return Err(os_err(&e, dst)),
        }
    }
    Ok(())
}

/// python `shutil.copy2(src, dst)`: a directory `dst` receives `src` by its basename
fn copy2(src: &str, dst: &str) -> Result<(), String> {
    let real_dst = if Path::new(dst).is_dir() { py_join(dst, &py_basename(src)) } else { dst.to_string() };
    if same_file(src, &real_dst) {
        return Err(format!("{} and {} are the same file", crate::util::py_quote(src), crate::util::py_quote(&real_dst)));
    }
    for name in [src, real_dst.as_str()] {
        if let Ok(m) = fs::metadata(name) {
            if m.file_type().is_fifo() {
                return Err(format!("`{}` is a named pipe", name));
            }
        }
    }
    // python opens src first, then dst with 'wb' (truncating)
    let mut input = File::open(src).map_err(|e| os_err(&e, src))?;
    let mut output = File::create(&real_dst).map_err(|e| os_err(&e, &real_dst))?;
    io::copy(&mut input, &mut output).map_err(|e| os_err_bare(&e))?;
    copystat(src, &real_dst, true)
}

/// `(src, dst, message)` triples python's `shutil.copytree` collects into `shutil.Error`
type CopyErrors = Vec<(String, String, String)>;

/// how a `_copytree` step fails: a plain OSError (raised as-is by the caller's loop) or the
/// aggregated `shutil.Error` list from a nested copytree
enum CopyErr {
    Os(String),
    Tree(CopyErrors),
}

impl CopyErr {
    /// `str(e)` of the exception python raises to the tool: the message, or the repr of the list
    fn into_message(self) -> String {
        match self {
            CopyErr::Os(m) => m,
            CopyErr::Tree(errors) => {
                let items: Vec<String> = errors
                    .iter()
                    .map(|(s, d, m)| format!("({}, {}, {})", crate::util::py_quote(s), crate::util::py_quote(d), crate::util::py_quote(m)))
                    .collect();
                format!("[{}]", items.join(", "))
            }
        }
    }
}

/// python `shutil.copytree(src, dst, symlinks=...)` with the default copy2 copy function. A failing
/// entry is recorded and the rest of the tree is still copied; the collected errors are raised together
fn copytree(src: &str, dst: &str, symlinks: bool) -> Result<(), String> {
    copytree_inner(src, dst, symlinks).map_err(CopyErr::into_message)
}

fn copytree_inner(src: &str, dst: &str, symlinks: bool) -> Result<(), CopyErr> {
    let entries: Vec<fs::DirEntry> = fs::read_dir(src)
        .map_err(|e| CopyErr::Os(os_err(&e, src)))?
        .collect::<Result<_, _>>()
        .map_err(|e| CopyErr::Os(os_err(&e, src)))?;
    fs::create_dir(dst).map_err(|e| CopyErr::Os(os_err(&e, dst)))?;
    let mut errors: CopyErrors = Vec::new();
    for ent in entries {
        let name = ent.file_name().to_string_lossy().into_owned();
        let srcname = py_join(src, &name);
        let dstname = py_join(dst, &name);
        if let Err(err) = copy_entry(&ent, &srcname, &dstname, symlinks) {
            match err {
                CopyErr::Os(m) => errors.push((srcname, dstname, m)),
                CopyErr::Tree(nested) => errors.extend(nested),
            }
        }
    }
    if let Err(m) = copystat(src, dst, true) {
        errors.push((src.to_string(), dst.to_string(), m));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(CopyErr::Tree(errors))
    }
}

/// one iteration of python `_copytree`'s loop body for a single directory entry
fn copy_entry(ent: &fs::DirEntry, srcname: &str, dstname: &str, symlinks: bool) -> Result<(), CopyErr> {
    let ft = ent.file_type().map_err(|e| CopyErr::Os(os_err(&e, srcname)))?;
    if ft.is_symlink() {
        if symlinks {
            let target = fs::read_link(srcname).map_err(|e| CopyErr::Os(os_err(&e, srcname)))?.to_string_lossy().into_owned();
            std::os::unix::fs::symlink(&target, dstname).map_err(|e| CopyErr::Os(os_err2(&e, &target, dstname)))?;
            copystat(srcname, dstname, false).map_err(CopyErr::Os)
        } else if Path::new(srcname).is_dir() {
            copytree_inner(srcname, dstname, symlinks)
        } else {
            copy2(srcname, dstname).map_err(CopyErr::Os)
        }
    } else if ft.is_dir() {
        copytree_inner(srcname, dstname, symlinks)
    } else {
        copy2(srcname, dstname).map_err(CopyErr::Os)
    }
}

/// python `shutil._destinsrc`
fn destinsrc(src: &str, dst: &str) -> bool {
    let mut s = py_abspath(src);
    let mut d = py_abspath(dst);
    if !s.ends_with('/') {
        s.push('/');
    }
    if !d.ends_with('/') {
        d.push('/');
    }
    d.starts_with(&s)
}

/// python `shutil.move(src, dst)` (rename first, copy + delete when rename fails)
fn shutil_move(src: &str, dst: &str) -> Result<(), String> {
    let mut real_dst = dst.to_string();
    if Path::new(dst).is_dir() {
        if same_file(src, dst) {
            return fs::rename(src, dst).map_err(|e| os_err2(&e, src, dst));
        }
        real_dst = py_join(dst, &py_basename(src));
        if Path::new(&real_dst).exists() {
            return Err(format!("Destination path '{}' already exists", real_dst));
        }
    }
    if fs::rename(src, &real_dst).is_ok() {
        return Ok(());
    }
    let is_link = fs::symlink_metadata(src).map(|m| m.file_type().is_symlink()).unwrap_or(false);
    if is_link {
        let linkto = fs::read_link(src).map_err(|e| os_err(&e, src))?.to_string_lossy().into_owned();
        std::os::unix::fs::symlink(&linkto, &real_dst).map_err(|e| os_err2(&e, &linkto, &real_dst))?;
        fs::remove_file(src).map_err(|e| os_err(&e, src))
    } else if Path::new(src).is_dir() {
        if destinsrc(src, dst) {
            return Err(format!("Cannot move a directory '{}' into itself '{}'.", src, dst));
        }
        copytree(src, &real_dst, true)?;
        fs::remove_dir_all(src).map_err(|e| os_err(&e, src))
    } else {
        copy2(src, &real_dst)?;
        fs::remove_file(src).map_err(|e| os_err(&e, src))
    }
}

fn move_inner(src: &str, dst: &str) -> Result<(), String> {
    let parent = py_parent(dst);
    fs::create_dir_all(&parent).map_err(|e| os_err(&e, &parent))?;
    shutil_move(src, dst)
}

fn copy_inner(src: &str, dst: &str) -> Result<(), String> {
    let parent = py_parent(dst);
    fs::create_dir_all(&parent).map_err(|e| os_err(&e, &parent))?;
    if Path::new(src).is_dir() {
        copytree(src, dst, false)
    } else {
        copy2(src, dst)
    }
}

/// python `Module._write` body: parent dirs, open (truncating for 'w'), then `f.write(content)`
fn write_inner(path: &str, content: &Value, mode: &str) -> Result<(), String> {
    let parent = py_parent(path);
    fs::create_dir_all(&parent).map_err(|e| os_err(&e, &parent))?;
    let mut f = if mode == "a" {
        OpenOptions::new().append(true).create(true).open(path)
    } else {
        OpenOptions::new().write(true).create(true).truncate(true).open(path)
    }
    .map_err(|e| os_err(&e, path))?;
    // the file is already opened (and truncated for 'w') when python's f.write() rejects the value
    let Value::String(text) = content else {
        return Err(format!("write() argument must be str, not {}", py_type_name(content)));
    };
    f.write_all(text.as_bytes()).map_err(|e| os_err_bare(&e))
}

/// `Timestamp` as python builds `st_mtime` / `st_ctime`: `sec + nsec * 1e-9`
fn ts(sec: i64, nsec: i64) -> f64 {
    sec as f64 + nsec as f64 * 1e-9
}

/// python `platform.node()` / `release()` / `machine()` and `system()`
#[derive(Default)]
struct Uname {
    sysname: String,
    nodename: String,
    release: String,
    machine: String,
}

fn cstr_field(p: *const libc::c_char) -> String {
    // SAFETY: utsname fields are NUL-terminated C strings inside the struct
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn uname() -> Uname {
    // SAFETY: utsname is plain old data; an all-zero value is valid before uname() fills it
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `u` is a valid, writable utsname
    if unsafe { libc::uname(&mut u) } != 0 {
        return Uname::default();
    }
    Uname {
        sysname: cstr_field(u.sysname.as_ptr()),
        nodename: cstr_field(u.nodename.as_ptr()),
        release: cstr_field(u.release.as_ptr()),
        machine: cstr_field(u.machine.as_ptr()),
    }
}

/// glibc version as python's `platform.libc_ver()` reports it (`2.43`), if this is a glibc system
fn glibc_version() -> Option<String> {
    let mut buf = [0 as libc::c_char; 64];
    // SAFETY: buf is valid for buf.len() bytes
    let n = unsafe { libc::confstr(libc::_CS_GNU_LIBC_VERSION, buf.as_mut_ptr(), buf.len()) };
    if n == 0 {
        return None;
    }
    cstr_field(buf.as_ptr()).strip_prefix("glibc ").map(|v| v.to_string())
}

/// python `platform.platform()` (non-terse, non-aliased), e.g. `Linux-6.8.0-x86_64-with-glibc2.39`
fn platform_string(u: &Uname) -> String {
    let base = format!("{}-{}-{}", u.sysname, u.release, u.machine);
    let s = match (u.sysname.as_str(), glibc_version()) {
        ("Linux", Some(v)) => format!("{}-with-glibc{}", base, v),
        _ => base,
    };
    s.replace(' ', "_")
}

/// python `sys.platform`
fn sys_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// python `os.path.expanduser("~")`
fn home_dir() -> String {
    let home = match std::env::var_os("HOME") {
        Some(h) => h.to_string_lossy().into_owned(),
        None => match passwd_home() {
            Some(h) => h,
            None => return "~".to_string(),
        },
    };
    let trimmed = home.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn passwd_home() -> Option<String> {
    // SAFETY: getpwuid returns either NULL or a pointer to static passwd data valid until the next call
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        Some(cstr_field((*pw).pw_dir))
    }
}

/// the `on_end_prompt` text: `key: value` lines (python's `details_string.strip()`)
pub fn system_info() -> String {
    let u = uname();
    let machine = if u.machine.is_empty() { "unknown".to_string() } else { u.machine.clone() };
    let details: [(&str, String); 6] = [
        ("OS", sys_platform().to_string()),
        ("OS release", u.release.clone()),
        ("platform", platform_string(&u)),
        ("architecture", machine),
        ("hostname", u.nodename.clone()),
        ("home dir", home_dir()),
    ];
    let mut out = String::new();
    for (key, value) in details {
        out.push_str(&format!("{}: {}\n", key, value));
    }
    out.trim().to_string()
}

/// the `(name, value)` pair of a required tool argument; the python TypeError text when it is not a str
fn arg_str(a: &Map<String, Value>, name: &str) -> Result<String, String> {
    match a.get(name) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!("expected str, bytes or os.PathLike object, not {}", py_type_name(other))),
        None => Err(format!("missing argument '{}'", name)),
    }
}

/// `arg_str` for tools whose python body has no try block (the TypeError escapes the tool)
fn must(a: &Map<String, Value>, name: &str) -> anyhow::Result<String> {
    arg_str(a, name).map_err(|m| anyhow!(m))
}

/// the keyword parameters of each `FileManager` tool method
fn tool_params(method: &str) -> Option<&'static [&'static str]> {
    Some(match method {
        "list" | "read" | "read_binary" | "get_info" | "remove" | "mkdir" => &["path"][..],
        "write" | "append" => &["path", "content"][..],
        "move" | "copy" => &["src", "dst"][..],
        _ => return None,
    })
}

/// python's binding of `func(**tool_args)`: an unexpected keyword is reported before a missing one.
/// Both are raised at the call boundary, so they come back as `Error while executing tool: ...`
fn bind_kwargs(method: &str, a: &Map<String, Value>, params: &[&str]) -> anyhow::Result<()> {
    if let Some(extra) = a.keys().find(|k| !params.contains(&k.as_str())) {
        anyhow::bail!("FileManager.{}() got an unexpected keyword argument '{}'", method, extra);
    }
    let missing: Vec<String> = params.iter().filter(|p| !a.contains_key(**p)).map(|p| format!("'{}'", p)).collect();
    let n = missing.len();
    let names = match n {
        0 => return Ok(()),
        1 => missing[0].clone(),
        2 => format!("{} and {}", missing[0], missing[1]),
        _ => format!("{}, and {}", missing[..n - 1].join(", "), missing[n - 1]),
    };
    let noun = if n == 1 { "argument" } else { "arguments" };
    anyhow::bail!("FileManager.{}() missing {} required positional {}: {}", method, n, noun, names)
}

impl FileManager {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }

    fn fail(&self, msg: impl Into<Value>) -> Value {
        self.base.result(msg, false)
    }

    async fn list(&self, path: &str) -> anyhow::Result<Value> {
        if let Some(reason) = verify_path(path, true, false) {
            return Ok(self.fail(reason));
        }
        // python returns os.listdir(path) as-is (a bare list, not a result dict)
        let rd = fs::read_dir(path).map_err(|e| anyhow!(os_err(&e, path)))?;
        let mut names = Vec::new();
        for ent in rd {
            let ent = ent.map_err(|e| anyhow!(os_err(&e, path)))?;
            names.push(Value::String(ent.file_name().to_string_lossy().into_owned()));
        }
        Ok(Value::Array(names))
    }

    async fn read(&self, path: &str) -> anyhow::Result<Value> {
        if let Some(reason) = verify_path(path, false, true) {
            return Ok(self.fail(reason));
        }
        Ok(match read_text(path) {
            Ok(text) => self.base.result(text, true),
            Err(ReadErr::NotText) => self.fail("File is not a valid text file. Try 'read_binary' instead."),
            Err(ReadErr::Io(m)) => self.fail(m),
        })
    }

    async fn read_binary(&self, path: &str) -> anyhow::Result<Value> {
        if let Some(reason) = verify_path(path, false, true) {
            return Ok(self.fail(reason));
        }
        match fs::read(path) {
            Err(e) => Ok(self.fail(os_err(&e, path))),
            // python returns the raw bytes inside result(), and the tool loop's json.dumps() of that
            // result raises TypeError outside its try block. This mirrors that failure.
            Ok(_) => Err(anyhow!("Object of type bytes is not JSON serializable")),
        }
    }

    /// python `_write` (used by write with mode 'w' and append with mode 'a')
    fn write_file(&self, path: &str, content: &Value, mode: &str) -> Value {
        match write_inner(path, content, mode) {
            Ok(()) => self.base.result(format!("Successfully wrote to {} (mode: {})", path, mode), true),
            Err(m) => self.fail(m),
        }
    }

    fn mkdir(&self, path: &str) -> Value {
        let p = py_path(path);
        match fs::create_dir_all(&p) {
            Ok(()) => self.base.result(format!("Directory created: {}", path), true),
            Err(e) => self.fail(os_err(&e, &p)),
        }
    }

    fn remove(&self, path: &str) -> Value {
        let p = py_path(path);
        let pp = Path::new(&p);
        if !pp.exists() {
            return self.fail("path does not exist");
        }
        let is_link = fs::symlink_metadata(pp).map(|m| m.file_type().is_symlink()).unwrap_or(false);
        let removed = if pp.is_file() || is_link {
            fs::remove_file(pp)
        } else if pp.is_dir() {
            fs::remove_dir_all(pp)
        } else {
            return self.fail("Path is not a file or directory");
        };
        match removed {
            Ok(()) => self.base.result(format!("Successfully removed: {}", path), true),
            Err(e) => self.fail(os_err(&e, &p)),
        }
    }

    fn move_path(&self, src: &str, dst: &str) -> Value {
        match move_inner(src, dst) {
            Ok(()) => self.base.result(format!("Moved {} -> {}", src, dst), true),
            Err(m) => self.fail(m),
        }
    }

    fn copy_path(&self, src: &str, dst: &str) -> Value {
        match copy_inner(src, dst) {
            Ok(()) => self.base.result(format!("Copied {} -> {}", src, dst), true),
            Err(m) => self.fail(m),
        }
    }

    async fn get_info(&self, path: &str) -> anyhow::Result<Value> {
        if let Some(reason) = verify_path(path, false, false) {
            return Ok(self.fail(reason));
        }
        let p = py_path(path);
        let md = match fs::metadata(&p) {
            Ok(md) => md,
            Err(e) => return Ok(self.fail(os_err(&e, &p))),
        };
        let pp = Path::new(&p);
        Ok(self.base.result(
            json!({
                "name": py_name(path),
                "absolute_path": py_absolute(path),
                "size_bytes": md.size(),
                "is_file": pp.is_file(),
                "is_dir": pp.is_dir(),
                "last_modified": ts(md.mtime(), md.mtime_nsec()),
                "created": ts(md.ctime(), md.ctime_nsec()),
            }),
            true,
        ))
    }
}

#[async_trait]
impl Module for FileManager {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    fn header(&self) -> Option<String> {
        meta::HEADER.map(|h| h.to_string())
    }

    async fn on_end_prompt(&self) -> anyhow::Result<Option<String>> {
        if !self.base.config.get_bool("put_system_info_in_prompt") {
            return Ok(None);
        }
        Ok(Some(system_info()))
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        let a = &args.0;
        let null = Value::Null;
        let Some(params) = tool_params(method) else {
            anyhow::bail!("'{}' object has no attribute '{}'", meta::CLASS_NAME, method);
        };
        bind_kwargs(method, a, params)?;
        match method {
            // no try block in python: a non-str path raises out of the tool
            "list" => self.list(&must(a, "path")?).await,
            "read" => self.read(&must(a, "path")?).await,
            "read_binary" => self.read_binary(&must(a, "path")?).await,
            "get_info" => self.get_info(&must(a, "path")?).await,
            "remove" => Ok(self.remove(&must(a, "path")?)),
            // try blocks in python: a bad argument comes back as an error result
            "write" | "append" => {
                let mode = if method == "append" { "a" } else { "w" };
                Ok(match arg_str(a, "path") {
                    Ok(p) => self.write_file(&p, a.get("content").unwrap_or(&null), mode),
                    Err(m) => self.fail(m),
                })
            }
            "mkdir" => Ok(match arg_str(a, "path") {
                Ok(p) => self.mkdir(&p),
                Err(m) => self.fail(m),
            }),
            "move" => Ok(match (arg_str(a, "src"), arg_str(a, "dst")) {
                (Ok(src), Ok(dst)) => self.move_path(&src, &dst),
                (Err(m), _) | (_, Err(m)) => self.fail(m),
            }),
            "copy" => Ok(match (arg_str(a, "src"), arg_str(a, "dst")) {
                (Ok(src), Ok(dst)) => self.copy_path(&src, &dst),
                (Err(m), _) | (_, Err(m)) => self.fail(m),
            }),
            _ => anyhow::bail!("'{}' object has no attribute '{}'", meta::CLASS_NAME, method),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fm() -> FileManager {
        FileManager::new(ModuleBase::detached("file_manager"))
    }

    async fn call(m: &FileManager, method: &str, args: Value) -> anyhow::Result<Value> {
        let map = args.as_object().cloned().unwrap_or_default();
        m.call_tool(method, ToolArgs::new(map)).await
    }

    #[tokio::test]
    async fn write_read_append_round_trip_with_python_strings() {
        let dir = tempfile::tempdir().unwrap();
        let ps = dir.path().join("sub").join("notes.txt").to_str().unwrap().to_string();
        let m = fm();

        let w = call(&m, "write", json!({"path": ps.clone(), "content": "a\r\nb\rc"})).await.unwrap();
        assert_eq!(w, json!({"status": "success", "content": format!("Successfully wrote to {} (mode: w)", ps)}));

        // text-mode read applies universal newlines
        let r = call(&m, "read", json!({"path": ps.clone()})).await.unwrap();
        assert_eq!(r, json!({"status": "success", "content": "a\nb\nc"}));

        let a = call(&m, "append", json!({"path": ps.clone(), "content": "!"})).await.unwrap();
        assert_eq!(a["content"], format!("Successfully wrote to {} (mode: a)", ps));
        let r = call(&m, "read", json!({"path": ps.clone()})).await.unwrap();
        assert_eq!(r["content"], "a\nb\nc!");
    }

    #[tokio::test]
    async fn verify_reasons_and_read_errors_match_python() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "hello").unwrap();
        let bin = dir.path().join("b.bin");
        fs::write(&bin, [0xffu8, 0xfe, 0x00]).unwrap();
        let m = fm();
        let fs_ = file.to_str().unwrap();
        let ds = dir.path().to_str().unwrap();

        let missing = call(&m, "list", json!({"path": dir.path().join("nope").to_str().unwrap()})).await.unwrap();
        assert_eq!(missing, json!({"status": "error", "content": "path does not exist"}));
        let not_dir = call(&m, "list", json!({"path": fs_})).await.unwrap();
        assert_eq!(not_dir, json!({"status": "error", "content": "target path is not a directory"}));
        let not_file = call(&m, "read", json!({"path": ds})).await.unwrap();
        assert_eq!(not_file, json!({"status": "error", "content": "target path is not a file"}));
        let not_text = call(&m, "read", json!({"path": bin.to_str().unwrap()})).await.unwrap();
        assert_eq!(
            not_text,
            json!({"status": "error", "content": "File is not a valid text file. Try 'read_binary' instead."})
        );
        // python's json.dumps of the raw bytes fails after a successful read_binary
        let bytes_err = call(&m, "read_binary", json!({"path": fs_})).await.unwrap_err();
        assert_eq!(bytes_err.to_string(), "Object of type bytes is not JSON serializable");
        // a non-str path escapes the tool as a TypeError (no try block in list)
        let type_err = call(&m, "list", json!({"path": 5})).await.unwrap_err();
        assert_eq!(type_err.to_string(), "expected str, bytes or os.PathLike object, not int");
    }

    #[tokio::test]
    async fn mkdir_move_copy_remove_messages() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        let m = fm();
        let a = format!("{}/a.txt", root);
        let b = format!("{}/out/b.txt", root);
        let c = format!("{}/c.txt", root);

        let mk = call(&m, "mkdir", json!({"path": format!("{}/out", root)})).await.unwrap();
        assert_eq!(mk, json!({"status": "success", "content": format!("Directory created: {}/out", root)}));
        call(&m, "write", json!({"path": a.clone(), "content": "x"})).await.unwrap();

        let mv = call(&m, "move", json!({"src": a.clone(), "dst": b.clone()})).await.unwrap();
        assert_eq!(mv["content"], format!("Moved {} -> {}", a, b));
        assert!(!Path::new(&a).exists());

        let cp = call(&m, "copy", json!({"src": b.clone(), "dst": c.clone()})).await.unwrap();
        assert_eq!(cp["content"], format!("Copied {} -> {}", b, c));
        assert_eq!(fs::read_to_string(&c).unwrap(), "x");

        // shutil.move into an existing directory keeps the basename
        let into = call(&m, "move", json!({"src": c.clone(), "dst": format!("{}/out", root)})).await.unwrap();
        assert_eq!(into, json!({"status": "success", "content": format!("Moved {} -> {}/out", c, root)}));
        assert_eq!(fs::read_to_string(format!("{}/out/c.txt", root)).unwrap(), "x");

        // a clash with an existing entry in that directory is an error result
        call(&m, "write", json!({"path": format!("{}/x.txt", root), "content": "y"})).await.unwrap();
        fs::write(format!("{}/out/x.txt", root), "z").unwrap();
        let clash = call(&m, "move", json!({"src": format!("{}/x.txt", root), "dst": format!("{}/out", root)})).await.unwrap();
        assert_eq!(
            clash,
            json!({"status": "error", "content": format!("Destination path '{}/out/x.txt' already exists", root)})
        );
        assert!(Path::new(&format!("{}/x.txt", root)).exists(), "a refused move leaves the source in place");

        let rm = call(&m, "remove", json!({"path": format!("{}/out/c.txt", root)})).await.unwrap();
        assert_eq!(rm["content"], format!("Successfully removed: {}/out/c.txt", root));
        let gone = call(&m, "remove", json!({"path": format!("{}/out/c.txt", root)})).await.unwrap();
        assert_eq!(gone, json!({"status": "error", "content": "path does not exist"}));
    }

    #[tokio::test]
    async fn get_info_reports_python_keys_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "abc").unwrap();
        let fs_ = file.to_str().unwrap().to_string();
        let info = call(&fm(), "get_info", json!({"path": fs_.clone()})).await.unwrap();
        assert_eq!(info["status"], "success");
        let content = info["content"].as_object().unwrap();
        let keys: Vec<&str> = content.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, ["name", "absolute_path", "size_bytes", "is_file", "is_dir", "last_modified", "created"]);
        assert_eq!(content["name"], "f.txt");
        assert_eq!(content["absolute_path"], fs_);
        assert_eq!(content["size_bytes"], 3);
        assert_eq!(content["is_file"], true);
        assert_eq!(content["is_dir"], false);
    }

    #[tokio::test]
    async fn copy_keeps_going_past_a_failing_entry_and_reports_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        let src = format!("{}/s", root);
        let dst = format!("{}/t", root);
        fs::create_dir(&src).unwrap();
        fs::write(format!("{}/a.txt", src), "x").unwrap();
        let fifo = format!("{}/f", src);
        let c = CString::new(fifo.clone()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);

        let r = call(&fm(), "copy", json!({"src": src.clone(), "dst": dst.clone()})).await.unwrap();
        // python's shutil.copytree copies a.txt, records the named pipe, then raises shutil.Error(list)
        assert_eq!(
            r,
            json!({"status": "error", "content": format!("[('{0}', '{1}/f', '`{0}` is a named pipe')]", fifo, dst)})
        );
        assert_eq!(fs::read_to_string(format!("{}/a.txt", dst)).unwrap(), "x");
    }

    #[tokio::test]
    async fn missing_and_unexpected_arguments_raise_like_python_call_binding() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "old").unwrap();
        let fs_ = file.to_str().unwrap().to_string();
        let m = fm();

        let err = call(&m, "list", json!({})).await.unwrap_err();
        assert_eq!(err.to_string(), "FileManager.list() missing 1 required positional argument: 'path'");
        let err = call(&m, "move", json!({"src": fs_.clone()})).await.unwrap_err();
        assert_eq!(err.to_string(), "FileManager.move() missing 1 required positional argument: 'dst'");
        let err = call(&m, "move", json!({})).await.unwrap_err();
        assert_eq!(err.to_string(), "FileManager.move() missing 2 required positional arguments: 'src' and 'dst'");
        let err = call(&m, "list", json!({"path": dir.path().to_str().unwrap(), "extra": 1})).await.unwrap_err();
        assert_eq!(err.to_string(), "FileManager.list() got an unexpected keyword argument 'extra'");

        // python raises at the call boundary, before the body runs: no truncation without content
        let err = call(&m, "write", json!({"path": fs_.clone()})).await.unwrap_err();
        assert_eq!(err.to_string(), "FileManager.write() missing 1 required positional argument: 'content'");
        assert_eq!(fs::read_to_string(&file).unwrap(), "old");
    }

    #[test]
    fn system_info_lines_follow_python_order() {
        let info = system_info();
        let keys: Vec<&str> = info.lines().filter_map(|l| l.split(": ").next()).collect();
        assert_eq!(keys, ["OS", "OS release", "platform", "architecture", "hostname", "home dir"]);
        assert!(!info.ends_with('\n'));
    }
}
