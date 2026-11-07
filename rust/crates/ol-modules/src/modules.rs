//! Port of modules/modules/module.py

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use ol_core::pyjson::py_repr;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

#[path = "../../../generated/modules/modules.rs"]
pub(crate) mod meta;

pub struct Modules {
    base: ModuleBase,
    disabled: Mutex<Vec<String>>,
}

impl Modules {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base, disabled: Mutex::new(Vec::new()) }
    }
}

/// port of python `", ".join(core.config.get("modules", key, default=[]))`: a missing key is an empty
/// list, a string is joined character by character, a dict joins its keys, and any other value (or a
/// non-string list item) raises the same TypeError python does
fn joined(key: &str) -> anyhow::Result<String> {
    let value = ol_core::config::get(&["modules", key]).unwrap_or_else(|| json!([]));
    let items: Vec<String> = match &value {
        Value::Array(a) => a
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                Value::String(s) => Ok(s.clone()),
                other => anyhow::bail!("sequence item {i}: expected str instance, {} found", py_type_name(other)),
            })
            .collect::<anyhow::Result<_>>()?,
        Value::String(s) => s.chars().map(|c| c.to_string()).collect(),
        Value::Object(m) => m.keys().cloned().collect(),
        _ => anyhow::bail!("can only join an iterable"),
    };
    Ok(items.join(", "))
}

/// the python type name of a json-derived value, as it appears in TypeError messages
fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

