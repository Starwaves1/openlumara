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

/// python's `self.models` once it has been stored. python's list_models() returns the model ids when
/// connected, but when disconnected it returns connect()'s result instead: True on success or an
/// APIError on failure. Both are truthy, so `if not self.models` never reloads them and each later use
/// fails on the non-list value.
#[derive(Clone)]
enum Cache {
    List(Vec<String>),
    /// python type name of the non-list value ("bool" or "APIError")
    NotAList(&'static str),
}

impl Cache {
    /// python `for model in self.models` over the stored value
    fn ids(&self) -> anyhow::Result<Vec<String>> {
        match self {
            Cache::List(v) => Ok(v.clone()),
            Cache::NotAList(t) => anyhow::bail!("'{}' object is not iterable", t),
        }
    }
}

pub struct Models {
    base: ModuleBase,
    disabled: Mutex<Vec<String>>,
    /// python: self.models (None until loaded; an empty list counts as not loaded)
    models: Mutex<Option<Cache>>,
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

    /// python list_models(): the sorted ids when connected; when disconnected, connect()'s result
    async fn list_models(&self) -> anyhow::Result<Cache> {
        let api = self.manager()?.api();
        if api.connected() {
            let ids = api.list_models().await.map_err(|e| anyhow::anyhow!("{}", e))?;
            return Ok(Cache::List(ids));
        }
        // ol-core's list_models() would turn connect() into an empty list, so connect here like python does
        Ok(match api.attempt_connect().await {
            Ok(()) => Cache::NotAList("bool"),
            Err(_) => Cache::NotAList("APIError"),
        })
    }

    /// python `if self.models`: the stored value, or None when python would reload
    fn cached(&self) -> Option<Cache> {
        self.models.lock().clone().filter(|c| !matches!(c, Cache::List(v) if v.is_empty()))
    }

    /// python _load_models(): returns the stored value (None if it couldn't be loaded)
    async fn load_models(&self) -> anyhow::Result<Option<Cache>> {
        if let Some(c) = self.cached() {
            return Ok(Some(c));
        }
        let fetched = self.list_models().await?;
        if matches!(&fetched, Cache::List(v) if v.is_empty()) {
            return Ok(None);
        }
        *self.models.lock() = Some(fetched.clone());
        Ok(Some(fetched))
    }

    /// python switch(name): `name` is whatever the caller passed (a JSON value for the tool, a str for the command)
    async fn switch(&self, name: &Value) -> anyhow::Result<Value> {
        let Some(cache) = self.load_models().await? else {
            return Ok(Value::Null);
        };
        let models = cache.ids()?;
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
            let Some(cache) = self.load_models().await? else {
                return Ok(if output.is_empty() { None } else { Some(output) });
            };
            let models = match cache {
                Cache::List(v) => v,
                Cache::NotAList(t) => anyhow::bail!("object of type '{}' has no len()", t),
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
                let Some(cache) = self.load_models().await? else { anyhow::bail!("'NoneType' object is not iterable") };
                Ok(self.base.result(json!(cache.ids()?), true))
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
                let models = match self.load_models().await? {
                    Some(Cache::List(v)) => v,
                    _ => anyhow::bail!("can only join an iterable"),
                };
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
        setup_with_api(current, insert_available, preloaded, None)
    }

    /// like setup(), with an optional api url in the config (the api is disconnected either way)
    fn setup_with_api(current: &str, insert_available: bool, preloaded: Option<Vec<&str>>, api_url: Option<&str>) -> (Arc<Manager>, Models) {
        let mut data = json!({
            "model": {"name": current},
            "modules": {"settings": {"models": {
                "insert_current_model_into_system_prompt": true,
                "insert_available_models_into_system_prompt": insert_available,
            }}},
        });
        if let Some(url) = api_url {
            data["api"] = json!({"url": url});
        }
        ol_core::config::set_for_tests(data.clone());
        // set_for_tests() keeps a config file from an earlier test and every read reloads it,
        // so the same data is written to it (JSON is valid YAML)
        let path = ol_core::config::config_path().expect("test config path");
        std::fs::write(&path, data.to_string()).expect("write test config");
        let mgr = Manager::new(Args::default());
        let models = Models::new(ModuleBase::new("models", false, Arc::downgrade(&mgr)));
        *models.models.lock() = preloaded.map(|v| Cache::List(v.into_iter().map(String::from).collect()));
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

    /// a local stand-in for the api: every request gets a one-model list, so connect() succeeds
    fn spawn_models_server() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let body = r#"{"data":[{"id":"m1"}]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        format!("http://{}/v1", addr)
    }

    #[test]
    fn disconnected_load_failure_caches_python_apierror_and_fails_each_use() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // the placeholder url makes connect() fail without any network traffic
        let (_mgr, m) = setup_with_api("llama-3", false, None, Some("http://API_URL_HERE/v1"));
        let err = block_on(m.call_tool("get_available", ToolArgs::new(Map::new()))).unwrap_err();
        assert_eq!(err.to_string(), "'APIError' object is not iterable");
        // python stored the APIError (truthy), so the list is never retried
        let err = block_on(m.run_command("models", vec![])).unwrap_err();
        assert_eq!(err.to_string(), "can only join an iterable");
        let err = block_on(m.run_command("model", vec!["qwen".into()])).unwrap_err();
        assert_eq!(err.to_string(), "'APIError' object is not iterable");
    }

    #[test]
    fn system_prompt_with_cached_apierror_raises_python_len_error() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_mgr, m) = setup_with_api("llama-3", true, None, Some("http://API_URL_HERE/v1"));
        let err = block_on(m.on_system_prompt()).unwrap_err();
        assert_eq!(err.to_string(), "object of type 'APIError' has no len()");
    }

    #[test]
    fn first_connect_caches_python_true_and_later_calls_still_fail_on_bool() {
        let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let url = spawn_models_server();
        let (_mgr, m) = setup_with_api("llama-3", false, None, Some(&url));
        // python's first list_models() connects and returns True, which is cached as self.models
        let err = block_on(m.call_tool("get_available", ToolArgs::new(Map::new()))).unwrap_err();
        assert_eq!(err.to_string(), "'bool' object is not iterable");
        // the connection is now up, but the cached True is never replaced
        let err = block_on(m.call_tool("get_available", ToolArgs::new(Map::new()))).unwrap_err();
        assert_eq!(err.to_string(), "'bool' object is not iterable");
        let args = ToolArgs::new(json!({"name": "m1"}).as_object().unwrap().clone());
        let err = block_on(m.call_tool("switch", args)).unwrap_err();
        assert_eq!(err.to_string(), "'bool' object is not iterable");
    }
}
