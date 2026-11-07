//! Port of modules/file_manager/module.py: the `FileManager` module, which gives the AI full
//! access to the filesystem (list, read, read_binary, write, append, mkdir, remove, move, copy,
//! get_info) and can put system information into the end-of-context prompt.
//!
//! Python's `os`, `pathlib` and `shutil` behaviour (path normalisation, `str(OSError)` messages,
//! `shutil.move` / `shutil.copytree` / `shutil.copy2` semantics) lives in `util.rs`.

#[path = "../../../generated/modules/file_manager.rs"]
pub mod meta;

use crate::util::{
    display_os, os_err, os_err2, os_err_bare, py_abspath, py_absolute, py_basename_os, py_name, py_parent, py_path, py_quote,
    py_repr_os, py_type_name, same_file, universal_newlines,
};
use anyhow::anyhow;
use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use serde_json::{json, Map, Value};
use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// python `os.path.samefile` / `os.stat` ValueError text for an embedded NUL in a path
const STAT_NUL: &str = "stat: embedded null character in path";
/// python `os.mkdir` ValueError text for an embedded NUL in a path
const MKDIR_NUL: &str = "mkdir: embedded null character in path";
/// python `open()` ValueError text for an embedded NUL in a path
const OPEN_NUL: &str = "embedded null byte";

fn has_nul(p: &Path) -> bool {
    p.as_os_str().as_bytes().contains(&0)
}

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
fn copystat(src: &Path, dst: &Path, follow: bool) -> Result<(), String> {
    let md = if follow { fs::metadata(src) } else { fs::symlink_metadata(src) }.map_err(|e| os_err(&e, src))?;
    set_times(dst, &md, follow)?;
    copy_xattrs(src, dst, follow)?;
    if follow {
        fs::set_permissions(dst, md.permissions()).map_err(|e| os_err(&e, dst))?;
    }
    Ok(())
}

fn set_times(dst: &Path, md: &fs::Metadata, follow: bool) -> Result<(), String> {
    let c = CString::new(dst.as_os_str().as_bytes()).map_err(|_| format!("embedded null byte in path {}", py_repr_os(dst)))?;
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
fn copy_xattrs(src: &Path, dst: &Path, follow: bool) -> Result<(), String> {
    let cs = CString::new(src.as_os_str().as_bytes()).map_err(|_| format!("embedded null byte in path {}", py_repr_os(src)))?;
    let cd = CString::new(dst.as_os_str().as_bytes()).map_err(|_| format!("embedded null byte in path {}", py_repr_os(dst)))?;
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
fn copy2(src: &Path, dst: &Path) -> Result<(), String> {
    let real_dst = if dst.is_dir() { dst.join(py_basename_os(src.as_os_str())) } else { dst.to_path_buf() };
    // python's copyfile() calls os.path.samefile first, which raises ValueError (not OSError) on a NUL
    if has_nul(src) || has_nul(&real_dst) {
        return Err(STAT_NUL.to_string());
    }
    if same_file(src, &real_dst) {
        return Err(format!("{} and {} are the same file", py_repr_os(src), py_repr_os(&real_dst)));
    }
    for name in [src, real_dst.as_path()] {
        if let Ok(m) = fs::metadata(name) {
            if m.file_type().is_fifo() {
                return Err(format!("`{}` is a named pipe", display_os(name)));
            }
        }
    }
    // python opens src first, then dst with 'wb' (truncating)
    let mut input = File::open(src).map_err(|e| os_err(&e, src))?;
    let mut output = File::create(&real_dst).map_err(|e| os_err(&e, &real_dst))?;
    io::copy(&mut input, &mut output).map_err(|e| os_err_bare(&e))?;
    copystat(src, &real_dst, true)
}

/// `(src, dst, message)` triples python's `shutil.copytree` collects into `shutil.Error`; the paths
/// are already python reprs, since that is how the list is shown
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
                let items: Vec<String> =
                    errors.iter().map(|(s, d, m)| format!("({}, {}, {})", s, d, py_quote(m))).collect();
                format!("[{}]", items.join(", "))
            }
        }
    }
}

