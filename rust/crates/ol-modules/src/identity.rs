//! Port of modules/identity/module.py

use crate::util;
use async_trait::async_trait;
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::{py_str, truthy};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

#[path = "../../../generated/modules/identity.rs"]
pub(crate) mod meta;

const NOT_SET: &str = "You have not yet set up an identity.";

pub struct Identity {
    base: ModuleBase,
    disabled: Mutex<Vec<String>>,
}

/// `on_system_prompt`: `identity = self.config.get("identity")`, `if not identity: return None`, else the identity
fn system_prompt_text(identity: Option<Value>) -> Option<String> {
    match identity {
        Some(v) if truthy(&v) => Some(py_str(&v)),
        _ => None,
    }
}

/// `cmd_set` with no arguments: `identity if len(identity) > 0 else "You have not yet set up an identity."`
fn current_identity_text(identity: Option<Value>) -> anyhow::Result<String> {
    match identity {
        Some(Value::String(s)) => Ok(if s.is_empty() { NOT_SET.to_string() } else { s }),
        Some(Value::Array(a)) if a.is_empty() => Ok(NOT_SET.to_string()),
        Some(Value::Object(m)) if m.is_empty() => Ok(NOT_SET.to_string()),
        Some(v @ (Value::Array(_) | Value::Object(_))) => Ok(py_str(&v)),
        Some(Value::Null) | None => anyhow::bail!("object of type 'NoneType' has no len()"),
        Some(other) => anyhow::bail!("object of type '{}' has no len()", util::py_type_name(&other)),
    }
}

impl Identity {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base, disabled: Mutex::new(Vec::new()) }
    }

    fn identity(&self) -> Option<Value> {
        self.base.config.get1("identity")
    }
}

#[async_trait]
impl Module for Identity {
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

