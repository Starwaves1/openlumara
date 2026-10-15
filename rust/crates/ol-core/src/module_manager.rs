//! Port of core/module_manager.py: external module libraries (git repos listed in
//! `core.module_libraries`), shallow-cloned into `data/module_libraries/`.
//!
//! Note for the Rust port: installed library entries are python modules/channels.
//! They are copied into user_modules/ user_channels/ exactly like python does, so the
//! folders stay compatible, but the Rust binary can't execute them (see PORTING.md).

use crate::config;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Weak;

pub const LIBRARY_SUBFOLDER: &str = "module_libraries";
pub const KNOWN_SUBFOLDERS: [&str; 4] = ["modules", "user_modules", "channels", "user_channels"];

pub struct ModuleManager {
    manager: Weak<crate::manager::Manager>,
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        let n = name.to_string_lossy();
        if n == "__pycache__" || n == ".git" {
            continue;
        }
        let target = dst.join(&name);
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), target)?;
        }
    }
    Ok(())
}

impl ModuleManager {
    pub fn new(manager: Weak<crate::manager::Manager>) -> Self {
        Self { manager }
    }

    fn log(&self, msg: &str) {
        match self.manager.upgrade() {
            Some(m) => m.log("modules", msg),
            None => crate::log("modules", msg),
        }
    }

    pub fn library_folder(&self) -> PathBuf {
        let p = crate::paths::get_data_path(Some(LIBRARY_SUBFOLDER)).unwrap_or_else(|_| LIBRARY_SUBFOLDER.into());
        let _ = std::fs::create_dir_all(&p);
        PathBuf::from(p)
    }

    /// derive a safe local folder name from a git url
    pub fn repo_name(&self, url: &str) -> String {
        let mut name = url.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
        if let Some(n) = name.strip_suffix(".git") {
            name = n.to_string();
        }
        let name: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
        if name.is_empty() {
            "repo".into()
        } else {
            name
        }
    }

    pub fn list_repos(&self) -> Vec<String> {
        match config::get(&["core", "module_libraries"]) {
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            _ => vec![],
        }
    }