/// python `shutil.copytree(src, dst, symlinks=...)` with the default copy2 copy function. A failing
/// entry is recorded and the rest of the tree is still copied; the collected errors are raised together
fn copytree(src: &Path, dst: &Path, symlinks: bool) -> Result<(), String> {
    copytree_inner(src, dst, symlinks).map_err(CopyErr::into_message)
}

fn copytree_inner(src: &Path, dst: &Path, symlinks: bool) -> Result<(), CopyErr> {
    let entries: Vec<fs::DirEntry> = fs::read_dir(src)
        .map_err(|e| CopyErr::Os(os_err(&e, src)))?
        .collect::<Result<_, _>>()
        .map_err(|e| CopyErr::Os(os_err(&e, src)))?;
    fs::create_dir(dst).map_err(|e| CopyErr::Os(os_err(&e, dst)))?;
    let mut errors: CopyErrors = Vec::new();
    for ent in entries {
        // the raw entry name: python joins it as-is, so non-UTF-8 names copy like any other
        let name = ent.file_name();
        let srcname = src.join(&name);
        let dstname = dst.join(&name);
        if let Err(err) = copy_entry(&ent, &srcname, &dstname, symlinks) {
            match err {
                CopyErr::Os(m) => errors.push((py_repr_os(&srcname), py_repr_os(&dstname), m)),
                CopyErr::Tree(nested) => errors.extend(nested),
            }
        }
    }
    if let Err(m) = copystat(src, dst, true) {
        errors.push((py_repr_os(src), py_repr_os(dst), m));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(CopyErr::Tree(errors))
    }
}