    async fn on_ready(&self) -> anyhow::Result<()> {
        if !self.base.config.get_bool("allow_ai_to_change_identity") {
            self.disabled.lock().push("set".into());
        }
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        // dont use identity if the characters module is enabled and a character is currently active
        let ch = self.base.channel().ok_or_else(util::no_channel)?;
        if util::chat_has_character(&ch)? {
            return Ok(None);
        }
        Ok(system_prompt_text(self.identity()))
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "set" => {
                let content = args.raw("content").cloned().unwrap_or(Value::Null);
                self.base.config.set("identity", content);
                Ok(self.base.result(json!(true), true))
            }
            "clear" => {
                self.base.config.set("identity", json!(""));
                Ok(self.base.result(json!(true), true))
            }
            _ => anyhow::bail!("'Identity' object has no attribute '{}'", method),
        }
    }

    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        if name != "identity" {
            anyhow::bail!("unknown command {}", name);
        }
        if args.is_empty() {
            return Ok(Some(current_identity_text(self.identity())?));
        }
        match args[0].as_str() {
            "set" => {
                let text = args[1..].join(" ");
                self.base.config.set("identity", json!(text));
                Ok(Some("identity set!".into()))
            }
            "clear" => {
                // python: `await self.config.set("identity", "")` - ModuleConfig.set() is not a
                // coroutine, so the identity is cleared and then awaiting its None result raises.
                self.base.config.set("identity", json!(""));
                anyhow::bail!("object NoneType can't be used in 'await' expression")
            }
            _ => Ok(Some("invalid argument".into())),
        }
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
    use serde_json::Map;

    /// the config is process-wide, so every test that installs one (in any module) runs under the same lock
    use crate::util::CONFIG_TEST_LOCK as CONFIG_LOCK;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    /// installs a config with the given identity text and the `allow_ai_to_change_identity` flag
    fn setup(identity: &str, allow: bool) -> Identity {
        let settings: Map<String, Value> = Map::from_iter([
            ("identity".to_string(), json!(identity)),
            ("allow_ai_to_change_identity".to_string(), json!(allow)),
        ]);
        let data = json!({"modules": {"settings": {"identity": settings}}});
        ol_core::config::set_for_tests(data.clone());
        // set_for_tests() keeps a config file from an earlier test, and every read reloads that file,
        // so the same data is written to it (JSON is valid YAML)
        let path = ol_core::config::config_path().expect("test config path");
        std::fs::write(&path, data.to_string()).expect("write test config");
        Identity::new(ModuleBase::detached("identity"))
    }

    fn stored_identity() -> Value {
        ol_core::config::get(&["modules", "settings", "identity", "identity"]).unwrap_or(Value::Null)
    }

    #[test]
    fn identity_command_set_show_and_empty_default() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup("", true);
        let shown = block_on(m.run_command("identity", vec![])).unwrap();
        assert_eq!(shown.as_deref(), Some("You have not yet set up an identity."));

        let set = block_on(m.run_command("identity", vec!["set".into(), "You".into(), "are".into(), "Ada".into()])).unwrap();
        assert_eq!(set.as_deref(), Some("identity set!"));
        assert_eq!(stored_identity(), json!("You are Ada"));
        let shown = block_on(m.run_command("identity", vec![])).unwrap();
        assert_eq!(shown.as_deref(), Some("You are Ada"));

        let bad = block_on(m.run_command("identity", vec!["nope".into()])).unwrap();
        assert_eq!(bad.as_deref(), Some("invalid argument"));
    }

    #[test]
    fn set_tool_returns_python_result_and_stores_content() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup("", true);
        let mut args = Map::new();
        args.insert("content".into(), json!("You are Bo"));
        let out = block_on(m.call_tool("set", ToolArgs::new(args))).unwrap();
        assert_eq!(out, json!({"status": "success", "content": true}));
        assert_eq!(stored_identity(), json!("You are Bo"));

        let out = block_on(m.call_tool("clear", ToolArgs::default())).unwrap();
        assert_eq!(out, json!({"status": "success", "content": true}));
        assert_eq!(stored_identity(), json!(""));
    }

    #[test]
    fn clear_command_clears_then_raises_like_python_await() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup("You are Cy", true);
        let err = block_on(m.run_command("identity", vec!["clear".into()])).unwrap_err();
        assert_eq!(err.to_string(), "object NoneType can't be used in 'await' expression");
        assert_eq!(stored_identity(), json!(""));
    }

    #[test]
    fn set_tool_disabled_when_ai_may_not_change_identity() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let m = setup("You are Di", false);
        assert!(m.disabled_tools().is_empty());
        block_on(m.on_ready()).unwrap();
        assert_eq!(m.disabled_tools(), vec!["set".to_string()]);
    }

    #[test]
    fn set_tool_description_matches_stripped_python_docstring() {
        // python tool_loader strips the docstring: "\n".join(clean_lines).strip()
        let set = meta::tools().into_iter().find(|t| t.method == "set").unwrap();
        let tool = ol_core::tool_loader::tool_dict_from_spec(&set, "identity_set");
        let desc = tool["function"]["description"].as_str().unwrap();
        assert_eq!(
            desc,
            "Defines who you are as an AI. When defining your identity, ALWAYS start with \"You are\". Give yourself a name. Make one up if user doesn't provide it. Don't use words like \"i\", \"i'm\" or \"i am\". Write in 2nd person when using this tool."
        );
        assert_eq!(tool["function"]["parameters"]["required"], json!(["content"]));
    }

    #[test]
    fn clear_tool_description_is_python_docstring() {
        let clear = meta::tools().into_iter().find(|t| t.method == "clear").unwrap();
        let tool = ol_core::tool_loader::tool_dict_from_spec(&clear, "identity_clear");
        assert_eq!(
            tool["function"]["description"].as_str(),
            Some("Wipes your identity as an AI so you may start from scratch. USE WITH CAUTION!")
        );
    }

    #[test]
    fn system_prompt_and_empty_text_rules() {
        assert_eq!(system_prompt_text(Some(json!("You are Ed"))), Some("You are Ed".to_string()));
        assert_eq!(system_prompt_text(Some(json!(""))), None);
        assert_eq!(system_prompt_text(None), None);
        assert_eq!(current_identity_text(Some(json!([]))).unwrap(), NOT_SET);
        assert_eq!(current_identity_text(Some(json!(5))).unwrap_err().to_string(), "object of type 'int' has no len()");
        assert_eq!(current_identity_text(None).unwrap_err().to_string(), "object of type 'NoneType' has no len()");
    }
}
