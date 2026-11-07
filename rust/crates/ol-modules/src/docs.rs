//! Port of modules/docs/module.py (incl. the webui sidebar routes)

use crate::util;
use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use std::sync::Arc;
use ol_core::pyjson::py_str;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::path::Path;

#[path = "../../../generated/modules/docs.rs"]
pub mod meta;

pub struct Docs {
    base: ModuleBase,
    disabled: Mutex<Vec<String>>,
}

const INSTRUCTION: &str = "You can call docs_list again with a subfolder to navigate deeper, or use docs_read to read a file.";

impl Docs {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base, disabled: Mutex::new(Vec::new()) }
    }

    fn folders(&self) -> Vec<String> {
        match self.base.config.get1("folders") {
            Some(Value::Array(a)) => a.iter().map(py_str).collect(),
            Some(Value::String(s)) if !s.is_empty() => s.chars().map(|c| c.to_string()).collect(),
            _ => Vec::new(),
        }
    }

    /// translates the folder path list into basenames for the AI
    pub fn get_folder_names(&self) -> Vec<String> {
        self.folders().iter().map(|f| util::basename(&util::rstrip_sep(f))).collect()
    }

    /// resolves a folder basename to its full path
    pub fn get_folder_path(&self, folder: &str) -> anyhow::Result<Option<String>> {
        let wanted = util::rstrip_sep(folder);
        for f in self.folders() {
            if util::basename(&util::rstrip_sep(&f)) == wanted {
                return Ok(Some(util::rstrip_sep(&ol_core::paths::get_path(&util::expanduser(&f))?)));
            }
        }
        Ok(None)
    }
}

/// one directory level: (subfolders, files) relative paths
fn list_level(base_path: &str, subfolder: Option<&str>) -> Result<(Vec<String>, Vec<String>), String> {
    let contents = util::listdir(base_path)?;
    let prefix = match subfolder {
        Some(s) if !s.is_empty() => format!("{}/", s),
        _ => String::new(),
    };
    // each entry is tested by its real OS name; only the text shown to the AI is lossy
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for name in &contents {
        let entry = Path::new(base_path).join(name);
        let shown = format!("{}{}", prefix, name.to_string_lossy());
        if entry.is_dir() {
            dirs.push(shown);
        } else if entry.is_file() {
            files.push(shown);
        }
    }
    dirs.sort();
    files.sort();
    Ok((dirs, files))
}

/// remove the folder from the requested path in case the AI decided to double-add it
fn strip_folder(path: &str, folder: &str) -> String {
    match path.strip_prefix(folder) {
        Some(rest) => rest.to_string(),
        None => path.to_string(),
    }
}

fn query_str(query: &Map<String, Value>, key: &str) -> String {
    query.get(key).filter(|v| !v.is_null()).map(py_str).unwrap_or_default()
}

