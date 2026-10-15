//! Port of core/tool_loader.py: tool catalog, active tool set, and the `tools_load` meta tool.

use crate::channel::Channel;
use crate::config;
use crate::module::{Module, ToolSpec};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, Weak};

pub const META_TOOL_NAMES: [&str; 1] = ["tools_load"];

const ARG_SECTION: &str = "Args:";
const END_SECTIONS: [&str; 4] = ["Returns:", "Raises:", "Note:", "Example:"];
static PARAM_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(\w+)(?:\s*\([^)]*\))?\s*:\s*(.+)").unwrap());

fn starts_with_section(s: &str) -> bool {
    s.starts_with(ARG_SECTION) || END_SECTIONS.iter().any(|e| s.starts_with(e))
}

/// Parse a Google-style docstring into (param_descriptions, clean_description).
pub fn parse_tool_docstring(docstring: &str) -> (IndexMap<String, String>, String) {
    if docstring.is_empty() {
        return (IndexMap::new(), String::new());
    }
    let lines: Vec<&str> = docstring.split('\n').collect();

    let mut clean_lines: Vec<&str> = Vec::new();
    let mut skip = false;
    for line in &lines {
        let stripped = line.trim();
        if starts_with_section(stripped) {
            skip = true;
        } else if skip && (stripped.is_empty() || !line.chars().next().map(|c| c.is_whitespace()).unwrap_or(false)) {
            skip = false;
            if !stripped.is_empty() {
                clean_lines.push(line);
            }
        } else if !skip {
            clean_lines.push(line);
        }
    }

    let mut descriptions = IndexMap::new();
    let mut in_args = false;
    let mut current_param: Option<String> = None;
    let mut current_desc: Vec<String> = Vec::new();
    for line in &lines {
        let stripped = line.trim();
        if stripped.starts_with(ARG_SECTION) {
            in_args = true;
        } else if in_args && END_SECTIONS.contains(&stripped) {
            break;
        } else if in_args && !stripped.is_empty() {
            if let Some(c) = PARAM_RE.captures(stripped) {
                if let Some(p) = current_param.take() {
                    if !current_desc.is_empty() {
                        descriptions.insert(p, current_desc.join(" "));
                    }
                }
                current_param = Some(c[1].to_string());
                current_desc = vec![c[2].to_string()];
            } else if current_param.is_some() {
                current_desc.push(stripped.to_string());
            }
        }
    }
    if let Some(p) = current_param {
        if !current_desc.is_empty() {
            descriptions.insert(p, current_desc.join(" "));
        }
    }
    (descriptions, clean_lines.join("\n").trim().to_string())
}

/// Build an API tool dict from a tool spec
pub fn tool_dict_from_spec(spec: &ToolSpec, tool_name: &str) -> Value {
    let (param_desc, doc) = parse_tool_docstring(spec.doc);
    let mut properties = Map::new();
    let mut required = Vec::new();
    for p in &spec.params {
        let mut prop = Map::new();
        prop.insert("type".into(), json!(p.ty.as_str()));
        if let Some(d) = param_desc.get(p.name) {
            if !d.is_empty() {
                prop.insert("description".into(), json!(d));
            }
        }
        properties.insert(p.name.to_string(), Value::Object(prop));
        if p.required {
            required.push(json!(p.name));
        }
    }
    let mut function = Map::new();
    function.insert("name".into(), json!(tool_name));
    function.insert(
        "parameters".into(),
        json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false}),
    );
    function.insert("strict".into(), json!(false));
    if !doc.is_empty() {
        function.insert("description".into(), json!(doc));
    }
    json!({"type": "function", "function": function})
}

#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub tool: Value,
    pub module: String,
    pub method: String,
    pub description: String,
}

#[derive(Default)]
struct State {
    catalog: IndexMap<String, CatalogEntry>,
    active_tools: Vec<Value>,
    active_names: Vec<String>,
    meta_def: Option<Value>,
}

pub struct ToolLoader {
    ch: Weak<Channel>,
    state: Mutex<State>,
}

fn tool_name_of(t: &Value) -> &str {
    t.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("")
}

impl ToolLoader {
    pub fn new(ch: Weak<Channel>) -> Self {
        Self { ch, state: Mutex::new(State::default()) }
    }

