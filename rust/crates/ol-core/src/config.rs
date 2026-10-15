//! Port of core/config.py.
//!
//! The config lives in `config.yml` at the project root (`--config some/path/name.yaml`
//! uses `some/path/name.yml`, exactly like the python StorageDict naming quirk).
//! Loading syncs the user config against the schema: core sections get defaults,
//! module/channel settings get merged with their schema defaults, settings of
//! modules that no longer exist are pruned, and enabled/disabled lists are reconciled.

use crate::pyjson::truthy;
use crate::storage::{StorageDict, StorageType};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::path::Path;

pub const CACHE_SECTIONS: [&str; 4] = ["channels", "user_channels", "modules", "user_modules"];

pub const DEFAULT_MODULES: [&str; 13] = [
    "tutorial",
    "docs",
    "identity",
    "models",
    "channel",
    "modules",
    "chats",
    "context",
    "memory",
    "scheduler",
    "calendar",
    "time",
    "auto_backup",
];

pub const DEFAULT_CHANNELS: [&str; 2] = ["cli", "webui"];

/// core_settings_schema
pub static CORE_SETTINGS_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "core": {
            "data_folder": {
                "default": "data",
                "description": "The folder where openlumara stores its data files (chats, memories, etc.)."
            },
            "auto_resume_chats": {
                "default": true,
                "description": "Whether to automatically resume the last active chat on startup."
            },
            "cmd_prefix": {
                "default": "/",
                "description": "The prefix used to determine whether something is a command. For example, when it's set to `/`, all commands must be prefixed with `/` as in `/help`. If you set it to `!`, it will be `!help` instead."
            },
            "tool_timeout": {
                "default": 30,
                "description": "Timeout in seconds for tool execution. This applies per individual tool, and is used to forcefully kill off tools that run too long."
            },
            "module_libraries": {
                "description": "A list of github urls that contain modules you want to be able to download and update in openlumara *(currently not implemented yet - stay tuned, this is the dev branch after all, lol)*",
                "default": []
            }
        },
        "api": {
            "url": {
                "default": "http://API_URL_HERE/v1",
                "description": "The base URL for the API endpoint. Get this from your chosen software (such as [llamacpp](https://llama.app/), [koboldcpp](https://koboldai.org/cpp), [lemonade](https://lemonade-server.ai/), or any other software you may prefer) or from your cloud API provider."
            },
            "key": {
                "default": "openlumara-dummy-key",
                "description": "The API key for authentication. For local, this is often unneeded unless you deliberately secured your server with an API key. When in doubt, leave it as default."
            },
            "max_context": {
                "default": 16768,
                "description": "Maximum number of tokens for the context window. This controls how much of your conversation your AI will remember. The higher the context, the further back it will remember, but how much context an AI supports depends on the model. Check your model's details page or modelcard for information on its supported context size."
            },
            "max_output_tokens": {
                "default": 8192,
                "description": "Maximum number of tokens for model output. This controls how much text your AI is allowed to generate. If you like short and sweet responses, set this to a low amount! But if you're coding or having it write entire books or something, you may want to up this value."
            },
            "max_messages": {
                "default": 200,
                "description": "Maximum number of messages to keep in conversation history. If this amount of messages is exceeded, it will trim the context window even if the token limit hasn't been reached!"
            },
            "use_developer_role": {
                "default": false,
                "description": "Enables the `developer` role for system prompts (see [this page](https://developers.openai.com/api/docs/guides/text#message-roles-and-instruction-following)). This helps the model distinguish between openlumara's instructions and your messages. Note: Not all models support this; unsupported models may crash if enabled."
            },
            "custom_fields": {
                "type": "object",
                "default": {},
                "description": "Additional custom fields to send with API requests. You can put anything in here that your chosen API supports!"
            }
        },
        "model": {
            "name": {
                "default": "",
                "description": "The name of the AI model to use."
            },
            "temperature": {
                "default": 0.7,
                "description": "Sampling temperature for the model (0.0 to 2.0)."
            },
            "use_tools": {
                "default": true,
                "description": "Enable tool/function calling for the model. Turn this off if you just wanna talk to the AI and don't care for all this agentic stuff! Essentially turns it into a chatbot that can't actually do anything, but it can still answer your questions."
            },
            "dynamic_tool_loading": {
                "default": true,
                "description": "When enabled, only a small selection of tools is loaded at first, but the AI can load them dynamically from enabled openlumara modules. When disabled, all available tools are loaded at startup for immediate use.",
                "depends": "use_tools"
            },
            "preloaded_modules": {
                "default": ["identity", "memory", "scheduler", "web_search", "web_reader"],
                "description": "List of module names whose tools are always preloaded at startup. Keeps frequently-used tools available immediately, everything else stays available via the dynamic tool loader.",
                "type": "list",
                "depends": "use_tools"
            },
            "automatically_compress_context": {
                "default": true,
                "description": "When enabled, context will automatically be compressed when the threshold (defined below) is hit. This will make the AI summarize the current state of your conversation, so that it can continue where it left off without having to reprocess the entire context with every request. This will let you have an 'infinite context window' of sorts where the AI can keep working on your request forever.\n\nWhen disabled, it will instead remove your old messages from context to make space for new ones - which is fine at small context sizes, but can be really slow at large context sizes. You will still have infinite context, but as soon as you hit the context window's size limit, requests will slow down massively."
            },
            "context_compression_threshold": {
                "default": 0.7,
                "description": "Percentage of the context that must be full in order for the automatic compression to trigger."
            },
            "enable_thinking": {
                "default": true,
                "description": "Enable reasoning (thinking) for the model."
            },
            "keep_reasoning_in_context": {
                "default": true,
                "description": "Keep the model's reasoning process in the conversation context. If you turn this off, the model won't remember its own thoughts, just the conclusions it drew. Can be useful for saving context.",
                "depends": "enable_thinking"
            },
            "only_preserve_reasoning_for_current_agentic_loop": {
                "default": true,
                "description": "An 'agentic loop' is a chain of multiple thoughts, toolcalls, and so on, until the AI reaches a conclusion. When you make a request to the AI, such as `search the web for kitty facts and write a summary of it to a note`, it will first think, then do the web search, then think again (sometimes), then write it to a note, and then it will tell you that it's done. That's an agentic loop! So when you enable this, the AI will forget the thoughts it had in previous agentic loops, which is a huge context/token saver.",
                "depends": {"enable_thinking": true, "keep_reasoning_in_context": true}
            },
            "reasoning_effort": {
                "default": "none",
                "type": "select",
                "description": "The reasoning effort level for the model. This controls how deeply the model will think before answering, but it only works on koboldcpp and some cloud API's. Set to none to outright disable sending this with the request.",
                "options": {
                    "none": 0,
                    "low": 1,
                    "medium": 2,
                    "high": 3,
                    "xhigh": 4,
                    "max": 5
                },
                "depends": "enable_thinking"
            }
        },
        "channels": {
            "enabled": {"default": [], "description": "List of enabled channel names.", "type": "list"},
            "disabled": {"default": [], "description": "List of disabled channel names.", "type": "list"},
            "settings": {"default": {}, "description": "Per-channel settings, keyed by channel name."}
        },
        "user_channels": {
            "path": {"default": "user_channels", "description": "Directory path for user-defined channels."},
            "enabled": {"default": [], "description": "List of enabled user channel names.", "type": "list"},
            "disabled": {"default": [], "description": "List of disabled user channel names.", "type": "list"},
            "settings": {"default": {}, "description": "Per-user-channel settings, keyed by channel name."}
        },
        "modules": {
            "enabled": {"default": [], "description": "List of enabled module names.", "type": "list"},
            "disabled": {"default": [], "description": "List of disabled module names.", "type": "list"},
            "settings": {"default": {}, "description": "Per-module settings, keyed by module name."}
        },
        "user_modules": {
            "path": {"default": "user_modules", "description": "Directory path for user-defined modules."},
            "enabled": {"default": [], "description": "List of enabled user module names.", "type": "list"},
            "disabled": {"default": [], "description": "List of disabled user module names.", "type": "list"},
            "settings": {"default": {}, "description": "Per-user-module settings, keyed by module name."}
        }
    })
});

