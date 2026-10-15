//! Port of the path helpers in core/functions.py (get_path, get_data_path, sandbox_path, ...)
//!
//! The Python code resolves the "project root" from the location of its own source files.
//! The Rust binary instead resolves it once at startup (see [`set_project_root`]):
//! `--root` / `OPENLUMARA_ROOT`, otherwise the current directory if it looks like an
//! OpenLumara checkout, otherwise the nearest ancestor of the executable that does.

use once_cell::sync::OnceCell;
use std::path::{Component, Path, PathBuf};

static PROJECT_ROOT: OnceCell<PathBuf> = OnceCell::new();

fn looks_like_root(p: &Path) -> bool {
    p.join("channels").join("webui").is_dir() || p.join("main.py").is_file()
}

fn detect_root() -> PathBuf {
    if let Ok(env) = std::env::var("OPENLUMARA_ROOT") {
        if !env.is_empty() {
            return absolutize(Path::new(&env));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if looks_like_root(&cwd) {
            return cwd;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut cur = exe.parent().map(|p| p.to_path_buf());
        while let Some(dir) = cur {
            if looks_like_root(&dir) {
                return dir;
            }
            cur = dir.parent().map(|p| p.to_path_buf());
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Set the project root explicitly. Has no effect if the root was already resolved.
pub fn set_project_root(p: impl AsRef<Path>) {
    let _ = PROJECT_ROOT.set(absolutize(p.as_ref()));
}

pub fn project_root() -> &'static Path {
    PROJECT_ROOT.get_or_init(detect_root)
}

fn absolutize(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    normpath(&abs)
}

/// os.path.normpath equivalent (lexical: collapses `.`, `..` and duplicate separators)
pub fn normpath(p: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !matches!(out.last(), Some(Component::RootDir)) {
                    out.push(comp);
                }
            }
            c => out.push(c),
        }
    }
    if out.is_empty() {
        return PathBuf::from(".");
    }
    out.iter().collect()
}

/// os.path.realpath equivalent (non-strict: components that don't exist are kept as-is)
pub fn realpath(p: &Path) -> PathBuf {
    let p = absolutize(p);
    // find the longest existing ancestor and canonicalize that
    let mut existing = p.clone();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = std::fs::canonicalize(&existing) {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return normpath(&out);
        }
        match (existing.file_name().map(|s| s.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return p,
        }
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

/// get path relative to the project root directory. returns root path if no path is specified.
pub fn get_path(path: &str) -> anyhow::Result<String> {
    get_path_opt(path, true)
}

pub fn get_path_opt(path: &str, sandbox: bool) -> anyhow::Result<String> {
    let root = path_str(project_root());
    if path.is_empty() {
        return Ok(root);
    }
    if Path::new(path).is_absolute() {
        return Ok(path.to_string());
    }
    if sandbox {
        sandbox_path(&root, Some(path))
    } else {
        Ok(path_str(&Path::new(&root).join(path)))
    }
}

/// get path to the data directory. contains all persistent data used by the framework
pub fn get_data_path(subpath: Option<&str>) -> anyhow::Result<String> {
    let mut data_path = crate::config::get(&["core", "data_folder"])
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "data".to_string());
    if !Path::new(&data_path).is_absolute() {
        data_path = get_path(&data_path)?;
    }
    if !Path::new(&data_path).exists() {
        std::fs::create_dir_all(&data_path)?;
    }
    match subpath {
        Some(s) if !s.is_empty() => sandbox_path(&data_path, Some(s)),
        _ => Ok(data_path),
    }
}

/// Python's urllib.parse.unquote (utf-8, errors='replace')
pub fn unquote(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .to_string()
}

fn strip_sep(s: &str) -> &str {
    s.trim_matches(std::path::MAIN_SEPARATOR)
}

/// validates a path string for traversal and encoding attacks.
pub fn validate_path_string(path: &str) -> anyhow::Result<String> {
    let sep = std::path::MAIN_SEPARATOR.to_string();
    let path = strip_sep(path);
    let mut decoded = path.to_string();
    for _ in 0..3 {
        decoded = unquote(&decoded);
    }
    decoded = decoded.replace('\\', &sep).replace('/', &sep);
    let decoded = strip_sep(&decoded).to_string();
    if decoded.contains("..") || decoded.contains('\0') {
        anyhow::bail!("Path traversal is not allowed ({})", path);
    }
    Ok(decoded)
}

/// protects against path traversal attacks and the like
pub fn sandbox_path(base_path: &str, requested_path: Option<&str>) -> anyhow::Result<String> {
    let requested = match requested_path {
        Some(r) if !r.is_empty() => r,
        _ => return Ok(base_path.to_string()),
    };
    let sep = std::path::MAIN_SEPARATOR.to_string();
    let base_path = base_path.replace('\\', &sep).replace('/', &sep);
    let mut path = requested.replace('\\', &sep).replace('/', &sep);
    path = strip_sep(&path).to_string();

    let prefix = format!("{}{}", base_path, sep);
    if let Some(stripped) = path.strip_prefix(&prefix) {
        path = stripped.to_string();
    } else if path == base_path {
        path = String::new();
    }

    let decoded = validate_path_string(&path)?;

    // block symlink paths
    let parts: Vec<&str> = decoded.split(std::path::MAIN_SEPARATOR).collect();
    for i in 1..parts.len() {
        let mut test = PathBuf::from(&base_path);
        for p in &parts[..i] {
            test.push(p);
        }
        if test.is_symlink() {
            anyhow::bail!("Symlinks are not allowed in the path");
        }
    }

    if path.is_empty() {
        return Ok(base_path);
    }

    let path_in_base = Path::new(&base_path).join(normpath(Path::new(&decoded)));
    let real = realpath(&path_in_base);
    if path_in_base.is_symlink() {
        anyhow::bail!("Symlinks are not allowed in the requested path");
    }

    let real_s = path_str(&real);
    let base_prefix = format!("{}{}", base_path, sep);
    if real_s.starts_with(&base_prefix) || real_s == base_path {
        Ok(real_s)
    } else {
        anyhow::bail!("Access denied: target path is outside sandbox")
    }
}

/// os.path.join for strings
pub fn join(a: &str, b: &str) -> String {
    path_str(&Path::new(a).join(b))
}

/// removes duplicates from a list (keeps first occurrence order)
pub fn remove_duplicates<T: PartialEq + Clone>(lst: &[T]) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for item in lst {
        if !out.contains(item) {
            out.push(item.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_basic() {
        let dir = tempfile::tempdir().unwrap();
        let base = realpath(dir.path()).to_string_lossy().to_string();
        assert_eq!(sandbox_path(&base, None).unwrap(), base);
        assert_eq!(sandbox_path(&base, Some("a/b.txt")).unwrap(), format!("{}/a/b.txt", base));
        assert_eq!(sandbox_path(&base, Some("/a/b.txt")).unwrap(), format!("{}/a/b.txt", base));
        assert!(sandbox_path(&base, Some("../etc")).is_err());
        assert!(sandbox_path(&base, Some("a/%2e%2e/b")).is_err());
        assert!(sandbox_path(&base, Some("%252e%252e/x")).is_err());
        // python strips the leading separator first, so an absolute path that already
        // includes the base is nested again (same quirk as the python implementation)
        assert_eq!(
            sandbox_path(&base, Some(&format!("{}/x", base))).unwrap(),
            format!("{}{}/x", base, base)
        );
    }

    #[test]
    fn sandbox_symlink_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let base = realpath(dir.path()).to_string_lossy().to_string();
        std::os::unix::fs::symlink("/tmp", format!("{}/link", base)).unwrap();
        assert!(sandbox_path(&base, Some("link/x")).is_err());
        assert!(sandbox_path(&base, Some("link")).is_err());
    }

    #[test]
    fn normpath_works() {
        assert_eq!(normpath(Path::new("/a/./b//c/../d")), PathBuf::from("/a/b/d"));
    }
}