    fn ch(&self) -> Option<Arc<Channel>> {
        self.ch.upgrade()
    }

    fn modules(&self) -> IndexMap<String, Arc<dyn Module>> {
        self.ch().map(|c| c.manager().modules()).unwrap_or_default()
    }

    pub fn active_tools(&self) -> Vec<Value> {
        self.state.lock().active_tools.clone()
    }
    pub fn active_names(&self) -> Vec<String> {
        self.state.lock().active_names.clone()
    }
    pub fn set_active(&self, tools: Vec<Value>, names: Vec<String>) {
        let mut s = self.state.lock();
        s.active_tools = tools;
        s.active_names = names;
    }
    pub fn catalog(&self) -> IndexMap<String, CatalogEntry> {
        self.state.lock().catalog.clone()
    }
    pub fn catalog_entry(&self, name: &str) -> Option<CatalogEntry> {
        self.state.lock().catalog.get(name).cloned()
    }
    pub fn is_meta_tool(name: &str) -> bool {
        META_TOOL_NAMES.contains(&name)
    }

    fn loadable(&self, entry: &CatalogEntry, modules: &IndexMap<String, Arc<dyn Module>>) -> bool {
        match modules.get(&entry.module) {
            Some(m) => !m.disabled_tools().contains(&entry.method),
            None => false,
        }
    }

    fn preloaded_modules() -> BTreeSet<String> {
        match config::get(&["model", "preloaded_modules"]) {
            Some(Value::Array(a)) => a.iter().map(|v| crate::pyjson::py_str(v).to_lowercase()).collect(),
            _ => BTreeSet::new(),
        }
    }

    fn default_tools(state: &State) -> Vec<(String, CatalogEntry)> {
        let pre = Self::preloaded_modules();
        state.catalog.iter().filter(|(_, e)| pre.contains(&e.module)).map(|(k, e)| (k.clone(), e.clone())).collect()
    }

    /// scan a module for tool methods and add them to the catalog
    pub fn register_module(&self, module: &Arc<dyn Module>) {
        let disabled = module.disabled_tools();
        let name = module.name().to_string();
        let mut s = self.state.lock();
        for spec in module.tools() {
            if spec.method.starts_with('_') || disabled.iter().any(|d| d == spec.method) {
                continue;
            }
            if spec.method == "result" || spec.method.starts_with("on_") {
                continue;
            }
            let tool_name = format!("{}_{}", name, spec.method);
            let tool = tool_dict_from_spec(&spec, &tool_name);
            let description = tool["function"].get("description").and_then(|d| d.as_str()).unwrap_or("").to_string();
            s.catalog.insert(tool_name, CatalogEntry { tool, module: name.clone(), method: spec.method.to_string(), description });
        }
    }

    /// remove all catalog entries and active tools for a module
    pub fn unregister_module(&self, module_name: &str) {
        let prefix = format!("{}_", module_name);
        let mut s = self.state.lock();
        s.catalog.retain(|k, _| !k.starts_with(&prefix));
        s.active_tools.retain(|t| !tool_name_of(t).starts_with(&prefix));
        s.active_names.retain(|n| !n.starts_with(&prefix));
    }

    fn enabled_module_names(&self, state: &State) -> Vec<String> {
        let with_tools: BTreeSet<&String> = state.catalog.values().map(|e| &e.module).collect();
        let mut names: Vec<String> = self.modules().keys().filter(|n| with_tools.contains(n)).cloned().collect();
        names.sort();
        names
    }