/// Recursively flattens a settings dictionary by extracting 'default' values.
pub fn flatten_settings(v: &Value) -> Value {
    match v {
        Value::Object(m) if m.contains_key("default") => flatten_settings(&m["default"]),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), flatten_settings(v))).collect()),
        other => other.clone(),
    }
}

pub static DEFAULT_CONFIG: Lazy<Value> = Lazy::new(|| flatten_settings(&CORE_SETTINGS_SCHEMA));

// ---------------------------------------------------------------------------------------
// global config storage
// ---------------------------------------------------------------------------------------

static CONFIG: Lazy<Mutex<Option<StorageDict>>> = Lazy::new(|| Mutex::new(None));

/// core.config.get(*keys): no reload from disk. returns None when the path doesn't exist.
pub fn get(keys: &[&str]) -> Option<Value> {
    let guard = CONFIG.lock();
    let root = match guard.as_ref() {
        Some(c) if !c.data.is_empty() => Value::Object(c.data.clone()),
        _ => DEFAULT_CONFIG.clone(),
    };
    drop(guard);
    if keys.is_empty() {
        return Some(root);
    }
    crate::pyjson::get_in(&root, keys).cloned()
}

pub fn get_or(keys: &[&str], default: Value) -> Value {
    get(keys).unwrap_or(default)
}

