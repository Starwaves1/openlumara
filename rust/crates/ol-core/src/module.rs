//! Port of core/module.py: the module base class, its config accessor, the command
//! decorator registry, and the tool-schema conventions.
//!
//! Python modules expose tools as public methods; the tool loader inspects their
//! signatures and Google-style docstrings. In Rust each module declares its tools as
//! [`ToolSpec`]s (method name, the *verbatim* Python docstring, typed parameters) and
//! the tool loader builds byte-identical tool schemas from them with the same
//! docstring parser. Calls are dispatched through [`Module::call_tool`].

use crate::config::ConfigView;
use crate::manager::Manager;
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::sync::{Arc, Weak};

/// JSON-schema type of a tool parameter, derived like Python's `_TYPE_MAP`:
/// str->string, int->integer, bool->boolean, list->array, dict->object,
/// anything else (float, unannotated, Optional[...]) -> string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    String,
    Integer,
    Boolean,
    Array,
    Object,
}

impl ParamType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ParamType::String => "string",
            ParamType::Integer => "integer",
            ParamType::Boolean => "boolean",
            ParamType::Array => "array",
            ParamType::Object => "object",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolParam {
    pub name: &'static str,
    pub ty: ParamType,
    /// true when the python parameter has no default value
    pub required: bool,
}

impl ToolParam {
    pub const fn req(name: &'static str, ty: ParamType) -> Self {
        Self { name, ty, required: true }
    }
    pub const fn opt(name: &'static str, ty: ParamType) -> Self {
        Self { name, ty, required: false }
    }
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    /// python method name (tool name is `<module>_<method>`)
    pub method: &'static str,
    /// the python docstring, verbatim (it is parsed for description + arg docs)
    pub doc: &'static str,
    pub params: Vec<ToolParam>,
}

impl ToolSpec {
    pub fn new(method: &'static str, doc: &'static str, params: Vec<ToolParam>) -> Self {
        Self { method, doc, params }
    }
}

/// command help: plain text, or per-subcommand help (`@command(name, help={...})`)
#[derive(Clone, Debug)]
pub enum CommandHelp {
    Text(String),
    Sub(Vec<(String, String)>),
}

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub name: String,
    pub help: CommandHelp,
    /// `send_to_ai=False` in python: the command and its output are ghost messages
    pub temporary: bool,
}

impl CommandSpec {
    pub fn new(name: &str, help: &str) -> Self {
        Self { name: name.to_lowercase().trim().to_string(), help: CommandHelp::Text(help.into()), temporary: false }
    }
    pub fn with_sub(name: &str, sub: &[(&str, &str)]) -> Self {
        Self {
            name: name.to_lowercase().trim().to_string(),
            help: CommandHelp::Sub(sub.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()),
            temporary: false,
        }
    }
    pub fn temporary(mut self) -> Self {
        self.temporary = true;
        self
    }
}

/// a `@webui.route(path, method)` handler declared by a module
#[derive(Clone, Debug)]
pub struct UiRoute {
    pub path: String,
    pub method: String,
}

impl UiRoute {
    pub fn get(path: &str) -> Self {
        Self { path: path.trim_matches('/').to_lowercase(), method: "GET".into() }
    }
    pub fn post(path: &str) -> Self {
        Self { path: path.trim_matches('/').to_lowercase(), method: "POST".into() }
    }
}

/// result of an `on_user_message` hook
#[derive(Clone, Debug)]
pub enum UserMessageHook {
    /// python: returned None
    Unchanged,
    /// python: returned a new string
    Replace(String),
    /// python: returned False -> stop processing the message
    Intercept,
}

/// unified way of returning tool results (Module.result)
pub fn tool_result(data: impl Into<Value>, success: bool) -> Value {
    let mut m = Map::new();
    m.insert("status".into(), Value::String(if success { "success" } else { "error" }.into()));
    m.insert("content".into(), data.into());
    Value::Object(m)
}

/// module config accessor (core.module.ModuleConfig)
#[derive(Clone)]
pub struct ModuleConfig {
    view: ConfigView,
}