#[async_trait]
impl Module for Docs {
    fn base(&self) -> &ModuleBase {
        &self.base
    }
    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }
    fn disabled_tools(&self) -> Vec<String> {
        self.disabled.lock().clone()
    }
    fn ui_routes(&self) -> Vec<UiRoute> {
        meta::ui_routes().into_iter().map(|(r, _)| r).collect()
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        for folder in self.folders() {
            let p = util::rstrip_sep(&ol_core::paths::get_path(&util::expanduser(&folder))?);
            std::fs::create_dir_all(&p).map_err(|e| anyhow::anyhow!(util::os_error(&e, &p)))?;
        }
        if self.base.config.get_bool("insert_system_prompt") {
            self.disabled.lock().push("get_topics".into());
        }
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        let folders = self.get_folder_names();
        if folders.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!("## Documentation can be fetched from these folders:\n{}", folders.join(", "))))
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "get_topics" => Ok(self.base.result(json!(self.get_folder_names()), true)),
            "list" => {
                let folder = args.str("folder").unwrap_or_default();
                let subfolder = args.str("subfolder");
                let attempt = || -> Result<(Vec<String>, Vec<String>), Result<String, String>> {
                    let folder_path = self.get_folder_path(&folder).map_err(|e| Err(e.to_string()))?;
                    let Some(folder_path) = folder_path.filter(|p| !p.is_empty()) else {
                        return Err(Ok(format!("Folder '{}' does not exist within the docs module configuration.", folder)));
                    };
                    let subfolder = match &subfolder {
                        Some(s) if !s.is_empty() && s.starts_with(&folder) => Some(strip_folder(s, &folder)),
                        other => other.clone(),
                    };
                    let base_path = ol_core::paths::sandbox_path(&folder_path, Some(subfolder.as_deref().unwrap_or(""))).map_err(|e| Err(e.to_string()))?;
                    list_level(&base_path, subfolder.as_deref()).map_err(Err)
                };
                match attempt() {
                    Ok((dirs, files)) => Ok(self.base.result(json!({"subfolders": dirs, "files": files, "instruction": INSTRUCTION}), true)),
                    Err(Ok(msg)) | Err(Err(msg)) => Ok(self.base.result(json!(msg), false)),
                }
            }
            "read" => {
                let folder = args.str("folder").unwrap_or_default();
                let path = args.str("path").unwrap_or_default();
                let folder_path = self.get_folder_path(&folder)?;
                let path = strip_folder(&path, &folder);
                let Some(folder_path) = folder_path.filter(|p| !p.is_empty()) else {
                    return Ok(self.base.result(json!(format!("Folder '{}' does not exist within the docs module configuration.", folder)), false));
                };
                let target_path = ol_core::paths::sandbox_path(&folder_path, Some(&path))?;
                match util::read_listed_text(&target_path) {
                    Ok(t) => Ok(self.base.result(json!(t), true)),
                    Err(e) => Ok(self.base.result(json!(e), false)),
                }
            }
            _ => anyhow::bail!("'Docs' object has no attribute '{}'", method),
        }
    }

    async fn ui_route(&self, path: &str, _method: &str, _body: Option<Value>, query: Map<String, Value>) -> anyhow::Result<Value> {
        match path {
            // doc folder basenames for the sidebar dropdown
            "folders" => Ok(json!(self.get_folder_names())),
            // one directory level: {subfolders: [...], files: [...]} relative paths
            "list" => {
                let folder = query_str(&query, "folder");
                let subfolder = Some(query_str(&query, "subfolder")).filter(|s| !s.is_empty());
                if folder.is_empty() {
                    return Ok(self.base.result(json!("no folder given"), false));
                }
                let Some(folder_path) = self.get_folder_path(&folder)?.filter(|p| !p.is_empty()) else {
                    return Ok(self.base.result(json!(format!("folder '{}' does not exist", folder)), false));
                };
                let subfolder = match subfolder {
                    Some(s) if s.starts_with(&folder) => Some(strip_folder(&s, &folder)),
                    other => other,
                };
                let listed = ol_core::paths::sandbox_path(&folder_path, Some(subfolder.as_deref().unwrap_or("")))
                    .map_err(|e| e.to_string())
                    .and_then(|base_path| list_level(&base_path, subfolder.as_deref()));
                match listed {
                    Ok((dirs, files)) => Ok(json!({"subfolders": dirs, "files": files})),
                    Err(e) => Ok(self.base.result(json!(e), false)),
                }
            }
            // raw text of a doc file
            "read" => {
                let folder = query_str(&query, "folder");
                let path = query_str(&query, "path");
                if folder.is_empty() || path.is_empty() {
                    return Ok(self.base.result(json!("folder and path are both required"), false));
                }
                let Some(folder_path) = self.get_folder_path(&folder)?.filter(|p| !p.is_empty()) else {
                    return Ok(self.base.result(json!(format!("folder '{}' does not exist", folder)), false));
                };
                let path = strip_folder(&path, &folder);
                let read = ol_core::paths::sandbox_path(&folder_path, Some(&path)).map_err(|e| e.to_string()).and_then(|t| util::read_listed_text(&t));
                match read {
                    Ok(content) => Ok(json!({"path": path, "content": content})),
                    Err(e) => Ok(self.base.result(json!(e), false)),
                }
            }
            _ => anyhow::bail!("route not found: {}", path),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    /// the config is process-wide: tests that install one hold this lock while they run
    use crate::util::CONFIG_TEST_LOCK as CONFIG_LOCK;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn args(v: Value) -> ToolArgs {
        ToolArgs::new(v.as_object().unwrap().clone())
    }

    fn query(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    /// installs a docs config whose only folder is `<tmp>/guide` (absolute, so it bypasses the project root)
    /// and returns the module plus the tempdir guard
    fn setup(insert_system_prompt: bool) -> (Docs, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let guide = dir.path().join("guide");
        std::fs::create_dir_all(guide.join("sub")).unwrap();
        std::fs::write(guide.join("a.md"), "hello docs\r\nsecond line").unwrap();
        std::fs::write(guide.join("sub").join("b.md"), "nested").unwrap();
        let settings: Map<String, Value> = Map::from_iter([
            ("folders".to_string(), json!([guide.to_string_lossy()])),
            ("insert_system_prompt".to_string(), json!(insert_system_prompt)),
        ]);
        let data = json!({"modules": {"settings": {"docs": settings}}});
        ol_core::config::set_for_tests(data.clone());
        // set_for_tests() keeps a config file from an earlier test, and every read reloads that file,
        // so the same data is written to it (JSON is valid YAML)
        let path = ol_core::config::config_path().expect("test config path");
        std::fs::write(&path, data.to_string()).expect("write test config");
        (Docs::new(ModuleBase::detached("docs")), dir)
    }

    #[test]
    fn list_tool_returns_subfolders_files_and_instruction() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (m, _dir) = setup(true);
        let out = block_on(m.call_tool("list", args(json!({"folder": "guide"})))).unwrap();
        assert_eq!(
            out,
            json!({
                "status": "success",
                "content": {
                    "subfolders": ["sub"],
                    "files": ["a.md"],
                    "instruction": "You can call docs_list again with a subfolder to navigate deeper, or use docs_read to read a file."
                }
            })
        );
        // a relative subfolder gets its own prefix on every entry
        let out = block_on(m.call_tool("list", args(json!({"folder": "guide", "subfolder": "sub"})))).unwrap();
        assert_eq!(out["content"]["files"], json!(["sub/b.md"]));
        assert_eq!(out["content"]["subfolders"], json!([]));
        // a subfolder that repeats the folder name is stripped first
        let out = block_on(m.call_tool("list", args(json!({"folder": "guide", "subfolder": "guidesub"})))).unwrap();
        assert_eq!(out["content"]["files"], json!(["sub/b.md"]));
    }

    #[test]
    fn unknown_folder_and_read_return_python_strings() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (m, _dir) = setup(true);
        let out = block_on(m.call_tool("list", args(json!({"folder": "nope"})))).unwrap();
        assert_eq!(
            out,
            json!({"status": "error", "content": "Folder 'nope' does not exist within the docs module configuration."})
        );
        let out = block_on(m.call_tool("read", args(json!({"folder": "nope", "path": "a.md"})))).unwrap();
        assert_eq!(out, json!({"status": "error", "content": "Folder 'nope' does not exist within the docs module configuration."}));

        // read normalizes CRLF like python universal newlines, and strips a doubled folder prefix
        let out = block_on(m.call_tool("read", args(json!({"folder": "guide", "path": "guide/a.md"})))).unwrap();
        assert_eq!(out, json!({"status": "success", "content": "hello docs\nsecond line"}));

        let out = block_on(m.call_tool("read", args(json!({"folder": "guide", "path": "missing.md"})))).unwrap();
        assert_eq!(out["status"], "error");
        assert!(out["content"].as_str().unwrap().starts_with("[Errno 2] No such file or directory: "), "{}", out);
    }

    #[test]
    fn system_prompt_and_get_topics_follow_config() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (m, _dir) = setup(true);
        let prompt = block_on(m.on_system_prompt()).unwrap();
        assert_eq!(prompt.as_deref(), Some("## Documentation can be fetched from these folders:\nguide"));
        let topics = block_on(m.call_tool("get_topics", ToolArgs::default())).unwrap();
        assert_eq!(topics, json!({"status": "success", "content": ["guide"]}));

        // insert_system_prompt true disables the get_topics tool, as the python on_ready does
        block_on(m.on_ready()).unwrap();
        assert_eq!(m.disabled_tools(), vec!["get_topics".to_string()]);

        let (m2, _dir2) = setup(false);
        block_on(m2.on_ready()).unwrap();
        assert!(m2.disabled_tools().is_empty());
    }

    #[test]
    fn non_utf8_file_names_are_listed_and_readable() {
        use std::os::unix::ffi::OsStrExt;
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (m, dir) = setup(true);
        let raw = std::ffi::OsStr::from_bytes(b"raw\xff.md");
        std::fs::write(dir.path().join("guide").join(raw), "raw bytes").unwrap();
        // python lists the surrogateescape name as a file; here it is listed with U+FFFD and still counts as a file
        let out = block_on(m.call_tool("list", args(json!({"folder": "guide"})))).unwrap();
        assert_eq!(out["content"]["files"], json!(["a.md", "raw\u{fffd}.md"]));
        assert_eq!(out["content"]["subfolders"], json!(["sub"]));
        // reading the listed name opens the real file
        let read = block_on(m.call_tool("read", args(json!({"folder": "guide", "path": "raw\u{fffd}.md"})))).unwrap();
        assert_eq!(read, json!({"status": "success", "content": "raw bytes"}));
    }

    #[test]
    fn webui_routes_list_and_read() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (m, _dir) = setup(true);
        let folders = block_on(m.ui_route("folders", "GET", None, Map::new())).unwrap();
        assert_eq!(folders, json!(["guide"]));
        let listed = block_on(m.ui_route("list", "GET", None, query(json!({"folder": "guide"})))).unwrap();
        assert_eq!(listed, json!({"subfolders": ["sub"], "files": ["a.md"]}));
        let read = block_on(m.ui_route("read", "GET", None, query(json!({"folder": "guide", "path": "a.md"})))).unwrap();
        assert_eq!(read, json!({"path": "a.md", "content": "hello docs\nsecond line"}));
        let bad = block_on(m.ui_route("read", "GET", None, query(json!({"folder": "guide"})))).unwrap();
        assert_eq!(bad, json!({"status": "error", "content": "folder and path are both required"}));
    }
}
