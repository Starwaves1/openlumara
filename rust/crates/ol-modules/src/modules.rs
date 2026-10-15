//! Port of modules/modules/module.py

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use ol_core::pyjson::{py_repr, py_str};
use parking_lot::Mutex;
use serde_json::{json, Value};

#[path = "../../../generated/modules/modules.rs"]
pub(crate) mod meta;

pub struct Modules {
    base: ModuleBase,
    disabled: Mutex<Vec<String>>,
}

impl Modules {
    pub fn new(base: ModuleBase) -> Self {
        Self { base, disabled: Mutex::new(Vec::new()) }
    }
}

fn joined(keys: &[&str]) -> String {
    match ol_core::config::get(keys) {
        Some(Value::Array(a)) => a.iter().map(py_str).collect::<Vec<_>>().join(", "),
        _ => String::new(),
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
        if ol_core::config::get_bool(&["model", "dynamic_tool_loading"], false) {
            // if dynamic tool loading is on, the list of enabled modules gets sent in the tool prompt
            // so don't send a duplicate in the system prompt
            return Ok(None);
        }
        let module_list = json!({
            "enabled": joined(&["modules", "enabled"]),
            "disabled": joined(&["modules", "disabled"]),
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
                let name = args.str("name").unwrap_or_default();
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