pub fn get_str(keys: &[&str]) -> Option<String> {
    get(keys).and_then(|v| v.as_str().map(|s| s.to_string()))
}

pub fn get_bool(keys: &[&str], default: bool) -> bool {
    get(keys).map(|v| truthy(&v)).unwrap_or(default)
}

pub fn get_i64(keys: &[&str], default: i64) -> i64 {
    get(keys).and_then(|v| crate::pyjson::as_i64(&v)).unwrap_or(default)
}

pub fn get_f64(keys: &[&str], default: f64) -> f64 {
    get(keys).and_then(|v| crate::pyjson::as_f64(&v)).unwrap_or(default)
}

pub fn get_str_list(keys: &[&str]) -> Vec<String> {
    match get(keys) {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
        _ => Vec::new(),
    }
}

/// dict(core.config.config)
pub fn snapshot() -> Map<String, Value> {
    CONFIG.lock().as_ref().map(|c| c.data.clone()).unwrap_or_default()
}

pub fn is_loaded() -> bool {
    CONFIG.lock().is_some()
}

pub fn config_path() -> Option<String> {
    CONFIG.lock().as_ref().map(|c| c.path.clone())
}

/// reload from disk if the file changed (core.config.config.load())
pub fn reload() {
    if let Some(c) = CONFIG.lock().as_mut() {
        c.load();
    }
}

