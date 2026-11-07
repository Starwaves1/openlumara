//! Port of modules/docs/module.py (incl. the webui sidebar routes)

use crate::util;
use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::{py_str, truthy};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::path::Path;

#[path = "../../../generated/modules/docs.rs"]
pub(crate) mod meta;

pub struct Docs {
    base: ModuleBase,
    disabled: Mutex<Vec<String>>,
}

const INSTRUCTION: &str = "You can call docs_list again with a subfolder to navigate deeper, or use docs_read to read a file.";

impl Docs {
    pub fn new(base: ModuleBase) -> Self {
        Self { base, disabled: Mutex::new(Vec::new()) }
    }

    /// the raw `folders` items in python's iteration order
    fn raw_folders(&self) -> anyhow::Result<Vec<Value>> {
        folder_items(self.base.config.get1("folders"))
    }

    /// python `f.rstrip(os.path.sep)` needs a str: any other item raises AttributeError
    fn folder_str(item: &Value) -> anyhow::Result<String> {
        match item {
            Value::String(s) => Ok(s.clone()),
            other => anyhow::bail!("'{}' object has no attribute 'rstrip'", util::py_type_name(other)),
        }
    }

    fn folders(&self) -> anyhow::Result<Vec<String>> {
        self.raw_folders()?.iter().map(Self::folder_str).collect()
    }

    /// translates the folder path list into basenames for the AI
    pub fn get_folder_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.folders()?.iter().map(|f| util::basename(&util::rstrip_sep(f))).collect())
    }

    /// resolves a folder basename to its full path
    pub fn get_folder_path(&self, folder: &str) -> anyhow::Result<Option<String>> {
        let wanted = util::rstrip_sep(folder);
        // python checks one item at a time and returns at the match, so a bad item only fails if it is reached
        for item in self.raw_folders()? {
            let f = Self::folder_str(&item)?;
            if util::basename(&util::rstrip_sep(&f)) == wanted {
                return Ok(Some(util::rstrip_sep(&ol_core::paths::get_path(&util::expanduser(&f))?)));
            }
        }
        Ok(None)
    }
}

/// python `for f in folders` after the `if not folders` guard: a list yields its items, a string its
/// characters, a dict its keys; any other truthy value raises python's "not iterable" TypeError
fn folder_items(folders: Option<Value>) -> anyhow::Result<Vec<Value>> {
    let Some(v) = folders.filter(truthy) else { return Ok(Vec::new()) };
    match v {
        Value::Array(a) => Ok(a),
        Value::String(s) => Ok(s.chars().map(|c| json!(c.to_string())).collect()),
        Value::Object(o) => Ok(o.keys().map(|k| json!(k)).collect()),
        other => anyhow::bail!("'{}' object is not iterable", util::py_type_name(&other)),
    }
}

/// one directory level: (subfolders, files) relative paths
fn list_level(base_path: &str, subfolder: Option<&str>) -> Result<(Vec<String>, Vec<String>), String> {
    let contents = util::listdir(base_path)?;
    let prefix = match subfolder {
        Some(s) if !s.is_empty() => format!("{}/", s),
        _ => String::new(),
    };
    let mut dirs: Vec<String> = contents.iter().filter(|d| Path::new(base_path).join(d).is_dir()).map(|d| format!("{}{}", prefix, d)).collect();
    let mut files: Vec<String> = contents.iter().filter(|f| Path::new(base_path).join(f).is_file()).map(|f| format!("{}{}", prefix, f)).collect();
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
        for item in self.raw_folders()? {
            // python's os.path.expanduser raises TypeError for a non-str item
            let folder = match &item {
                Value::String(s) => s.clone(),
                other => anyhow::bail!("expected str, bytes or os.PathLike object, not {}", util::py_type_name(other)),
            };
            let p = util::rstrip_sep(&ol_core::paths::get_path(&util::expanduser(&folder))?);
            std::fs::create_dir_all(&p).map_err(|e| anyhow::anyhow!(util::os_error(&e, &p)))?;
        }
        if self.base.config.get_bool("insert_system_prompt") {
            self.disabled.lock().push("get_topics".into());
        }
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        let folders = self.get_folder_names()?;
        if folders.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!("## Documentation can be fetched from these folders:\n{}", folders.join(", "))))
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "get_topics" => Ok(self.base.result(json!(self.get_folder_names()?), true)),
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
                match util::read_text(&target_path) {
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
            "folders" => Ok(json!(self.get_folder_names()?)),
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
                let read = ol_core::paths::sandbox_path(&folder_path, Some(&path)).map_err(|e| e.to_string()).and_then(|t| util::read_text(&t));
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

    #[test]
    fn folder_items_iterates_like_python() {
        // falsy values never iterate
        assert!(folder_items(Some(json!([]))).unwrap().is_empty());
        assert!(folder_items(Some(json!(0))).unwrap().is_empty());
        assert!(folder_items(None).unwrap().is_empty());
        // a dict yields its keys
        assert_eq!(folder_items(Some(json!({"notes": 1}))).unwrap(), vec![json!("notes")]);
        // a string yields characters
        assert_eq!(folder_items(Some(json!("ab"))).unwrap(), vec![json!("a"), json!("b")]);
        // a truthy number or bool raises python's TypeError text
        assert_eq!(folder_items(Some(json!(3))).unwrap_err().to_string(), "'int' object is not iterable");
        assert_eq!(folder_items(Some(json!(true))).unwrap_err().to_string(), "'bool' object is not iterable");
    }
}
