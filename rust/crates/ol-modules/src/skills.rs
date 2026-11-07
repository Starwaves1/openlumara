//! Port of modules/skills/module.py: skill files (a folder holding a SKILL.md) listed in the
//! system prompt, loaded on demand through the `use` tool and the `/skill` command.

#[path = "../../../generated/modules/skills.rs"]
pub mod meta;
#[path = "skills_pyyaml.rs"]
mod pyyaml;

use crate::util::{self, expanduser, listdir, py_strip, rstrip_sep, splitlines};
use anyhow::anyhow;
use async_trait::async_trait;
use indexmap::IndexMap;
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::{py_str, truthy};
use parking_lot::Mutex;
use pyyaml::PyObj;
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// skill name -> folder that holds its SKILL.md (python `self.index`, insertion ordered)
type Index = IndexMap<String, PathBuf>;

pub struct Skills {
    base: ModuleBase,
    /// python `self.index`; unset until on_ready like the python attribute
    index: Mutex<Option<Index>>,
}

/// why a skill body could not be loaded (python `_load_body` returns "not found" or str(e))
enum LoadError {
    NotFound,
    Read(String),
}

/// python str.lstrip() with no arguments
fn py_lstrip(s: &str) -> &str {
    s.trim_start_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// python `_parse_frontmatter`: splits a SKILL.md into (frontmatter or None, body).
/// the frontmatter is the yaml block between the first pair of --- lines, read as `yaml.safe_load`
/// reads it; None when that fails or does not give a mapping (python dict).
fn parse_frontmatter(text: &str) -> (Option<PyObj>, String) {
    let stripped = py_lstrip(text);
    if !stripped.starts_with("---") {
        return (None, text.to_string());
    }
    let lines = splitlines(stripped);
    // python: the first line after the opening one that is --- or ...
    let Some(end) = (1..lines.len()).find(|&i| matches!(py_strip(lines[i]), "---" | "...")) else {
        return (None, text.to_string());
    };
    let rest = lines[end + 1..].join("\n");
    match pyyaml::load(&lines[1..end].join("\n")) {
        Ok(meta @ PyObj::Dict(_)) => (Some(meta), rest),
        _ => (None, rest),
    }
}

/// python `if meta and meta.get("name")`: the name as str(), None when the key is missing or falsy
fn frontmatter_name(meta: &PyObj) -> Option<String> {
    let name = meta.get("name")?;
    name.truthy().then(|| name.py_str())
}

/// the order python's `sorted()` gives a listed name: its code points, where python decodes each
/// byte that is not utf-8 to U+DC80..U+DCFF (surrogateescape)
fn python_str_key(name: &OsStr) -> Vec<u32> {
    let mut key = Vec::new();
    for chunk in name.as_encoded_bytes().utf8_chunks() {
        key.extend(chunk.valid().chars().map(u32::from));
        key.extend(chunk.invalid().iter().map(|&b| 0xDC00 + u32::from(b)));
    }
    key
}

/// python `_scan` body: every folder under `root` that holds a SKILL.md, keyed by skill name.
/// the name comes from the frontmatter when present, otherwise the folder name is used.
/// a folder name that is not utf-8 is found and read by its real path; its index key is the
/// lossy name, since a Rust string cannot hold python's surrogateescape characters.
fn scan_root(root: &Path) -> anyhow::Result<Index> {
    let mut index = Index::new();
    if !root.is_dir() {
        return Ok(index);
    }
    let root_str = root.to_string_lossy().into_owned();
    let mut entries = listdir(&root_str).map_err(|e| anyhow!(e))?;
    entries.sort_by_key(|n| python_str_key(n));
    for entry in entries {
        let skill_path = root.join(&entry);
        if !skill_path.is_dir() {
            continue;
        }
        let skill_file = skill_path.join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        let mut name = entry.to_string_lossy().into_owned();
        // python: a SKILL.md that cannot be read or parsed keeps the folder name
        if let Ok(text) = util::read_file_text(&skill_file, &skill_file.to_string_lossy()) {
            if let Some(from_meta) = parse_frontmatter(&text).0.as_ref().and_then(frontmatter_name) {
                name = from_meta;
            }
        }
        if !index.contains_key(&name) {
            index.insert(name, skill_path);
        }
    }
    Ok(index)
}

/// python `_load_body`: the body of the named skill's SKILL.md, stripped
fn load_body(index: &Index, name: &str) -> Result<String, LoadError> {
    let Some(skill_path) = index.get(name) else {
        return Err(LoadError::NotFound);
    };
    let skill_file = skill_path.join("SKILL.md");
    let text = util::read_file_text(&skill_file, &skill_file.to_string_lossy()).map_err(LoadError::Read)?;
    let (_, body) = parse_frontmatter(&text);
    Ok(py_strip(&body).to_string())
}

/// the `name` argument of the `use` tool: python's `name not in self.index` hashes it first, so a JSON
/// array or object raises TypeError before any message is built; any other value is used as str()
fn use_name_arg(v: &Value) -> anyhow::Result<String> {
    match v {
        Value::Array(_) => anyhow::bail!("unhashable type: 'list'"),
        Value::Object(_) => anyhow::bail!("unhashable type: 'dict'"),
        _ => Ok(py_str(v)),
    }
}

/// `", ".join(self.index.keys()) if self.index else "none installed"`
fn available(index: &Index) -> String {
    if index.is_empty() {
        "none installed".to_string()
    } else {
        index.keys().map(String::as_str).collect::<Vec<_>>().join(", ")
    }
}

/// the failure message shared by the `use` tool and the `/skill` command
fn load_error_text(index: &Index, name: &str, err: LoadError) -> String {
    match err {
        LoadError::NotFound => format!("No skill named '{}'. Available skills: {}", name, available(index)),
        LoadError::Read(e) => format!("Could not read skill '{}': {}", name, e),
    }
}

impl Skills {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base, index: Mutex::new(None) }
    }

    fn with_index<R>(&self, f: impl FnOnce(&Index) -> R) -> anyhow::Result<R> {
        let guard = self.index.lock();
        let index = guard.as_ref().ok_or_else(|| anyhow!("'Skills' object has no attribute 'index'"))?;
        Ok(f(index))
    }

    /// python `_root_path`: the configured folder, expanded and sandboxed to the project root
    fn root_path(&self) -> anyhow::Result<String> {
        let folder = self.base.config.get1("folder").unwrap_or(Value::Null);
        let Some(folder) = folder.as_str() else {
            anyhow::bail!("expected str, bytes or os.PathLike object, not {}", util::py_type_name(&folder));
        };
        let p = rstrip_sep(&ol_core::paths::get_path(&expanduser(folder)?)?);
        Ok(p)
    }

    fn folder_set(&self) -> bool {
        self.base.config.get1("folder").map(|v| truthy(&v)).unwrap_or(false)
    }

    /// python `_scan`: rebuilds the index from disk and logs how many skills were found
    fn scan(&self) -> anyhow::Result<()> {
        *self.index.lock() = Some(Index::new());
        if !self.folder_set() {
            return Ok(());
        }
        let root = self.root_path()?;
        let index = scan_root(Path::new(&root))?;
        let count = index.len();
        *self.index.lock() = Some(index);
        self.base.log(&self.base.name, &format!("scanned: {} skill(s) loaded", count));
        Ok(())
    }

    /// python `use(name)`: the skill's instructions, or why they are not available
    pub fn use_skill(&self, name: &str) -> anyhow::Result<Value> {
        let loaded = self.with_index(|index| load_body(index, name).map_err(|e| load_error_text(index, name, e)))?;
        Ok(match loaded {
            Ok(body) => self.base.result(body, true),
            Err(msg) => self.base.result(msg, false),
        })
    }

    /// python `skills(args)`: `/skills` lists the installed skills, `/skills reload` rescans the folder
    fn cmd_skills(&self, args: &[String]) -> anyhow::Result<String> {
        if args.first().map(|a| a == "reload").unwrap_or(false) {
            self.scan()?;
            let count = self.with_index(|i| i.len())?;
            return Ok(format!("Skills reloaded ({} loaded)", count));
        }
        let names: Vec<String> = self.with_index(|i| i.keys().cloned().collect())?;
        if names.is_empty() {
            return Ok("No skills installed. Add skills, then use `/skills reload` or restart OpenLumara.".to_string());
        }
        let mut lines = vec![format!("Installed skills ({}):", names.len())];
        for name in names {
            lines.push(format!("  {}", name));
        }
        Ok(lines.join("\n"))
    }

    /// python `skill_command(args)`: `/skill <name>` loads the named skill into the conversation
    fn cmd_skill(&self, args: &[String]) -> anyhow::Result<String> {
        let Some(name) = args.first() else {
            return Ok("Please provide a skill to load".to_string());
        };
        let loaded = self.with_index(|index| load_body(index, name).map_err(|e| load_error_text(index, name, e)))?;
        Ok(match loaded {
            Ok(body) => format!("[Skill '{}' loaded - follow these instructions]\n{}", name, body),
            Err(msg) => msg,
        })
    }
}

