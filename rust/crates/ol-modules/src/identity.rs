//! Port of modules/identity/module.py

use crate::util;
use async_trait::async_trait;
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec};
use parking_lot::Mutex;
use serde_json::{json, Value};

#[path = "../../../generated/modules/identity.rs"]
pub(crate) mod meta;

pub struct Identity {
    base: ModuleBase,
    disabled: Mutex<Vec<String>>,
}

impl Identity {
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
        match self.identity() {
            Some(v) if ol_core::pyjson::truthy(&v) => Ok(Some(ol_core::pyjson::py_str(&v))),
            _ => Ok(None),
        }
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
            let identity = match self.identity() {
                Some(Value::String(s)) => s,
                Some(Value::Null) | None => anyhow::bail!("object of type 'NoneType' has no len()"),
                Some(other) => ol_core::pyjson::py_str(&other),
            };
            return Ok(Some(if !identity.is_empty() { identity } else { "You have not yet set up an identity.".into() }));
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

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