/// one iteration of python `_copytree`'s loop body for a single directory entry
fn copy_entry(ent: &fs::DirEntry, srcname: &Path, dstname: &Path, symlinks: bool) -> Result<(), CopyErr> {
    let ft = ent.file_type().map_err(|e| CopyErr::Os(os_err(&e, srcname)))?;
    if ft.is_symlink() {
        if symlinks {
            let target = fs::read_link(srcname).map_err(|e| CopyErr::Os(os_err(&e, srcname)))?;
            std::os::unix::fs::symlink(&target, dstname).map_err(|e| CopyErr::Os(os_err2(&e, &target, dstname)))?;
            copystat(srcname, dstname, false).map_err(CopyErr::Os)
        } else if srcname.is_dir() {
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
    let (src_p, dst_p) = (Path::new(src), Path::new(dst));
    let mut real_dst = dst_p.to_path_buf();
    if dst_p.is_dir() {
        // python's _samefile() only swallows OSError, so a NUL in src escapes as a ValueError from os.stat
        if has_nul(src_p) {
            return Err(STAT_NUL.to_string());
        }
        if same_file(src_p, dst_p) {
            return fs::rename(src_p, dst_p).map_err(|e| os_err2(&e, src, dst));
        }
        real_dst = dst_p.join(py_basename_os(src_p.as_os_str()));
        if real_dst.exists() {
            return Err(format!("Destination path '{}' already exists", display_os(&real_dst)));
        }
    }
    // os.rename's own ValueError for a NUL (not an OSError, so no fallback): src is converted first
    if has_nul(src_p) {
        return Err("rename: embedded null character in src".to_string());
    }
    if has_nul(&real_dst) {
        return Err("rename: embedded null character in dst".to_string());
    }
    if fs::rename(src_p, &real_dst).is_ok() {
        return Ok(());
    }
    let is_link = fs::symlink_metadata(src_p).map(|m| m.file_type().is_symlink()).unwrap_or(false);
    if is_link {
        let linkto = fs::read_link(src_p).map_err(|e| os_err(&e, src_p))?;
        std::os::unix::fs::symlink(&linkto, &real_dst).map_err(|e| os_err2(&e, &linkto, &real_dst))?;
        fs::remove_file(src_p).map_err(|e| os_err(&e, src_p))
    } else if src_p.is_dir() {
        if destinsrc(src, dst) {
            return Err(format!("Cannot move a directory '{}' into itself '{}'.", src, dst));
        }
        copytree(src_p, &real_dst, true)?;
        rmtree(src)
    } else {
        copy2(src_p, &real_dst)?;
        fs::remove_file(src_p).map_err(|e| os_err(&e, src_p))
    }
}

/// python `shutil.rmtree(path)` on CPython 3.14's fd-based walk. The walk is replayed with the same stack
/// order, and each failure is reported on the path the failing step was working on: the directory for an
/// unlink or scandir, the entry itself for its open or rmdir. A child that vanishes mid-walk is skipped.
fn rmtree(top: &str) -> Result<(), String> {
    enum Step {
        /// open a directory and scan it (`child` is false only for the top-level path)
        Walk { path: PathBuf, child: bool },
        /// remove an emptied directory
        Rmdir { path: PathBuf, child: bool },
    }
    let skip_missing = |e: &io::Error, child: bool| child && e.kind() == io::ErrorKind::NotFound;
    let mut stack = vec![Step::Walk { path: PathBuf::from(top), child: false }];
    while let Some(step) = stack.pop() {
        match step {
            Step::Rmdir { path, child } => match fs::remove_dir(&path) {
                Ok(()) => {}
                Err(e) if skip_missing(&e, child) => {}
                Err(e) => return Err(os_err(&e, &path)),
            },
            Step::Walk { path, child } => {
                let md = match fs::symlink_metadata(&path) {
                    Ok(md) => md,
                    Err(e) if skip_missing(&e, child) => continue,
                    Err(e) => return Err(os_err(&e, &path)),
                };
                if md.file_type().is_symlink() {
                    return Err("Cannot call rmtree on a symbolic link".to_string());
                }
                let entries = match fs::read_dir(&path) {
                    Ok(rd) => rd,
                    Err(e) if skip_missing(&e, child) => continue,
                    Err(e) => return Err(os_err(&e, &path)),
                };
                stack.push(Step::Rmdir { path: path.clone(), child });
                let entries: Vec<fs::DirEntry> = match entries.collect::<Result<_, _>>() {
                    Ok(v) => v,
                    Err(e) if skip_missing(&e, child) => continue,
                    Err(e) => return Err(os_err(&e, &path)),
                };
                for ent in entries {
                    let fullname = path.join(ent.file_name());
                    match ent.file_type() {
                        Ok(ft) if ft.is_dir() => {
                            stack.push(Step::Walk { path: fullname, child: true });
                            continue;
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                        _ => {}
                    }
                    match fs::remove_file(&fullname) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        // python's onexc re-raises with the directory being scanned as the filename
                        Err(e) => return Err(os_err(&e, &path)),
                    }
                }
            }
        }
    }
    Ok(())
}

/// python `Path(dst).parent.mkdir(parents=True, exist_ok=True)`, which move and copy run before their body
fn make_parent(dst: &str) -> Result<(), String> {
    let parent = py_parent(dst);
    if parent.contains('\0') {
        return Err(MKDIR_NUL.to_string());
    }
    fs::create_dir_all(&parent).map_err(|e| os_err(&e, &parent))
}

fn copy_inner(src: &str, dst: &str) -> Result<(), String> {
    if Path::new(src).is_dir() {
        // python's copytree makes dst with os.makedirs, which raises the mkdir ValueError for a NUL
        if has_nul(Path::new(dst)) {
            return Err(MKDIR_NUL.to_string());
        }
        copytree(Path::new(src), Path::new(dst), false)
    } else {
        copy2(Path::new(src), Path::new(dst))
    }
}

/// python `Module._write` body: parent dirs, open (truncating for 'w'), then `f.write(content)`
fn write_inner(path: &str, content: &Value, mode: &str) -> Result<(), String> {
    let parent = py_parent(path);
    if parent.contains('\0') {
        return Err(MKDIR_NUL.to_string());
    }
    fs::create_dir_all(&parent).map_err(|e| os_err(&e, &parent))?;
    if path.contains('\0') {
        return Err(OPEN_NUL.to_string());
    }
    let mut f = if mode == "a" {
        OpenOptions::new().append(true).create(true).open(path)
    } else {
        OpenOptions::new().write(true).create(true).truncate(true).open(path)
    }
    .map_err(|e| os_err(&e, path))?;
    // the file is already opened (and truncated for 'w') when python's f.write() rejects the value
    let Value::String(text) = content else {
        // TextIOWrapper.write names None as "None" (verified: python3 -c "f.write(None)"), not "NoneType"
        let t = if content.is_null() { "None" } else { py_type_name(content) };
        return Err(format!("write() argument must be str, not {}", t));
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

/// python 3.14 `Path(x)` TypeError text for a non-str path argument (pathlib's own message, not `os.fspath`'s)
fn path_type_err(other: &Value) -> String {
    format!("argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{}'", py_type_name(other))
}

fn missing_arg(name: &str) -> String {
    format!("missing argument '{}'", name)
}

/// the value of a path argument as `Path(value)` accepts it
fn path_arg(a: &Map<String, Value>, name: &str) -> Result<String, String> {
    match a.get(name) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(path_type_err(other)),
        None => Err(missing_arg(name)),
    }
}

/// `path_arg` for tools whose python body has no try block (the TypeError escapes the tool)
fn must(a: &Map<String, Value>, name: &str) -> anyhow::Result<String> {
    path_arg(a, name).map_err(|m| anyhow!(m))
}

/// python `shutil.move(src, dst)` with a non-str `src`: the error depends on whether `dst` is a directory,
/// because `shutil.move` only calls `os.stat(src)` / `os.fspath(src)` on that branch
fn move_src_type_err(dst: &str, src: &Value) -> String {
    let t = py_type_name(src);
    if !Path::new(dst).is_dir() {
        return format!("rename: src should be string, bytes or os.PathLike, not {}", t);
    }
    match src {
        // an int is taken as a file descriptor by os.stat, then os.fspath rejects it
        Value::Bool(_) => format!("expected str, bytes or os.PathLike object, not {}", t),
        Value::Number(n) if !n.is_f64() => format!("expected str, bytes or os.PathLike object, not {}", t),
        _ => format!("stat: path should be string, bytes, os.PathLike or integer, not {}", t),
    }
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
            // result raises TypeError outside its try block, which escapes the loop. A module cannot
            // reproduce that escape: ol-core turns this Err into an "Error while executing tool" result.
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
        if path.contains('\0') {
            return self.fail(MKDIR_NUL);
        }
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
            fs::remove_file(pp).map_err(|e| os_err(&e, pp))
        } else if pp.is_dir() {
            rmtree(&p)
        } else {
            return self.fail("Path is not a file or directory");
        };
        match removed {
            Ok(()) => self.base.result(format!("Successfully removed: {}", path), true),
            Err(m) => self.fail(m),
        }
    }

    /// python `move`: `Path(dst)` and its parent mkdir run before `shutil.move`, so dst is checked first
    fn move_tool(&self, a: &Map<String, Value>) -> Value {
        let dst = match path_arg(a, "dst") {
            Ok(d) => d,
            Err(m) => return self.fail(m),
        };
        if let Err(m) = make_parent(&dst) {
            return self.fail(m);
        }
        let src = match a.get("src") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => return self.fail(move_src_type_err(&dst, other)),
            None => return self.fail(missing_arg("src")),
        };
        match shutil_move(&src, &dst) {
            Ok(()) => self.base.result(format!("Moved {} -> {}", src, dst), true),
            Err(m) => self.fail(m),
        }
    }

    /// python `copy`: `Path(dst)` and its parent mkdir run before `Path(src).is_dir()`
    fn copy_tool(&self, a: &Map<String, Value>) -> Value {
        let dst = match path_arg(a, "dst") {
            Ok(d) => d,
            Err(m) => return self.fail(m),
        };
        if let Err(m) = make_parent(&dst) {
            return self.fail(m);
        }
        let src = match a.get("src") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => return self.fail(path_type_err(other)),
            None => return self.fail(missing_arg("src")),
        };
        match copy_inner(&src, &dst) {
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
                Ok(match path_arg(a, "path") {
                    Ok(p) => self.write_file(&p, a.get("content").unwrap_or(&null), mode),
                    Err(m) => self.fail(m),
                })
            }
            "mkdir" => Ok(match path_arg(a, "path") {
                Ok(p) => self.mkdir(&p),
                Err(m) => self.fail(m),
            }),
            "move" => Ok(self.move_tool(a)),
            "copy" => Ok(self.copy_tool(a)),
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
        assert_eq!(
            type_err.to_string(),
            "argument should be a str or an os.PathLike object where __fspath__ returns a str, not 'int'"
        );
    }

    #[tokio::test]
    async fn move_and_copy_check_dst_and_make_its_parent_before_src() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        let m = fm();

        // python makes dst's parent before shutil.move sees the non-str src, then os.rename rejects it
        let mv = call(&m, "move", json!({"src": 5, "dst": format!("{}/new/parent/x.txt", root)})).await.unwrap();
        assert_eq!(
            mv,
            json!({"status": "error", "content": "rename: src should be string, bytes or os.PathLike, not int"})
        );
        assert!(Path::new(&format!("{}/new/parent", root)).is_dir());

        // a bad dst is reported first when src is bad too
        let both = call(&m, "move", json!({"src": 5, "dst": null})).await.unwrap();
        assert_eq!(
            both,
            json!({"status": "error", "content": "argument should be a str or an os.PathLike object where __fspath__ returns a str, not 'NoneType'"})
        );

        // copy: dst's parent is created, then Path(src) rejects the int
        let cp = call(&m, "copy", json!({"src": 5, "dst": format!("{}/c/d/y.txt", root)})).await.unwrap();
        assert_eq!(
            cp,
            json!({"status": "error", "content": "argument should be a str or an os.PathLike object where __fspath__ returns a str, not 'int'"})
        );
        assert!(Path::new(&format!("{}/c/d", root)).is_dir());

        // into an existing directory, shutil.move's os.fspath wording applies
        let into = call(&m, "move", json!({"src": 5, "dst": root.clone()})).await.unwrap();
        assert_eq!(into, json!({"status": "error", "content": "expected str, bytes or os.PathLike object, not int"}));
    }

    #[tokio::test]
    async fn write_non_str_content_names_python_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = format!("{}/w.txt", dir.path().to_str().unwrap());
        let m = fm();
        // python: f.write(None) -> TypeError "write() argument must be str, not None", caught by _write
        let w = call(&m, "write", json!({"path": path.clone(), "content": null})).await.unwrap();
        assert_eq!(w, json!({"status": "error", "content": "write() argument must be str, not None"}));
        let n = call(&m, "write", json!({"path": path.clone(), "content": 5})).await.unwrap();
        assert_eq!(n, json!({"status": "error", "content": "write() argument must be str, not int"}));
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

    #[tokio::test]
    async fn nul_bytes_raise_the_same_messages_as_python() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        let m = fm();
        let file = format!("{}/x.txt", root);
        fs::write(&file, "x").unwrap();

        // open() rejects a NUL in the basename after the parent was made
        let w = call(&m, "write", json!({"path": format!("{}/a\0b", root), "content": "c"})).await.unwrap();
        assert_eq!(w, json!({"status": "error", "content": "embedded null byte"}));
        // a NUL in the parent fails in mkdir first
        let w = call(&m, "write", json!({"path": format!("{}/a\0/c", root), "content": "c"})).await.unwrap();
        assert_eq!(w, json!({"status": "error", "content": "mkdir: embedded null character in path"}));
        let mk = call(&m, "mkdir", json!({"path": format!("{}/a\0b/c", root)})).await.unwrap();
        assert_eq!(mk, json!({"status": "error", "content": "mkdir: embedded null character in path"}));
        // rename() names the argument that holds the NUL, source first
        let mv = call(&m, "move", json!({"src": format!("{}/a\0b", root), "dst": format!("{}/y", root)})).await.unwrap();
        assert_eq!(mv, json!({"status": "error", "content": "rename: embedded null character in src"}));
        let mv = call(&m, "move", json!({"src": file.clone(), "dst": format!("{}/q\0", root)})).await.unwrap();
        assert_eq!(mv, json!({"status": "error", "content": "rename: embedded null character in dst"}));
        // copy2's samefile() check raises a stat ValueError
        let cp = call(&m, "copy", json!({"src": file.clone(), "dst": format!("{}/q\0", root)})).await.unwrap();
        assert_eq!(cp, json!({"status": "error", "content": "stat: embedded null character in path"}));
        // a NUL-named path does not exist for remove or read
        let rm = call(&m, "remove", json!({"path": format!("{}/a\0b", root)})).await.unwrap();
        assert_eq!(rm, json!({"status": "error", "content": "path does not exist"}));
        assert_eq!(fs::read_to_string(&file).unwrap(), "x", "a refused write never truncates the file");
    }

    #[tokio::test]
    async fn copy_and_move_handle_non_utf8_entry_names_like_python() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src = root.join("s");
        fs::create_dir(&src).unwrap();
        let raw = std::ffi::OsStr::from_bytes(b"f\xffname");
        fs::write(src.join(raw), "raw").unwrap();
        fs::write(src.join("plain.txt"), "p").unwrap();
        let m = fm();

        // copytree copies the entry under its raw bytes, so the copy reports success
        let dst = root.join("t");
        let r = call(&m, "copy", json!({"src": src.to_str().unwrap(), "dst": dst.to_str().unwrap()})).await.unwrap();
        assert_eq!(r["status"], "success", "{}", r);
        assert_eq!(fs::read(dst.join(raw)).unwrap(), b"raw");

        // a directory moved to a new name keeps its raw entry names (rename takes the fast path here)
        let moved = root.join("m");
        let r = call(&m, "move", json!({"src": dst.to_str().unwrap(), "dst": moved.to_str().unwrap()})).await.unwrap();
        assert_eq!(r["status"], "success", "{}", r);
        assert_eq!(fs::read(moved.join(raw)).unwrap(), b"raw");
        assert!(!dst.exists());
    }

    #[tokio::test]
    async fn remove_reports_the_directory_whose_step_failed() {
        // root bypasses the permission check that makes the failure, so there is nothing to test
        // SAFETY: geteuid has no preconditions
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().join("top");
        let inner = top.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("f"), "x").unwrap();
        // a read-only directory cannot unlink its entries, so python names that directory
        fs::set_permissions(&inner, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let m = fm();
        let r = call(&m, "remove", json!({"path": top.to_str().unwrap()})).await.unwrap();
        fs::set_permissions(&inner, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(
            r,
            json!({"status": "error", "content": format!("[Errno 13] Permission denied: '{}'", inner.to_str().unwrap())})
        );
    }

    #[test]
    fn repr_of_undecodable_names_matches_python() {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(crate::util::py_repr_os(std::ffi::OsStr::from_bytes(b"a\xff")), "'a\\udcff'");
        assert_eq!(crate::util::py_repr_os(std::ffi::OsStr::from_bytes(b"a'\xff\"")), "'a\\'\\udcff\"'");
        assert_eq!(crate::util::display_os(std::ffi::OsStr::from_bytes(b"a\xff")), "a\\udcff");
    }

    #[test]
    fn system_info_lines_follow_python_order() {
        let info = system_info();
        let keys: Vec<&str> = info.lines().filter_map(|l| l.split(": ").next()).collect();
        assert_eq!(keys, ["OS", "OS release", "platform", "architecture", "hostname", "home dir"]);
        assert!(!info.ends_with('\n'));
    }
}
