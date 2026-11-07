//! Port of modules/models/module.py

use crate::util;
use async_trait::async_trait;
use ol_core::manager::Manager;
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

#[path = "../../../generated/modules/models.rs"]
pub mod meta;

pub struct Models {
    base: ModuleBase,
    disabled: Mutex<Vec<String>>,
    /// python: self.models (None until loaded; an empty list counts as not loaded)
    models: Mutex<Option<Vec<String>>>,
    /// python sets `self.header = "current model"` from on_system_prompt
    header: Mutex<Option<String>>,
}

impl Models {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

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

    /// python switch(name): `name` is whatever the caller passed (a JSON value for the tool, a str for the command)
    async fn switch(&self, name: &Value) -> anyhow::Result<Value> {
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
        // python calls name.strip() inside the loop, so a non-str name fails once there is a model to compare against
        let wanted = match name {
            Value::String(s) => util::py_strip(s).to_lowercase(),
            other => anyhow::bail!("'{}' object has no attribute 'strip'", util::py_type_name(other)),
        };
        let mut found_id = None;
        for model_id in &models {
            if util::py_strip(model_id).to_lowercase() == wanted {
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
        // python: self.models = None; self.disabled_tools starts empty for a fresh instance
        *self.models.lock() = None;
        self.disabled.lock().clear();
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
            "switch" => {
                // python passes the raw argument through; a JSON null arrives as None
                let name = args.raw("name").cloned().unwrap_or(Value::Null);
                self.switch(&name).await
            }
            _ => anyhow::bail!("'Models' object has no attribute '{}'", method),
        }
    }

    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        match name {
            "model" => {
                if args.is_empty() {
                    return Ok(Some(format!("Current model: {}", py_opt(&self.current_model()?))));
                }
                let r = self.switch(&json!(util::py_strip(&args.join(" ")))).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use ol_core::manager::{Args, Manager};
    use serde_json::Map;

    /// the config is process-wide, so the tests that install one run one at a time
    static CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    /// installs a config with the current model and the two prompt flags, then builds the module
    /// with a live manager. `preloaded` fills the python `self.models` cache so no HTTP call is made.
    fn setup(current: &str, insert_available: bool, preloaded: Option<Vec<&str>>) -> (Arc<Manager>, Models) {
        let data = json!({
            "model": {"name": current},
            "modules": {"settings": {"models": {
                "insert_current_model_into_system_prompt": true,
                "insert_available_models_into_system_prompt": insert_available,
            }}},
        });
        ol_core::config::set_for_tests(data.clone());
        // set_for_tests() keeps a config file from an earlier test and every read reloads it,
        // so the same data is written to it (JSON is valid YAML)
        let path = ol_core::config::config_path().expect("test config path");
        std::fs::write(&path, data.to_string()).expect("write test config");
        let mgr = Manager::new(Args::default());
        let models = Models::new(ModuleBase::new("models", false, Arc::downgrade(&mgr)));
        *models.models.lock() = preloaded.map(|v| v.into_iter().map(String::from).collect());
        (mgr, models)
    }

    fn stored_model() -> Option<String> {
        ol_core::config::get_str(&["model", "name"])
    }

    #[test]
    fn switch_matches_case_insensitively_and_stores_the_canonical_id() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mgr, m) = setup("llama-3", false, Some(vec!["Qwen3.5-9B", "llama-3"]));
        let args = ToolArgs::new(json!({"name": "  qwen3.5-9b "}).as_object().unwrap().clone());
        let out = block_on(m.call_tool("switch", args)).unwrap();
        assert_eq!(out, json!("model has been switched to Qwen3.5-9B"));
        assert_eq!(mgr.api().get_model().as_deref(), Some("Qwen3.5-9B"));
        assert_eq!(stored_model().as_deref(), Some("Qwen3.5-9B"));
    }

    #[test]
    fn switch_unknown_model_returns_exact_error_and_keeps_current_model() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mgr, m) = setup("llama-3", false, Some(vec!["Qwen3.5-9B", "llama-3"]));
        let out = block_on(m.run_command("model", vec!["gpt-9".into()])).unwrap();
        assert_eq!(out.as_deref(), Some("model does not exist. use models_get_available() first"));
        assert_eq!(mgr.api().get_model().as_deref(), Some("llama-3"));
    }

    #[test]
    fn switch_with_non_string_name_fails_like_python_strip() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mgr, m) = setup("llama-3", false, Some(vec!["Qwen3.5-9B", "llama-3"]));
        let args = ToolArgs::new(json!({"name": 7}).as_object().unwrap().clone());
        let err = block_on(m.call_tool("switch", args)).unwrap_err();
        assert_eq!(err.to_string(), "'int' object has no attribute 'strip'");
        assert_eq!(mgr.api().get_model().as_deref(), Some("llama-3"));
    }

    #[test]
    fn switch_strips_python_isspace_control_chars_from_the_name() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mgr, m) = setup("llama-3", false, Some(vec!["Qwen3.5-9B", "llama-3"]));
        // python str.strip() removes U+001C..U+001F, which Rust str::trim() keeps
        let out = block_on(m.run_command("model", vec!["\u{1c}Qwen3.5-9B\u{1f}".into()])).unwrap();
        assert_eq!(out.as_deref(), Some("model has been switched to Qwen3.5-9B"));
        assert_eq!(mgr.api().get_model().as_deref(), Some("Qwen3.5-9B"));
    }

    #[test]
    fn models_command_lists_ids_with_switch_hint() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_mgr, m) = setup("llama-3", false, Some(vec!["Qwen3.5-9B", "llama-3"]));
        let out = block_on(m.run_command("models", vec![])).unwrap();
        assert_eq!(out.as_deref(), Some("Qwen3.5-9B\nllama-3\n\nUse `/model <name>` to switch to your model of choice"));
    }

    #[test]
    fn model_command_without_args_shows_current_model() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_mgr, m) = setup("llama-3", false, None);
        let out = block_on(m.run_command("model", vec![])).unwrap();
        assert_eq!(out.as_deref(), Some("Current model: llama-3"));
    }

    #[test]
    fn get_available_returns_the_model_list_as_a_success_result() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_mgr, m) = setup("llama-3", false, Some(vec!["Qwen3.5-9B", "llama-3"]));
        let out = block_on(m.call_tool("get_available", ToolArgs::new(Map::new()))).unwrap();
        assert_eq!(out, json!({"status": "success", "content": ["Qwen3.5-9B", "llama-3"]}));
    }

    #[test]
    fn on_ready_with_available_models_in_prompt_disables_get_available() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_mgr, m) = setup("llama-3", true, None);
        block_on(m.on_ready()).unwrap();
        assert_eq!(m.disabled_tools(), vec!["get_available".to_string()]);

        let (_mgr2, plain) = setup("llama-3", false, None);
        block_on(plain.on_ready()).unwrap();
        assert!(plain.disabled_tools().is_empty());
    }

    #[test]
    fn system_prompt_shows_current_model_and_sets_header_when_models_not_inserted() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_mgr, m) = setup("llama-3", false, None);
        let out = block_on(m.on_system_prompt()).unwrap();
        assert_eq!(out.as_deref(), Some("llama-3"));
        assert_eq!(m.header().as_deref(), Some("current model"));
    }

    #[test]
    fn system_prompt_lists_switchable_models_when_inserted() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_mgr, m) = setup("llama-3", true, Some(vec!["Qwen3.5-9B", "llama-3"]));
        let out = block_on(m.on_system_prompt()).unwrap();
        assert_eq!(
            out.as_deref(),
            Some("Current model: llama-3\n\nModels you can switch to using the models_switch() toolcall: Qwen3.5-9B, llama-3")
        );
        assert_eq!(m.header(), None);
    }
}