impl ModuleConfig {
    pub fn new(view: ConfigView) -> Self {
        Self { view }
    }
    /// get a (nested) value, reloading the config from disk first
    pub fn get(&self, keys: &[&str]) -> Option<Value> {
        let current = Value::Object(self.view.to_dict());
        crate::pyjson::get_in(&current, keys).cloned()
    }
    pub fn get1(&self, key: &str) -> Option<Value> {
        self.get(&[key])
    }
    pub fn get_str(&self, key: &str) -> Option<String> {
        self.get1(key).and_then(|v| v.as_str().map(|s| s.to_string()))
    }
    pub fn get_bool(&self, key: &str) -> bool {
        self.get1(key).map(|v| crate::pyjson::truthy(&v)).unwrap_or(false)
    }
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get1(key).and_then(|v| crate::pyjson::as_i64(&v))
    }
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.get1(key).and_then(|v| crate::pyjson::as_f64(&v))
    }
    /// set a key (only if it already exists, like the python version)
    pub fn set(&self, key: &str, value: Value) -> bool {
        if !self.view.contains(key) {
            return false;
        }
        self.view.set(key, value);
        true
    }
    pub fn to_dict(&self) -> Map<String, Value> {
        self.view.to_dict()
    }
    pub fn view(&self) -> &ConfigView {
        &self.view
    }
}

/// state every module instance carries (what python's Module.__init__ sets up)
pub struct ModuleBase {
    pub name: String,
    pub is_user_module: bool,
    pub config: ModuleConfig,
    manager: Weak<Manager>,
}

impl ModuleBase {
    pub fn new(name: &str, is_user_module: bool, manager: Weak<Manager>) -> Self {
        let section = if is_user_module { "user_modules" } else { "modules" };
        let view = ConfigView::new(&[section, "settings", name]);
        Self { name: name.to_string(), is_user_module, config: ModuleConfig::new(view), manager }
    }

    /// a detached base for tests
    pub fn detached(name: &str) -> Self {
        Self::new(name, false, Weak::new())
    }

    pub fn manager(&self) -> Option<Arc<Manager>> {
        self.manager.upgrade()
    }

    /// the currently active channel (python: self.channel)
    pub fn channel(&self) -> Option<Arc<crate::channel::Channel>> {
        self.manager()?.current_channel()
    }

    /// alias for manager.log()
    pub fn log(&self, category: &str, message: &str) {
        match self.manager() {
            Some(m) => m.log(category, message),
            None => crate::log(category, message),
        }
    }

    pub fn result(&self, data: impl Into<Value>, success: bool) -> Value {
        tool_result(data, success)
    }
}

/// tool call arguments with python-ish lenient accessors
#[derive(Clone, Debug, Default)]
pub struct ToolArgs(pub Map<String, Value>);

impl ToolArgs {
    pub fn new(m: Map<String, Value>) -> Self {
        Self(m)
    }
    pub fn raw(&self, name: &str) -> Option<&Value> {
        self.0.get(name).filter(|v| !v.is_null())
    }
    pub fn has(&self, name: &str) -> bool {
        self.raw(name).is_some()
    }
    /// string argument (non-strings are converted with python str())
    pub fn str(&self, name: &str) -> Option<String> {
        self.raw(name).map(crate::pyjson::py_str)
    }
    pub fn str_or(&self, name: &str, default: &str) -> String {
        self.str(name).unwrap_or_else(|| default.to_string())
    }
    pub fn i64(&self, name: &str) -> Option<i64> {
        self.raw(name).and_then(crate::pyjson::as_i64)
    }
    pub fn f64(&self, name: &str) -> Option<f64> {
        self.raw(name).and_then(crate::pyjson::as_f64)
    }
    pub fn bool(&self, name: &str) -> Option<bool> {
        self.raw(name).map(|v| match v {
            Value::String(s) => !matches!(s.to_lowercase().as_str(), "false" | "0" | "" | "no" | "off"),
            other => crate::pyjson::truthy(other),
        })
    }
    pub fn bool_or(&self, name: &str, default: bool) -> bool {
        self.bool(name).unwrap_or(default)
    }
    /// list of strings (a bare string becomes a one-item list)
    pub fn str_list(&self, name: &str) -> Option<Vec<String>> {
        self.raw(name).map(|v| match v {
            Value::Array(a) => a.iter().map(crate::pyjson::py_str).collect(),
            Value::String(s) => vec![s.clone()],
            other => vec![crate::pyjson::py_str(other)],
        })
    }
    pub fn list(&self, name: &str) -> Option<Vec<Value>> {
        self.raw(name).map(|v| match v {
            Value::Array(a) => a.clone(),
            other => vec![other.clone()],
        })
    }
    pub fn obj(&self, name: &str) -> Option<Map<String, Value>> {
        self.raw(name).and_then(|v| v.as_object().cloned())
    }
}