#[async_trait]
impl Module for Modules {
    fn base(&self) -> &ModuleBase {
        &self.base
    }
    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }
    fn disabled_tools(&self) -> Vec<String> {
        self.disabled.lock().clone()
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        if !self.base.config.get_bool("allow_ai_to_toggle") {
            self.disabled.lock().push("toggle".into());
        }
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        // python's core.config.get() reloads config.yaml from disk on every call, so edits made
        // outside the process are seen here
        ol_core::config::reload();
        if ol_core::config::get_bool(&["model", "dynamic_tool_loading"], false) {
            // if dynamic tool loading is on, the list of enabled modules gets sent in the tool prompt
            // so don't send a duplicate in the system prompt
            return Ok(None);
        }
        let module_list = json!({
            "enabled": joined("enabled")?,
            "disabled": joined("disabled")?,
        });
        Ok(Some(py_repr(&module_list)))
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "toggle" => {
                if !self.base.config.get_bool("allow_ai_to_toggle") {
                    return Ok(self.base.result(
                        json!("Module toggling is disabled for security. Ask user to manually toggle the module via the web ui or `/config` or by editing the config file."),
                        true,
                    ));
                }
                // python calls name.lower() on the raw argument before any branch, so a non-string
                // name (including JSON null) raises AttributeError instead of being coerced
                let name = match args.0.get("name") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => anyhow::bail!("'{}' object has no attribute 'lower'", py_type_name(other)),
                    None => args.str("name").unwrap_or_default(),
                };
                if name == "modules" {
                    return Ok(json!("module manager can only be manually turned off by using the settings dialog, the `/module` command, or editing the config file. user needs to know what you're doing!"));
                }
                let manager = self.base.manager().ok_or_else(|| anyhow::anyhow!("'NoneType' object has no attribute 'toggle_module'"))?;
                if manager.toggle_module(&name) {
                    Ok(self.base.result(json!("Module has been toggled. Remind user to use `/restart` to apply changes"), true))
                } else {
                    Ok(self.base.result(json!("That module does not exist!"), false))
                }
            }
            "get_help" => {
                let module_name = args.str("module_name").unwrap_or_default();
                let structure = ol_core::config::get_module_structure();
                match structure.get(&module_name) {
                    None => Ok(self.base.result(json!("That module does not exist"), false)),
                    Some(v) => Ok(v.clone()),
                }
            }
            _ => anyhow::bail!("'Modules' object has no attribute '{}'", method),
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

    use crate::util::CONFIG_TEST_LOCK as CONFIG_LOCK;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    /// installs a config with the `allow_ai_to_toggle` flag, the enabled/disabled module lists
    /// and the `model.dynamic_tool_loading` flag
    fn setup(allow_toggle: bool, dynamic_tool_loading: bool) -> Modules {
        install_config(json!({
            "modules": {
                "enabled": ["identity", "notes"],
                "disabled": ["tutorial"],
                "settings": {"modules": {"allow_ai_to_toggle": allow_toggle}}
            },
            "model": {"dynamic_tool_loading": dynamic_tool_loading}
        }));
        Modules::new(ModuleBase::detached("modules"))
    }

    /// installs a config whose `modules.enabled` is the given value, to exercise how the list is read
    fn setup_enabled(enabled: Value) -> Modules {
        install_config(json!({
            "modules": {
                "enabled": enabled,
                "disabled": [],
                "settings": {"modules": {"allow_ai_to_toggle": false}}
            },
            "model": {"dynamic_tool_loading": false}
        }));
        Modules::new(ModuleBase::detached("modules"))
    }

    fn install_config(data: Value) {
        ol_core::config::set_for_tests(data.clone());
        // set_for_tests() keeps a config file from an earlier test, and every read reloads that file,
        // so the same data is written to it (JSON is valid YAML)
        let path = ol_core::config::config_path().expect("test config path");
        std::fs::write(&path, data.to_string()).expect("write test config");
    }

    fn call(m: &Modules, method: &str, pairs: &[(&str, Value)]) -> Value {
        let args: Map<String, Value> = pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        block_on(m.call_tool(method, ToolArgs::new(args))).unwrap()
    }

    #[test]
    fn toggle_refused_when_ai_toggling_is_off() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup(false, false);
        // on_ready puts the toggle tool into disabled_tools when the setting is off
        block_on(m.on_ready()).unwrap();
        assert_eq!(m.disabled_tools(), vec!["toggle".to_string()]);
        let out = call(&m, "toggle", &[("name", json!("notes"))]);
        assert_eq!(
            out,
            json!({
                "status": "success",
                "content": "Module toggling is disabled for security. Ask user to manually toggle the module via the web ui or `/config` or by editing the config file."
            })
        );
    }

    #[test]
    fn toggle_of_module_manager_is_refused_with_plain_string() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup(true, false);
        block_on(m.on_ready()).unwrap();
        assert!(m.disabled_tools().is_empty());
        let out = call(&m, "toggle", &[("name", json!("modules"))]);
        assert_eq!(
            out,
            json!("module manager can only be manually turned off by using the settings dialog, the `/module` command, or editing the config file. user needs to know what you're doing!")
        );
    }

    fn call_result(m: &Modules, method: &str, pairs: &[(&str, Value)]) -> anyhow::Result<Value> {
        let args: Map<String, Value> = pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        block_on(m.call_tool(method, ToolArgs::new(args)))
    }

    #[test]
    fn toggle_with_non_string_name_raises_like_python_attribute_error() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup(true, false);
        // python: name.lower() on an int raises before any branch runs
        let err = call_result(&m, "toggle", &[("name", json!(7))]).unwrap_err();
        assert_eq!(err.to_string(), "'int' object has no attribute 'lower'");
        // a JSON null is None in python, which has no lower() either
        let err = call_result(&m, "toggle", &[("name", Value::Null)]).unwrap_err();
        assert_eq!(err.to_string(), "'NoneType' object has no attribute 'lower'");
    }

    #[test]
    fn get_help_unknown_module_is_an_error_result() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup(true, false);
        let out = call(&m, "get_help", &[("module_name", json!("no_such_module_xyz"))]);
        assert_eq!(out, json!({"status": "error", "content": "That module does not exist"}));
    }

    #[test]
    fn system_prompt_lists_enabled_and_disabled_like_python_str_dict() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup(false, false);
        let prompt = block_on(m.on_system_prompt()).unwrap();
        assert_eq!(prompt.as_deref(), Some("{'enabled': 'identity, notes', 'disabled': 'tutorial'}"));
    }

    #[test]
    fn system_prompt_reflects_config_edited_on_disk_while_running() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup(false, false);
        // an edit made outside the process, like a user editing config.yaml by hand
        let path = ol_core::config::config_path().expect("test config path");
        let edited = json!({
            "modules": {
                "enabled": ["identity"],
                "disabled": ["tutorial", "notes"],
                "settings": {"modules": {"allow_ai_to_toggle": false}}
            },
            "model": {"dynamic_tool_loading": false}
        });
        std::fs::write(&path, edited.to_string()).expect("write edited config");
        let prompt = block_on(m.on_system_prompt()).unwrap();
        assert_eq!(prompt.as_deref(), Some("{'enabled': 'identity', 'disabled': 'tutorial, notes'}"));
    }

    #[test]
    fn system_prompt_joins_a_plain_string_character_by_character_like_python() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // python's ", ".join("identity") iterates the string
        let m = setup_enabled(json!("identity"));
        let prompt = block_on(m.on_system_prompt()).unwrap();
        assert_eq!(prompt.as_deref(), Some("{'enabled': 'i, d, e, n, t, i, t, y', 'disabled': ''}"));
    }

    #[test]
    fn system_prompt_errors_on_null_list_like_python_type_error() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup_enabled(Value::Null);
        let err = block_on(m.on_system_prompt()).unwrap_err();
        assert_eq!(err.to_string(), "can only join an iterable");
    }

    #[test]
    fn system_prompt_errors_on_non_string_list_item_like_python_type_error() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup_enabled(json!(["identity", 7]));
        let err = block_on(m.on_system_prompt()).unwrap_err();
        assert_eq!(err.to_string(), "sequence item 1: expected str instance, int found");
    }

    #[test]
    fn system_prompt_omitted_when_dynamic_tool_loading_is_on() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup(false, true);
        assert_eq!(block_on(m.on_system_prompt()).unwrap(), None);
    }
}
