//! Port of modules/models/module.py

use crate::util;
use async_trait::async_trait;
use ol_core::manager::Manager;
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

#[path = "../../../generated/modules/models.rs"]
pub(crate) mod meta;

pub struct Models {
    base: ModuleBase,
    disabled: Mutex<Vec<String>>,
    /// python: self.models (None until loaded; an empty list counts as not loaded)
    models: Mutex<Option<Vec<String>>>,
    /// python sets `self.header = "current model"` from on_system_prompt
    header: Mutex<Option<String>>,
}

impl Models {
    pub fn new(base: ModuleBase) -> Self {
        Self { base, disabled: Mutex::new(Vec::new()), models: Mutex::new(None), header: Mutex::new(meta::HEADER.map(|s| s.to_string())) }
    }

    fn manager(&self) -> anyhow::Result<Arc<Manager>> {
        self.base.manager().ok_or_else(|| anyhow::anyhow!("'NoneType' object has no attribute 'API'"))
    }

    fn current_model(&self) -> anyhow::Result<Option<String>> {
        Ok(self.manager()?.api().get_model())
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        self.manager()?.api().list_models().await.map_err(|e| anyhow::anyhow!("{}", e))
    }

    fn loaded(&self) -> Option<Vec<String>> {
        self.models.lock().clone().filter(|m| !m.is_empty())
    }

    /// python _load_models(): returns the model list (None if it couldn't be loaded)
    async fn load_models(&self) -> anyhow::Result<Option<Vec<String>>> {
        if let Some(m) = self.loaded() {
            return Ok(Some(m));
        }
        let models = self.list_models().await?;
        if models.is_empty() {
            return Ok(self.models.lock().clone());
        }
        *self.models.lock() = Some(models.clone());
        Ok(Some(models))
    }

    async fn switch(&self, name: &str) -> anyhow::Result<Value> {
        let models = match self.loaded() {
            Some(m) => m,
            None => {
                let models = self.list_models().await?;
                if models.is_empty() {
                    return Ok(Value::Null);
                }
                *self.models.lock() = Some(models.clone());
                models
            }
        };
        let mut found_id = None;
        for model_id in &models {
            if model_id.trim().to_lowercase() == name.trim().to_lowercase() {
                found_id = Some(model_id.clone());
            }
        }
        let Some(found_id) = found_id else {
            return Ok(json!("model does not exist. use models_get_available() first"));
        };
        // python: core.config.config["model"]["name"] = found_id; save(); API.set_model(found_id)
        self.manager()?.api().set_model(&found_id);
        Ok(json!(format!("model has been switched to {}", found_id)))
    }
}

fn py_opt(s: &Option<String>) -> String {
    s.clone().unwrap_or_else(|| "None".into())
}

#[async_trait]
impl Module for Models {
    fn base(&self) -> &ModuleBase {
        &self.base
    }
    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }
    fn disabled_tools(&self) -> Vec<String> {
        self.disabled.lock().clone()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        util::command_specs(meta::commands())
    }
    fn header(&self) -> Option<String> {
        self.header.lock().clone()
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        *self.models.lock() = None;
        if self.base.config.get_bool("insert_available_models_into_system_prompt") {
            self.disabled.lock().push("get_available".into());
        }
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        let mut output = String::new();
        if self.base.config.get_bool("insert_current_model_into_system_prompt") {
            output.push_str(&format!("Current model: {}", py_opt(&self.current_model()?)));
        }
        let current_model = self.current_model()?;
        if self.base.config.get_bool("insert_available_models_into_system_prompt") {
            let models = match self.loaded() {
                Some(m) => m,
                None => {
                    let models = self.list_models().await?;
                    if models.is_empty() {
                        return Ok(if output.is_empty() { None } else { Some(output) });
                    }
                    *self.models.lock() = Some(models.clone());
                    models
                }
            };
            if models.len() > 1 {
                output.push_str("\n\nModels you can switch to using the models_switch() toolcall: ");
                output.push_str(&models.join(", "));
            }
            Ok(Some(output))
        } else {
            *self.header.lock() = Some("current model".into());
            Ok(current_model)
        }
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "get_available" => {
                let Some(models) = self.load_models().await? else { anyhow::bail!("'NoneType' object is not iterable") };
                Ok(self.base.result(json!(models), true))
            }
            "switch" => self.switch(&args.str("name").unwrap_or_default()).await,
            _ => anyhow::bail!("'Models' object has no attribute '{}'", method),
        }
    }

    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        match name {
            "model" => {
                if args.is_empty() {
                    return Ok(Some(format!("Current model: {}", py_opt(&self.current_model()?))));
                }
                let r = self.switch(args.join(" ").trim()).await?;
                Ok(match r {
                    Value::Null => None,
                    other => Some(ol_core::pyjson::py_str(&other)),
                })
            }
            "models" => {
                let Some(models) = self.load_models().await? else { anyhow::bail!("can only join an iterable") };
                Ok(Some(format!("{}\n\nUse `/model <name>` to switch to your model of choice", models.join("\n"))))
            }
            _ => anyhow::bail!("unknown command {}", name),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