/// python TypeError message for a missing argument / unexpected keyword argument
pub fn validate_tool_args(class_name: &str, spec: &ToolSpec, args: &Map<String, Value>) -> Result<(), String> {
    for key in args.keys() {
        if !spec.params.iter().any(|p| p.name == key) {
            return Err(format!("{}.{}() got an unexpected keyword argument '{}'", class_name, spec.method, key));
        }
    }
    let missing: Vec<&str> = spec.params.iter().filter(|p| p.required && !args.contains_key(p.name)).map(|p| p.name).collect();
    if !missing.is_empty() {
        let quoted: Vec<String> = missing.iter().map(|m| format!("'{}'", m)).collect();
        let list = match quoted.len() {
            1 => quoted[0].clone(),
            2 => format!("{} and {}", quoted[0], quoted[1]),
            _ => format!("{}, and {}", quoted[..quoted.len() - 1].join(", "), quoted[quoted.len() - 1]),
        };
        return Err(format!(
            "{}.{}() missing {} required positional argument{}: {}",
            class_name,
            spec.method,
            missing.len(),
            if missing.len() == 1 { "" } else { "s" },
            list
        ));
    }
    Ok(())
}

/// Base trait for modules/plugins (core.module.Module)
#[async_trait]
pub trait Module: Send + Sync + 'static {
    fn base(&self) -> &ModuleBase;

    fn name(&self) -> &str {
        &self.base().name
    }

    /// tool methods exposed to the AI, in python class-definition order
    fn tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// method names whose tools are disabled (python: self.disabled_tools)
    fn disabled_tools(&self) -> Vec<String> {
        Vec::new()
    }

    /// execute a tool method. arguments were already validated against the ToolSpec.
    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        let _ = args;
        anyhow::bail!("'{}' object has no attribute '{}'", self.name(), method)
    }

    /// overrides the system prompt section header (python: module.header)
    fn header(&self) -> Option<String> {
        None
    }

    /// inserted into the system prompt if something is returned
    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    /// inserted at the end of the context (after history) if something is returned
    async fn on_end_prompt(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    /// injected into the user's message (persisted in history metadata)
    async fn on_message_inject(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    /// runs once the module is ready to be used (and again after a reload)
    async fn on_ready(&self) -> anyhow::Result<()> {
        Ok(())
    }
    /// runs on shutdown and before a reload
    async fn on_shutdown(&self) -> anyhow::Result<()> {
        Ok(())
    }
    /// whether on_background() should be started as a background task
    fn has_background(&self) -> bool {
        false
    }
    /// long-running background task (schedulers, cronjobs, ...)
    async fn on_background(&self) -> anyhow::Result<()> {
        Ok(())
    }
    /// triggers when the user sends a message
    async fn on_user_message(&self, content: &str) -> anyhow::Result<UserMessageHook> {
        let _ = content;
        Ok(UserMessageHook::Unchanged)
    }
    /// triggers when the assistant sends a message
    async fn on_assistant_message(&self, content: &str) -> anyhow::Result<()> {
        let _ = content;
        Ok(())
    }

    /// commands registered with @core.module.command
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    /// run one of this module's commands. Ok(None) mirrors a python handler returning None.
    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        let _ = args;
        anyhow::bail!("unknown command {}", name)
    }

    /// webui extension api routes (@webui.route)
    fn ui_routes(&self) -> Vec<UiRoute> {
        Vec::new()
    }
    /// handle a webui extension api call. returning a `tool_result()`-shaped dict is
    /// unwrapped into the api envelope by the webui.
    async fn ui_route(&self, path: &str, method: &str, body: Option<Value>, query: Map<String, Value>) -> anyhow::Result<Value> {
        let _ = (method, body, query);
        anyhow::bail!("route not found: {}", path)
    }

    fn as_any(&self) -> &dyn std::any::Any;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arg_validation_messages() {
        let spec = ToolSpec::new(
            "add_event",
            "",
            vec![ToolParam::req("title", ParamType::String), ToolParam::req("year", ParamType::Integer), ToolParam::opt("x", ParamType::String)],
        );
        let args = json!({"title": "a"}).as_object().unwrap().clone();
        assert_eq!(
            validate_tool_args("Calendar", &spec, &args).unwrap_err(),
            "Calendar.add_event() missing 1 required positional argument: 'year'"
        );
        let args = json!({}).as_object().unwrap().clone();
        assert_eq!(
            validate_tool_args("Calendar", &spec, &args).unwrap_err(),
            "Calendar.add_event() missing 2 required positional arguments: 'title' and 'year'"
        );
        let args = json!({"title": "a", "year": 1, "bogus": 1}).as_object().unwrap().clone();
        assert_eq!(
            validate_tool_args("Calendar", &spec, &args).unwrap_err(),
            "Calendar.add_event() got an unexpected keyword argument 'bogus'"
        );
    }
}