/// mutate the live config and save it to disk
pub fn update<R>(f: impl FnOnce(&mut Map<String, Value>) -> R) -> R {
    let mut guard = CONFIG.lock();
    if guard.is_none() {
        let mut empty = StorageDict::with_options("config", StorageType::Yaml, Some(&crate::paths::project_root().to_string_lossy()), false, true)
            .expect("config storage");
        empty.data = match DEFAULT_CONFIG.clone() {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        *guard = Some(empty);
    }
    let cfg = guard.as_mut().unwrap();
    let r = f(&mut cfg.data);
    if let Err(e) = cfg.save() {
        crate::log("error", &format!("error saving config: {}", e));
    }
    r
}

/// replace the whole config (config.load(data=...)) and save
pub fn replace(data: Map<String, Value>) {
    update(|m| *m = data);
}

/// install an in-memory config (tests / temporary)
pub fn set_for_tests(data: Value) {
    let dir = std::env::temp_dir().join(format!("ol-test-config-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut s = StorageDict::with_options("config", StorageType::Yaml, Some(&dir.to_string_lossy()), false, true).unwrap();
    s.data = data.as_object().cloned().unwrap_or_default();
    *CONFIG.lock() = Some(s);
}

/// ConfigManager: a view onto a sub-path of the config (e.g. modules.settings.memory)
#[derive(Clone, Debug)]
pub struct ConfigView {
    base: Vec<String>,
}

impl ConfigView {
    pub fn new(base: &[&str]) -> Self {
        Self { base: base.iter().map(|s| s.to_string()).collect() }
    }

    fn resolve(root: &Map<String, Value>, base: &[String]) -> Option<Value> {
        let mut cur = Value::Object(root.clone());
        for k in base {
            cur = cur.as_object()?.get(k)?.clone();
        }
        Some(cur)
    }

    /// get a nested value relative to the base path, reloading from disk first
    pub fn get(&self, keys: &[&str]) -> Option<Value> {
        reload();
        let root = snapshot();
        let mut cur = Self::resolve(&root, &self.base)?;
        for k in keys {
            cur = cur.as_object()?.get(*k)?.clone();
        }
        Some(cur)
    }

    pub fn get1(&self, key: &str) -> Option<Value> {
        self.get(&[key])
    }

    pub fn get_or(&self, key: &str, default: Value) -> Value {
        self.get1(key).unwrap_or(default)
    }

    pub fn get_bool(&self, key: &str, default: bool) -> bool {
        self.get1(key).map(|v| truthy(&v)).unwrap_or(default)
    }

    pub fn get_str(&self, key: &str) -> Option<String> {
        self.get1(key).and_then(|v| v.as_str().map(|s| s.to_string()))
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get1(key).and_then(|v| crate::pyjson::as_i64(&v))
    }

    /// set a key to a value (creates the base path) and save
    pub fn set(&self, key: &str, value: Value) {
        let base = self.base.clone();
        update(|root| {
            let mut cur = root;
            for k in &base {
                let entry = cur.entry(k.clone()).or_insert_with(|| Value::Object(Map::new()));
                if !entry.is_object() {
                    *entry = Value::Object(Map::new());
                }
                cur = entry.as_object_mut().unwrap();
            }
            cur.insert(key.to_string(), value);
        });
    }

    pub fn to_dict(&self) -> Map<String, Value> {
        reload();
        let root = snapshot();
        match Self::resolve(&root, &self.base) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }

    pub fn contains(&self, key: &str) -> bool {
        let root = snapshot();
        matches!(Self::resolve(&root, &self.base), Some(Value::Object(m)) if m.contains_key(key))
    }
}

// ---------------------------------------------------------------------------------------
// discovery
// ---------------------------------------------------------------------------------------

const ENTRY_FILENAMES: [&str; 2] = ["module.py", "channel.py"];

/// list module/channel folder names on disk (folder-only layout; must contain module.py
/// or channel.py, must not contain __init__.py)
pub fn discover_folder_names(folder: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(rd) = std::fs::read_dir(folder) else { return names };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('_') || name.starts_with('.') {
            continue;
        }
        let p = e.path();
        if !p.is_dir() || p.join("__init__.py").exists() {
            continue;
        }
        if ENTRY_FILENAMES.iter().any(|f| p.join(f).is_file()) {
            names.push(name);
        }
    }
    names.sort();
    names
}

pub fn section_folder(section: &str) -> std::path::PathBuf {
    crate::paths::project_root().join(section)
}

/// names available in a config section: compiled-in registry entries plus anything
/// discovered on disk (so settings of unported/python-only items are preserved)
pub fn available_names(section: &str) -> Vec<String> {
    let mut names: Vec<String> = discover_folder_names(&section_folder(section));
    let reg = crate::registry::get();
    match section {
        "modules" => names.extend(reg.modules.keys().cloned()),
        "channels" => names.extend(reg.channels.keys().cloned()),
        _ => {}
    }
    names.sort();
    names.dedup();
    names
}

/// whether a section item can actually be loaded by the Rust port
pub fn is_compiled_in(section: &str, name: &str) -> bool {
    match section {
        "modules" => crate::registry::module(name).is_some(),
        "channels" => crate::registry::channel(name).is_some(),
        _ => false,
    }
}

fn schema_for(section: &str, name: &str) -> Option<Value> {
    match section {
        "modules" if crate::registry::module(name).is_some() => Some(crate::registry::module_settings(name)),
        "channels" if crate::registry::channel(name).is_some() => Some(crate::registry::channel_settings(name)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// static metadata extraction for items that are not compiled in
// ---------------------------------------------------------------------------------------

pub struct StaticMeta {
    pub docstring: String,
    pub unsafe_: bool,
    pub category: String,
}

/// statically read docstring/unsafe/category off the first module-looking class of a
/// python file, without importing it (lightweight version of the python AST fallback)
pub fn extract_class_metadata_from_file(path: &Path) -> Option<StaticMeta> {
    let src = std::fs::read_to_string(path).ok()?;
    let class_rx = regex::Regex::new(r"(?m)^class\s+\w+[^\n]*:\s*$").unwrap();
    let m = class_rx.find(&src)?;
    let body = &src[m.end()..];
    let doc_rx = regex::Regex::new(r#"^\s*(?:"""([\s\S]*?)"""|'''([\s\S]*?)''')"#).unwrap();
    let docstring = doc_rx
        .captures(body)
        .and_then(|c| c.get(1).or(c.get(2)))
        .map(|d| crate::registry::clean_doc(d.as_str()))
        .unwrap_or_default();
    // only look at the class body (until the next top-level statement)
    let end = regex::Regex::new(r"(?m)^\S").unwrap().find(body).map(|m| m.start()).unwrap_or(body.len());
    let class_body = &body[..end];
    let unsafe_ = regex::Regex::new(r"(?m)^\s+unsafe\s*=\s*True").unwrap().is_match(class_body);
    let category = regex::Regex::new(r#"(?m)^\s+category\s*=\s*["']([^"']*)["']"#)
        .unwrap()
        .captures(class_body)
        .map(|c| c[1].to_string())
        .unwrap_or_default();
    Some(StaticMeta { docstring, unsafe_, category })
}

fn entry_file(section: &str, name: &str) -> Option<std::path::PathBuf> {
    let dir = section_folder(section).join(name);
    ENTRY_FILENAMES.iter().map(|f| dir.join(f)).find(|p| p.is_file())
}

// ---------------------------------------------------------------------------------------
// schema / structure
// ---------------------------------------------------------------------------------------

/// Recursively merges user_config with the core settings schema.
fn merge_core_settings(user: Option<&Value>, schema: &Value) -> Value {
    let Value::Object(sm) = schema else { return user.cloned().unwrap_or(Value::Null) };
    if sm.contains_key("default") {
        if let Some(Value::Object(um)) = user {
            if um.contains_key("default") {
                return sm["default"].clone();
            }
        }
        return match user {
            Some(v) if !v.is_null() => v.clone(),
            _ => sm["default"].clone(),
        };
    }
    let empty = Map::new();
    let um = match user {
        Some(Value::Object(m)) => m,
        _ => &empty,
    };
    let mut out = Map::new();
    for (k, v) in sm {
        if um.contains_key(k) {
            out.insert(k.clone(), merge_core_settings(um.get(k), v));
        } else {
            out.insert(k.clone(), flatten_settings(v));
        }
    }
    Value::Object(out)
}

/// Returns the config schema (flattened defaults), including settings for every
/// available module/channel, so that settings of disabled modules persist.
pub fn get_schema() -> Map<String, Value> {
    let mut schema = match merge_core_settings(None, &CORE_SETTINGS_SCHEMA) {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    for section in CACHE_SECTIONS {
        let sec = schema.entry(section.to_string()).or_insert_with(|| json!({}));
        if !sec.is_object() {
            *sec = json!({});
        }
        let settings = sec.as_object_mut().unwrap().entry("settings".to_string()).or_insert_with(|| json!({}));
        if !settings.is_object() {
            *settings = json!({});
        }
        let settings = settings.as_object_mut().unwrap();
        for name in available_names(section) {
            if let Some(s) = schema_for(section, &name) {
                settings.insert(name.clone(), flatten_settings(&s));
            }
        }
    }
    schema
}

/// flat dict of settings + metadata for every available module/channel:
/// `{name: {"settings": {...schema...}, "metadata": {"doc", "unsafe", "category", "type"}}}`
pub fn get_module_structure() -> IndexMap<String, Value> {
    let mut out = IndexMap::new();
    let type_map = [("channels", "channel"), ("user_channels", "user_channel"), ("modules", "module"), ("user_modules", "user_module")];
    for (section, type_str) in type_map {
        for name in available_names(section) {
            let (settings, doc, unsafe_, category) = match section {
                "modules" if crate::registry::module(&name).is_some() => {
                    let d = crate::registry::module(&name).unwrap();
                    (crate::registry::module_settings(&name), crate::registry::clean_doc(d.doc), d.unsafe_, d.category.to_string())
                }
                "channels" if crate::registry::channel(&name).is_some() => {
                    let d = crate::registry::channel(&name).unwrap();
                    (crate::registry::channel_settings(&name), crate::registry::clean_doc(d.doc), d.unsafe_, d.category.to_string())
                }
                _ => {
                    // not compiled in: read what we can from the python file
                    let meta = entry_file(section, &name).and_then(|p| extract_class_metadata_from_file(&p));
                    match meta {
                        Some(m) => (json!({}), m.docstring, m.unsafe_, m.category),
                        None => (json!({}), String::new(), false, String::new()),
                    }
                }
            };
            out.insert(
                name.clone(),
                json!({
                    "settings": settings,
                    "metadata": {"doc": doc, "unsafe": unsafe_, "category": category, "type": type_str}
                }),
            );
        }
    }
    out
}

/// the core settings schema in the frontend's settings-UI format
pub fn get_core_settings_structure() -> Map<String, Value> {
    let mut structure = Map::new();
    let Value::Object(schema) = &*CORE_SETTINGS_SCHEMA else { return structure };
    for (section_key, section_schema) in schema {
        let Value::Object(fields) = section_schema else { continue };
        let mut settings = Map::new();
        for (field_key, field_schema) in fields {
            let Value::Object(fs) = field_schema else { continue };
            settings.insert(
                field_key.clone(),
                json!({
                    "default": fs.get("default").cloned().unwrap_or(Value::Null),
                    "description": fs.get("description").cloned().unwrap_or(json!("")),
                    "type": fs.get("type").cloned().unwrap_or(Value::Null),
                    "options": fs.get("options").cloned().unwrap_or(Value::Null),
                    "unsafe": fs.get("unsafe").cloned().unwrap_or(json!(false)),
                    "depends": fs.get("depends").cloned().unwrap_or(Value::Null),
                    "min": fs.get("min").cloned().unwrap_or(Value::Null),
                    "max": fs.get("max").cloned().unwrap_or(Value::Null),
                    "step": fs.get("step").cloned().unwrap_or(Value::Null),
                }),
            );
        }
        structure.insert(
            section_key.clone(),
            json!({
                "settings": settings,
                "metadata": {
                    "doc": format!("Configure {}", crate::pyjson::title(&section_key.replace('_', " "))),
                    "unsafe": false
                }
            }),
        );
    }
    structure
}

/// Recursively syncs structural keys from the schema.
pub fn sync_config(user: &Value, schema: &Value) -> Value {
    let (Value::Object(sm), Value::Object(um)) = (schema, user) else { return schema.clone() };
    let mut result = um.clone();
    for (key, schema_val) in sm {
        if let Some(user_val) = result.get(key).cloned() {
            let empty = match schema_val {
                Value::Object(m) => m.is_empty(),
                Value::Array(a) => a.is_empty(),
                _ => false,
            };
            if empty {
                continue;
            }
            if schema_val.is_object() && user_val.is_object() {
                result.insert(key.clone(), sync_config(&user_val, schema_val));
            }
        } else {
            result.insert(key.clone(), schema_val.clone());
        }
    }
    Value::Object(result)
}

/// Updates the enabled/disabled lists based on discovery.
pub fn reconcile_lists(available: &[String], defaults: &[&str], section: &Value) -> (Vec<String>, Vec<String>) {
    use std::collections::BTreeSet;
    let avail: BTreeSet<&str> = available.iter().map(|s| s.as_str()).collect();
    let defaults: BTreeSet<&str> = defaults.iter().copied().collect();
    let list = |k: &str| -> BTreeSet<String> {
        section
            .get(k)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default()
    };
    let enabled: BTreeSet<String> = list("enabled").into_iter().filter(|n| avail.contains(n.as_str())).collect();
    let disabled: BTreeSet<String> = list("disabled").into_iter().filter(|n| avail.contains(n.as_str())).collect();
    let mut new_enabled = enabled.clone();
    let mut new_disabled = disabled.clone();
    for item in &avail {
        if enabled.contains(*item) || disabled.contains(*item) {
            continue;
        }
        if defaults.contains(item) {
            new_enabled.insert(item.to_string());
        } else {
            new_disabled.insert(item.to_string());
        }
    }
    (new_enabled.into_iter().collect(), new_disabled.into_iter().collect())
}

/// Recursively merges current_settings with module_defaults schema.
fn merge_module_settings(current: Option<&Value>, defaults: &Value) -> Value {
    if let Value::Object(dm) = defaults {
        if dm.contains_key("default") {
            if let Some(Value::Object(cm)) = current {
                if cm.contains_key("default") {
                    return dm["default"].clone();
                }
            }
            return match current {
                Some(v) if !v.is_null() => v.clone(),
                _ => dm["default"].clone(),
            };
        }
    } else {
        return match current {
            Some(v) if !v.is_null() => v.clone(),
            _ => defaults.clone(),
        };
    }
    let dm = defaults.as_object().unwrap();
    let empty = Map::new();
    let cm = match current {
        Some(Value::Object(m)) => m,
        _ => &empty,
    };
    let mut out = Map::new();
    for (k, v) in dm {
        if cm.contains_key(k) {
            out.insert(k.clone(), merge_module_settings(cm.get(k), v));
        } else {
            out.insert(k.clone(), flatten_settings(v));
        }
    }
    Value::Object(out)
}

/// deep pruning and merging of module settings for one section
fn sync_module_settings(config: &mut Map<String, Value>, instances: &[(String, Value)], section: &str, available: &[String]) {
    let sec = config.entry(section.to_string()).or_insert_with(|| json!({}));
    if !sec.is_object() {
        *sec = json!({});
    }
    let settings = sec.as_object_mut().unwrap().entry("settings".to_string()).or_insert_with(|| json!({}));
    if !settings.is_object() {
        *settings = json!({});
    }
    let settings = settings.as_object_mut().unwrap();

    // 1. remove settings for modules that are no longer available
    let names: Vec<String> = settings.keys().cloned().collect();
    for name in names {
        if !available.contains(&name) {
            settings.shift_remove(&name);
        }
    }

    // 2. merge defaults for enabled (instantiated) modules
    for (name, defaults) in instances {
        let Value::Object(dm) = defaults else { continue };
        if let Some(existing @ Value::Object(_)) = settings.get(name) {
            let merged = merge_module_settings(Some(existing), defaults);
            if merged.as_object().map(|m| m.is_empty()).unwrap_or(false) {
                settings.shift_remove(name);
            } else {
                settings.insert(name.clone(), merged);
            }
        } else if !dm.is_empty() {
            let flat = flatten_settings(defaults);
            if flat.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
                settings.insert(name.clone(), flat);
            }
        }
    }
}

/// Load the config file (core.config.load)
pub fn load(file_path: Option<&str>) -> anyhow::Result<()> {
    let (filename, dirname) = match file_path {
        Some(p) => {
            let path = Path::new(p);
            let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "config".into());
            let dir = path.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
            let dir = if dir.is_empty() { std::env::current_dir()?.to_string_lossy().to_string() } else { dir };
            let dir = if Path::new(&dir).is_absolute() { dir } else { std::env::current_dir()?.join(dir).to_string_lossy().to_string() };
            (stem, dir)
        }
        None => ("config".to_string(), crate::paths::project_root().to_string_lossy().to_string()),
    };

    let mut storage = StorageDict::with_options(&filename, StorageType::Yaml, Some(&dirname), true, true)?;
    let new_config = storage.data.is_empty();
    let raw = Value::Object(storage.data.clone());

    let list_of = |section: &str| -> Vec<String> {
        raw.get(section)
            .and_then(|s| s.get("enabled"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default()
    };
    let mut enabled_channels = list_of("channels");
    if enabled_channels.is_empty() && new_config {
        enabled_channels = DEFAULT_CHANNELS.iter().map(|s| s.to_string()).collect();
    }
    let mut enabled_modules = list_of("modules");
    if enabled_modules.is_empty() && new_config {
        enabled_modules = DEFAULT_MODULES.iter().map(|s| s.to_string()).collect();
    }

    let schema = Value::Object(get_schema());
    let mut target = if new_config {
        schema.clone()
    } else {
        sync_config(&raw, &schema)
    };
    let target_map = target.as_object_mut().unwrap();

    for section in CACHE_SECTIONS {
        let available = available_names(section);
        let (enabled_list, defaults): (Vec<String>, Vec<&str>) = match section {
            "channels" => (enabled_channels.clone(), DEFAULT_CHANNELS.to_vec()),
            "modules" => (enabled_modules.clone(), DEFAULT_MODULES.to_vec()),
            _ => (Vec::new(), Vec::new()),
        };
        // instances: enabled items that are compiled in, with their schema
        let instances: Vec<(String, Value)> = enabled_list
            .iter()
            .filter(|n| is_compiled_in(section, n))
            .filter_map(|n| schema_for(section, n).map(|s| (n.clone(), s)))
            .collect();
        // python only imports instances for the four sections in a fixed alphabetical order
        let mut instances = instances;
        instances.sort_by(|a, b| a.0.cmp(&b.0));
        sync_module_settings(target_map, &instances, section, &available);

        let sec_val = target_map.get(section).cloned().unwrap_or(json!({}));
        let (en, dis) = reconcile_lists(&available, &defaults, &sec_val);
        let sec = target_map.get_mut(section).unwrap().as_object_mut().unwrap();
        sec.insert("enabled".into(), json!(en));
        sec.insert("disabled".into(), json!(dis));
    }

    storage.load_data(target.as_object().cloned().unwrap_or_default());
    storage.save()?;
    if new_config {
        println!("A new configuration file has been created at {}.", storage.path);
    }
    *CONFIG.lock() = Some(storage);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_defaults() {
        let d = &*DEFAULT_CONFIG;
        assert_eq!(d["api"]["url"], json!("http://API_URL_HERE/v1"));
        assert_eq!(d["model"]["reasoning_effort"], json!("none"));
        assert_eq!(d["channels"]["settings"], json!({}));
        let keys: Vec<&String> = d.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["core", "api", "model", "channels", "user_channels", "modules", "user_modules"]);
    }

    #[test]
    fn reconcile() {
        let avail = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (en, dis) = reconcile_lists(&avail, &["b"], &json!({"enabled": ["a", "zzz"], "disabled": []}));
        assert_eq!(en, vec!["a", "b"]);
        assert_eq!(dis, vec!["c"]);
    }

    #[test]
    fn sync_keeps_user_values() {
        let schema = json!({"api": {"url": "x", "custom_fields": {}}, "model": {"name": ""}});
        let user = json!({"api": {"url": "mine", "custom_fields": {"a": 1}}});
        let r = sync_config(&user, &schema);
        assert_eq!(r, json!({"api": {"url": "mine", "custom_fields": {"a": 1}}, "model": {"name": ""}}));
    }

    #[test]
    fn module_settings_merge() {
        let defaults = json!({"a": {"default": 1}, "b": {"default": "x"}});
        let merged = merge_module_settings(Some(&json!({"a": 5, "zzz": 1})), &defaults);
        assert_eq!(merged, json!({"a": 5, "b": "x"}));
    }

    #[test]
    fn static_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("module.py");
        std::fs::write(&p, "import core\n\nclass Foo(core.module.Module):\n    \"\"\"Does foo\n\n    more text\n    \"\"\"\n    unsafe = True\n    category = \"tools\"\n\n    async def x(self):\n        pass\n").unwrap();
        let m = extract_class_metadata_from_file(&p).unwrap();
        assert_eq!(m.docstring, "Does foo\n\nmore text");
        assert!(m.unsafe_);
        assert_eq!(m.category, "tools");
    }
}

/// replace the live config without saving it (command line overrides)
pub fn replace_in_memory(data: Map<String, Value>) {
    if let Some(c) = CONFIG.lock().as_mut() {
        c.data = data;
    }
}