#[async_trait]
impl Module for Skills {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    fn commands(&self) -> Vec<CommandSpec> {
        util::command_specs(meta::commands())
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "use" => {
                let name = use_name_arg(args.0.get("name").unwrap_or(&Value::Null))?;
                self.use_skill(&name)
            }
            _ => anyhow::bail!("'Skills' object has no attribute '{}'", method),
        }
    }

    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        match name {
            "skills" => Ok(Some(self.cmd_skills(&args)?)),
            "skill" => Ok(Some(self.cmd_skill(&args)?)),
            _ => anyhow::bail!("unknown command {}", name),
        }
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        *self.index.lock() = Some(Index::new());
        if self.folder_set() {
            let root = self.root_path()?;
            // python os.makedirs('') raises ENOENT, std does not
            if root.is_empty() {
                anyhow::bail!("[Errno 2] No such file or directory: ''");
            }
            // python os.makedirs() raises ValueError for a NUL in the path; std reports a different text
            if root.contains('\0') {
                anyhow::bail!("embedded null byte");
            }
            std::fs::create_dir_all(&root).map_err(|e| anyhow!(util::os_error(&e, &root)))?;
        }
        self.scan()
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        if !self.base.config.get_bool("insert_system_prompt") {
            return Ok(None);
        }
        let names: Vec<String> = self.with_index(|i| i.keys().cloned().collect())?;
        if names.is_empty() {
            return Ok(None);
        }
        Ok(Some(names.join(", ")))
    }

    fn ui_routes(&self) -> Vec<UiRoute> {
        meta::ui_routes().into_iter().map(|(r, _)| r).collect()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    /// a skills module whose index is the scan of a temp folder holding:
    /// alpha/SKILL.md (frontmatter name "Alpha Skill"), beta/SKILL.md (no frontmatter),
    /// gamma/ (no SKILL.md, ignored) and loose.txt (not a folder, ignored)
    fn setup() -> (Skills, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::write(
            root.join("alpha").join("SKILL.md"),
            "---\nname: Alpha Skill\ndescription: first\n---\n\n  Do the alpha thing.  \n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("beta")).unwrap();
        std::fs::write(root.join("beta").join("SKILL.md"), "Beta body\r\nline two\r\n").unwrap();
        std::fs::create_dir_all(root.join("gamma")).unwrap();
        std::fs::write(root.join("loose.txt"), "nope").unwrap();
        let m = Skills::new(ModuleBase::detached("skills"));
        *m.index.lock() = Some(scan_root(root).unwrap());
        (m, dir)
    }

    #[test]
    fn frontmatter_split_and_name() {
        let (meta, body) = parse_frontmatter("---\nname: Foo\n---\nhello\n");
        assert_eq!(body, "hello");
        assert_eq!(frontmatter_name(&meta.unwrap()), Some("Foo".to_string()));

        // yaml 1.1 words, which serde_yaml read as plain strings: python gives True / keeps the folder
        let (meta, _) = parse_frontmatter("---\nname: yes\n---\nbody");
        assert_eq!(frontmatter_name(&meta.unwrap()), Some("True".to_string()));
        let (meta, _) = parse_frontmatter("---\nname: no\n---\nbody");
        assert_eq!(frontmatter_name(&meta.unwrap()), None);

        // no opening delimiter, or no closing delimiter: the text comes back unchanged
        assert_eq!(parse_frontmatter("plain text"), (None, "plain text".to_string()));
        assert_eq!(parse_frontmatter("---\nname: x\n"), (None, "---\nname: x\n".to_string()));
    }

    #[test]
    fn scan_uses_frontmatter_name_and_folder_fallback() {
        let (m, _dir) = setup();
        let names: Vec<String> = m.with_index(|i| i.keys().cloned().collect()).unwrap();
        // sorted by folder name: alpha (named "Alpha Skill" by its frontmatter), then beta
        assert_eq!(names, vec!["Alpha Skill".to_string(), "beta".to_string()]);
    }

    #[test]
    fn use_tool_returns_stripped_body_and_exact_errors() {
        let (m, _dir) = setup();
        let ok = block_on(m.call_tool("use", ToolArgs::new(json!({"name": "Alpha Skill"}).as_object().unwrap().clone()))).unwrap();
        assert_eq!(ok, json!({"status": "success", "content": "Do the alpha thing."}));

        // CRLF line endings are read as \n, and the body is stripped
        let beta = block_on(m.call_tool("use", ToolArgs::new(json!({"name": "beta"}).as_object().unwrap().clone()))).unwrap();
        assert_eq!(beta, json!({"status": "success", "content": "Beta body\nline two"}));

        let missing = block_on(m.call_tool("use", ToolArgs::new(json!({"name": "nope"}).as_object().unwrap().clone()))).unwrap();
        assert_eq!(
            missing,
            json!({"status": "error", "content": "No skill named 'nope'. Available skills: Alpha Skill, beta"})
        );
    }

    #[test]
    fn use_tool_name_argument_follows_python_types() {
        let (m, _dir) = setup();
        // python: a list or dict name raises TypeError from the `in` check, so the call fails
        let err = block_on(m.call_tool("use", ToolArgs::new(json!({"name": ["beta"]}).as_object().unwrap().clone()))).unwrap_err();
        assert_eq!(err.to_string(), "unhashable type: 'list'");
        let err = block_on(m.call_tool("use", ToolArgs::new(json!({"name": {"a": 1}}).as_object().unwrap().clone()))).unwrap_err();
        assert_eq!(err.to_string(), "unhashable type: 'dict'");

        // python: an int, bool or None name is not in the index and gets str() text in the message
        let n = block_on(m.call_tool("use", ToolArgs::new(json!({"name": 5}).as_object().unwrap().clone()))).unwrap();
        assert_eq!(n, json!({"status": "error", "content": "No skill named '5'. Available skills: Alpha Skill, beta"}));
        let b = block_on(m.call_tool("use", ToolArgs::new(json!({"name": true}).as_object().unwrap().clone()))).unwrap();
        assert_eq!(b, json!({"status": "error", "content": "No skill named 'True'. Available skills: Alpha Skill, beta"}));
        let nul = block_on(m.call_tool("use", ToolArgs::new(json!({"name": null}).as_object().unwrap().clone()))).unwrap();
        assert_eq!(nul, json!({"status": "error", "content": "No skill named 'None'. Available skills: Alpha Skill, beta"}));
    }

    #[test]
    fn skill_and_skills_commands_text() {
        let (m, _dir) = setup();
        let loaded = block_on(m.run_command("skill", vec!["beta".to_string()])).unwrap();
        assert_eq!(loaded, Some("[Skill 'beta' loaded - follow these instructions]\nBeta body\nline two".to_string()));

        let none = block_on(m.run_command("skill", vec![])).unwrap();
        assert_eq!(none, Some("Please provide a skill to load".to_string()));

        let listing = block_on(m.run_command("skills", vec![])).unwrap();
        assert_eq!(listing, Some("Installed skills (2):\n  Alpha Skill\n  beta".to_string()));

        // an empty index has its own message, and the use tool reports "none installed"
        let empty = Skills::new(ModuleBase::detached("skills"));
        *empty.index.lock() = Some(Index::new());
        let msg = block_on(empty.run_command("skills", vec![])).unwrap();
        assert_eq!(
            msg,
            Some("No skills installed. Add skills, then use `/skills reload` or restart OpenLumara.".to_string())
        );
        let res = block_on(empty.call_tool("use", ToolArgs::new(json!({"name": "x"}).as_object().unwrap().clone()))).unwrap();
        assert_eq!(res, json!({"status": "error", "content": "No skill named 'x'. Available skills: none installed"}));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_folder_is_indexed_and_read_by_its_real_path() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join(OsStr::from_bytes(b"caf\xe9"));
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("SKILL.md"), "Latin body\n").unwrap();

        // python lists the folder (surrogateescape name); the index key is its lossy name, and the
        // SKILL.md is read through the real folder, so the skill is not dropped
        let index = scan_root(dir.path()).unwrap();
        assert_eq!(index.keys().cloned().collect::<Vec<_>>(), vec!["caf\u{fffd}".to_string()]);
        let m = Skills::new(ModuleBase::detached("skills"));
        *m.index.lock() = Some(index);
        let res = block_on(m.call_tool("use", ToolArgs::new(json!({"name": "caf\u{fffd}"}).as_object().unwrap().clone()))).unwrap();
        assert_eq!(res, json!({"status": "success", "content": "Latin body"}));
    }

    #[cfg(unix)]
    #[test]
    fn listed_names_sort_by_python_code_point_order() {
        use std::os::unix::ffi::OsStrExt;
        // python: "b" (U+0062) sorts before the surrogateescape of b"\xff" (U+DCFF), and "é" (U+00E9)
        // sorts after "b" but before that escape
        assert!(python_str_key(OsStr::new("b")) < python_str_key(OsStr::from_bytes(b"\xff")));
        assert_eq!(python_str_key(OsStr::from_bytes(b"\xc3\xa9")), vec![0xE9]);
        assert_eq!(python_str_key(OsStr::from_bytes(b"\xff")), vec![0xDCFF]);
    }
}