    fn build_meta_def(&self, state: &State) -> Value {
        let names = self.enabled_module_names(state);
        let modules = if names.is_empty() { "(none currently enabled)".to_string() } else { names.join(", ") };
        json!({
            "type": "function",
            "function": {
                "name": "tools_load",
                "description": format!("Loads all tools belonging to one or more modules into your active toolset. Currently enabled modules (pass these names in module_names): {}.", modules),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "module_names": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Names of the modules whose tools should be loaded"
                        }
                    },
                    "required": ["module_names"],
                    "additionalProperties": false
                },
                "strict": false
            }
        })
    }

    /// register the tools_load meta tool
    pub fn register_meta_tools(&self) {
        if !config::get_bool(&["model", "dynamic_tool_loading"], true) {
            return;
        }
        let mut s = self.state.lock();
        if s.meta_def.is_some() {
            return;
        }
        // channel isn't fully constructed yet at this point in python either: no modules
        let def = json!({
            "type": "function",
            "function": {
                "name": "tools_load",
                "description": "Loads all tools belonging to one or more modules into your active toolset. Currently enabled modules (pass these names in module_names): (none currently enabled).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "module_names": {"type": "array", "items": {"type": "string"}, "description": "Names of the modules whose tools should be loaded"}
                    },
                    "required": ["module_names"],
                    "additionalProperties": false
                },
                "strict": false
            }
        });
        s.meta_def = Some(def.clone());
        s.active_tools = vec![def];
        s.active_names = vec!["tools_load".into()];
    }

    /// rebuild the dynamic tools_load description with the current module list
    pub fn refresh_meta_tool_descriptions(&self) {
        let mut s = self.state.lock();
        if s.meta_def.is_none() {
            return;
        }
        let def = self.build_meta_def(&s);
        s.meta_def = Some(def.clone());
        if let Some(pos) = s.active_tools.iter().position(|t| tool_name_of(t) == "tools_load") {
            s.active_tools[pos] = def;
        }
    }

    fn baseline_tools(&self, s: &State) -> (Vec<Value>, Vec<String>) {
        let modules = self.modules();
        let mut tools = Vec::new();
        let mut names = Vec::new();
        if let Some(m) = &s.meta_def {
            tools.push(m.clone());
            names.push("tools_load".to_string());
        }
        for (name, entry) in Self::default_tools(s) {
            if names.contains(&name) || !self.loadable(&entry, &modules) {
                continue;
            }
            tools.push(entry.tool.clone());
            names.push(name);
        }
        (tools, names)
    }

    /// preload all tools of the preloaded modules
    pub fn load_default_tools(&self) {
        if !config::get_bool(&["model", "dynamic_tool_loading"], true) {
            return;
        }
        let modules = self.modules();
        let mut s = self.state.lock();
        if s.meta_def.is_none() {
            return;
        }
        for (name, entry) in Self::default_tools(&s) {
            if s.active_names.contains(&name) || !self.loadable(&entry, &modules) {
                continue;
            }
            s.active_tools.push(entry.tool.clone());
            s.active_names.push(name);
        }
    }

    /// reset active tools to the baseline (meta tool + default tools)
    pub fn reset_for_new_chat(&self) {
        if config::get_bool(&["model", "dynamic_tool_loading"], true) {
            let mut s = self.state.lock();
            let (t, n) = self.baseline_tools(&s);
            s.active_tools = t;
            s.active_names = n;
        } else {
            {
                let mut s = self.state.lock();
                s.active_tools.clear();
                s.active_names.clear();
            }
            self.load_all_tools();
        }
    }

    /// load every loadable tool from the catalog into the active set
    pub fn load_all_tools(&self) {
        let modules = self.modules();
        let mut s = self.state.lock();
        let entries: Vec<(String, CatalogEntry)> = s.catalog.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (name, entry) in entries {
            if !self.loadable(&entry, &modules) || s.active_names.contains(&name) {
                continue;
            }
            s.active_tools.push(entry.tool.clone());
            s.active_names.push(name);
        }
    }

    /// modules whose tools are active but are not part of the baseline
    pub fn get_active_non_baseline_modules(&self) -> Vec<String> {
        let baseline = Self::preloaded_modules();
        let s = self.state.lock();
        let mut out = BTreeSet::new();
        for name in &s.active_names {
            if let Some(e) = s.catalog.get(name) {
                if !baseline.contains(&e.module) {
                    out.insert(e.module.clone());
                }
            }
        }
        out.into_iter().collect()
    }

    pub fn persist_active_tools(&self) {
        let Some(ch) = self.ch() else { return };
        if ch.chat.current().is_none() {
            return;
        }
        ch.chat.set_loaded_modules(self.get_active_non_baseline_modules());
    }

    /// load the modules persisted in the chat's metadata
    pub fn restore_chat_tools(&self) {
        let Some(ch) = self.ch() else { return };
        if ch.chat.current().is_none() {
            return;
        }
        for m in ch.chat.get_loaded_modules() {
            self.activate_module_tools(&m);
        }
    }

    /// load all cataloged tools of an enabled module
    pub fn activate_module_tools(&self, module_name: &str) -> Value {
        let module_name = module_name.to_lowercase().trim().to_string();
        let modules = self.modules();
        let Some(module) = modules.get(&module_name) else {
            let s = self.state.lock();
            return json!({"status": "error", "unknown_module": module_name, "enabled_modules": self.enabled_module_names(&s)});
        };
        let disabled_tools = module.disabled_tools();
        let mut s = self.state.lock();
        let mut loaded = Vec::new();
        let mut already = Vec::new();
        let mut disabled = Vec::new();
        let entries: Vec<(String, CatalogEntry)> = s.catalog.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (name, entry) in entries {
            if entry.module != module_name {
                continue;
            }
            if disabled_tools.contains(&entry.method) {
                disabled.push(name);
            } else if s.active_names.contains(&name) {
                already.push(name);
            } else {
                s.active_tools.push(entry.tool.clone());
                s.active_names.push(name.clone());
                loaded.push(name);
            }
        }
        let mut r = json!({"loaded": loaded, "already_loaded": already});
        if !disabled.is_empty() {
            r["disabled"] = json!(disabled);
        }
        r
    }

    /// the tools_load meta tool
    pub fn tools_load(&self, module_names: &[String]) -> Value {
        let mut loaded: Vec<Value> = Vec::new();
        let mut already: Vec<Value> = Vec::new();
        let mut disabled: Vec<Value> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        let mut enabled_modules: Option<Value> = None;
        for m in module_names {
            let r = self.activate_module_tools(m);
            if r.get("status").and_then(|s| s.as_str()) == Some("error") {
                unknown.push(m.to_lowercase().trim().to_string());
                enabled_modules = r.get("enabled_modules").cloned();
                continue;
            }
            for (key, dst) in [("loaded", &mut loaded), ("already_loaded", &mut already), ("disabled", &mut disabled)] {
                if let Some(Value::Array(a)) = r.get(key) {
                    dst.extend(a.iter().cloned());
                }
            }
        }
        let has_success = !loaded.is_empty() || !already.is_empty();
        if !unknown.is_empty() && !has_success {
            let enabled = enabled_modules
                .as_ref()
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(crate::pyjson::py_str).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            let also = if unknown.len() > 1 { format!(" (also unknown: {})", unknown[1..].join(", ")) } else { String::new() };
            return json!({"status": "error", "content": format!("No enabled module named '{}'{}. Enabled modules: {}.", unknown[0], also, enabled)});
        }
        self.persist_active_tools();
        if !unknown.is_empty() {
            return json!({"status": "success", "not_found": unknown, "enabled_modules": enabled_modules.unwrap_or(Value::Null)});
        }
        json!({"status": if has_success { "success" } else { "error" }})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::{ParamType, ToolParam};

    #[test]
    fn docstring_parsing() {
        let doc = "Adds an event to the calendar.\n\n        Args:\n            title (str): The title\n              continued here\n            year: The year\n\n        Returns:\n            something\n        ";
        let (params, clean) = parse_tool_docstring(doc);
        assert_eq!(clean, "Adds an event to the calendar.");
        assert_eq!(params.get("title").unwrap(), "The title continued here");
        assert_eq!(params.get("year").unwrap(), "The year");
    }

    #[test]
    fn tool_dict() {
        let spec = ToolSpec::new("create", "Creates a memory.\n\nArgs:\n    content: what to remember", vec![ToolParam::req("content", ParamType::String), ToolParam::opt("pinned", ParamType::Boolean)]);
        let t = tool_dict_from_spec(&spec, "memory_create");
        assert_eq!(
            crate::pyjson::dumps(&t),
            r#"{"type": "function", "function": {"name": "memory_create", "parameters": {"type": "object", "properties": {"content": {"type": "string", "description": "what to remember"}, "pinned": {"type": "boolean"}}, "required": ["content"], "additionalProperties": false}, "strict": false, "description": "Creates a memory."}}"#
        );
    }
}