    fn find_git(&self) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("git");
            if cand.is_file() {
                return Some(cand);
            }
        }
        self.log("git executable not found - module libraries cannot be synced");
        None
    }

    async fn run_git(&self, git: &Path, args: &[&str]) -> (i32, String, String) {
        match tokio::process::Command::new(git).args(args).output().await {
            Ok(o) => (
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stdout).trim().to_string(),
                String::from_utf8_lossy(&o.stderr).trim().to_string(),
            ),
            Err(e) => (-1, String::new(), e.to_string()),
        }
    }

    /// the origin url of a cloned library, if git is available
    pub async fn git_origin(&self, path: &Path) -> Option<String> {
        let git = self.find_git()?;
        let p = path.to_string_lossy().to_string();
        let (code, out, _) = self.run_git(&git, &["-C", &p, "remote", "get-url", "origin"]).await;
        if code == 0 && !out.is_empty() {
            Some(out)
        } else {
            None
        }
    }

    /// clone a library repo, or update it if already cloned. returns the local path
    pub async fn sync_repo(&self, url: &str) -> Option<PathBuf> {
        let git = self.find_git()?;
        let url = url.trim();
        let dest = self.library_folder().join(self.repo_name(url));
        let d = dest.to_string_lossy().to_string();
        if dest.join(".git").is_dir() {
            let (code, _, err) = self.run_git(&git, &["-C", &d, "fetch", "--depth", "1", "origin"]).await;
            if code != 0 {
                self.log(&format!("failed to fetch {}: {} - using existing copy", url, err));
                return Some(dest);
            }
            let (code, _, _) = self.run_git(&git, &["-C", &d, "reset", "--hard", "origin/HEAD"]).await;
            if code != 0 {
                let (code, _, err) = self.run_git(&git, &["-C", &d, "reset", "--hard", "@{u}"]).await;
                if code != 0 {
                    self.log(&format!("failed to update {}: {} - using existing copy", url, err));
                }
            }
            return Some(dest);
        }
        let (code, _, err) = self.run_git(&git, &["clone", "--depth", "1", url, &d]).await;
        if code != 0 {
            self.log(&format!("failed to clone {}: {}", url, err));
            return None;
        }
        self.log(&format!("cloned module library {}", url));
        Some(dest)
    }

    /// sync all module libraries. true if at least one synced
    pub async fn load(&self) -> bool {
        let repos = self.list_repos();
        if repos.is_empty() {
            return false;
        }
        let mut synced = 0;
        for r in repos {
            if self.sync_repo(&r).await.is_some() {
                synced += 1;
            }
        }
        synced > 0
    }

    /// locally cloned libraries as {name: path}
    pub fn list_libraries(&self) -> BTreeMap<String, PathBuf> {
        let base = self.library_folder();
        let mut libs = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() && p.join(".git").is_dir() {
                    libs.insert(e.file_name().to_string_lossy().to_string(), p);
                }
            }
        }
        libs
    }

    fn first_doc_line(file: &Path) -> String {
        config::extract_class_metadata_from_file(file)
            .map(|m| m.docstring.trim().split('\n').next().unwrap_or("").to_string())
            .unwrap_or_default()
    }

    fn scan_dir(&self, root: &Path, library: &str, installed: &[String], out: &mut Vec<Value>) {
        let Ok(rd) = std::fs::read_dir(root) else { return };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().to_string();
            let dir = e.path();
            if !dir.is_dir() || name.starts_with('_') || name.starts_with('.') {
                continue;
            }
            for (file, kind) in [("module.py", "module"), ("channel.py", "channel")] {
                let p = dir.join(file);
                if p.is_file() {
                    out.push(json!({
                        "library": library,
                        "name": name,
                        "type": kind,
                        "doc": Self::first_doc_line(&p),
                        "installed": installed.contains(&name),
                    }));
                    break;
                }
            }
        }
    }

    /// scan cloned libraries for installable modules/channels
    pub fn list_available(&self) -> Vec<Value> {
        let mut installed = Vec::new();
        for sub in KNOWN_SUBFOLDERS {
            installed.extend(config::discover_folder_names(&config::section_folder(sub)));
        }
        let mut out = Vec::new();
        for (lib, path) in self.list_libraries() {
            self.scan_dir(&path, &lib, &installed, &mut out);
            for sub in KNOWN_SUBFOLDERS {
                let sp = path.join(sub);
                if sp.is_dir() {
                    self.scan_dir(&sp, &lib, &installed, &mut out);
                }
            }
        }
        out
    }

    /// locate a module/channel folder inside a cloned library: (source_dir, kind)
    pub fn find_source(&self, lib_path: &Path, name: &str) -> Option<(PathBuf, &'static str)> {
        let mut roots = vec![lib_path.to_path_buf()];
        roots.extend(KNOWN_SUBFOLDERS.iter().map(|s| lib_path.join(s)));
        for root in roots {
            for (file, kind) in [("module.py", "module"), ("channel.py", "channel")] {
                if root.join(name).join(file).is_file() {
                    return Some((root.join(name), kind));
                }
            }
        }
        None
    }

    fn target_root(kind: &str) -> PathBuf {
        crate::paths::project_root().join(if kind == "module" { "user_modules" } else { "user_channels" })
    }

    /// copy a module/channel from a cloned library into the user folders
    pub fn install(&self, library: &str, name: &str) -> Result<String, String> {
        let libs = self.list_libraries();
        let Some(lib_path) = libs.get(library) else { return Err(format!("library '{}' not found - sync it first", library)) };
        let Some((source, kind)) = self.find_source(lib_path, name) else { return Err(format!("'{}' not found in library '{}'", name, library)) };
        let target = Self::target_root(kind).join(name);
        if target.exists() {
            return Err(format!("'{}' is already installed at {}", name, target.display()));
        }
        copy_tree(&source, &target).map_err(|e| e.to_string())?;
        self.log(&format!("installed {} '{}' from library '{}'", kind, name, library));
        Ok(target.to_string_lossy().to_string())
    }

    /// re-sync a library and overwrite an installed module with the latest version
    pub async fn update(&self, library: &str, name: &str) -> Result<String, String> {
        let url = self.list_repos().into_iter().find(|r| self.repo_name(r) == library);
        let Some(url) = url else { return Err(format!("library '{}' is not a configured repo - cannot update", library)) };
        let Some(dest) = self.sync_repo(&url).await else { return Err(format!("failed to update library '{}'", library)) };
        let Some((source, kind)) = self.find_source(&dest, name) else {
            return Err(format!("'{}' not found in library '{}' anymore", name, library));
        };
        let target = Self::target_root(kind).join(name);
        if !target.is_dir() {
            return Err(format!("'{}' is not installed - use install instead", name));
        }
        copy_tree(&source, &target).map_err(|e| e.to_string())?;
        self.log(&format!("updated {} '{}' from library '{}'", kind, name, library));
        Ok(target.to_string_lossy().to_string())
    }

    /// remove an installed user module/channel
    pub fn uninstall(&self, name: &str) -> Result<String, String> {
        for sub in ["user_modules", "user_channels"] {
            let target = crate::paths::project_root().join(sub).join(name);
            if target.is_dir() {
                std::fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
                self.log(&format!("removed {}/{}", sub, name));
                return Ok(target.to_string_lossy().to_string());
            }
        }
        Err(format!("'{}' is not installed as a user module/channel", name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names() {
        let mm = ModuleManager::new(Weak::new());
        assert_eq!(mm.repo_name("https://github.com/Rose22/openlumara-modules.git"), "openlumara-modules");
        assert_eq!(mm.repo_name("git@github.com:x/y z/"), "y_z");
    }
}
