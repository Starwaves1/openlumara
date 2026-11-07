//! Port of modules/tutorial/module.py. The python module has no tools, commands, routes,
//! settings or prompt hooks: its only code is an empty `on_ready`. Its content lives in
//! modules/tutorial/webui/ (welcome panel template + css).
//!
//! The webui assets are embedded byte-for-byte from the python tree so there is one source
//! of truth. Rust does not render the welcome panel or serve /ext-assets yet: the webui port
//! (rust/PORTING.md, channels/webui) must append `css_urls()` to its css files and render
//! `WELCOME_TEMPLATE` only when `Tutorial::webui_extension_enabled()` is true. That method
//! already applies python's enabled-list and loaded-module checks, so callers need no more gating.

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase};
use ol_core::pyjson::truthy;
use serde_json::Value;
use std::sync::Arc;

#[path = "../../../generated/modules/tutorial.rs"]
pub(crate) mod meta;

/// modules/tutorial/webui/templates/chat/welcome.html (jinja macro `below_message`)
pub const WELCOME_TEMPLATE: &str =
    include_str!("../../../../modules/tutorial/webui/templates/chat/welcome.html");

/// modules/tutorial/webui/assets/css/tutorial.css
pub const TUTORIAL_CSS: &str = include_str!("../../../../modules/tutorial/webui/assets/css/tutorial.css");

/// python channels/webui `_ui_asset_urls("css")` entry for the tutorial stylesheet
pub const CSS_URL: &str = "/ext-assets/tutorial/css/tutorial.css";

/// python channels/webui `_ui_extension_enabled`, given the configured value and the declared
/// default. A configured value wins, then the default, then True. Strings are false for
/// 'false', '0', 'no', 'off' or '' (case and surrounding whitespace ignored).
pub fn ui_extension_enabled(configured: Option<&Value>, declared_default: Option<&Value>) -> bool {
    let value = [configured, declared_default].into_iter().flatten().find(|v| !v.is_null());
    match value {
        None => true,
        Some(Value::String(s)) => !matches!(s.trim().to_lowercase().as_str(), "false" | "0" | "no" | "off" | ""),
        Some(v) => truthy(v),
    }
}

/// css urls the webui appends to its css files for this module (empty when the extension is off)
pub fn css_urls(enabled: bool) -> Vec<String> {
    if enabled {
        vec![CSS_URL.to_string()]
    } else {
        Vec::new()
    }
}

pub struct Tutorial {
    base: ModuleBase,
}

impl Tutorial {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }

    /// whether the webui may load this module's templates, assets and /ext-assets urls.
    /// python gates this in two places: `_ui_extension_dirs`/`_ui_asset_dirs` skip a module
    /// unless it is in its `enabled` config list and loaded by the manager, and then call
    /// `_ui_extension_enabled`. Both checks are applied here.
    pub fn webui_extension_enabled(&self) -> bool {
        let section = if self.base.is_user_module { "user_modules" } else { "modules" };
        let listed = ol_core::config::get_str_list(&[section, "enabled"]).iter().any(|n| n == self.name());
        let loaded = self.base.manager().is_some_and(|m| m.module(self.name()).is_some());
        if !listed || !loaded {
            return false;
        }
        let settings = ol_core::registry::module_settings(self.name());
        let default = settings.get("enable_webui_extension").and_then(|s| s.get("default"));
        let configured = self.base.config.get1("enable_webui_extension");
        ui_extension_enabled(configured.as_ref(), default)
    }
}

#[async_trait]
impl Module for Tutorial {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    /// python: `async def on_ready(self): pass`
    async fn on_ready(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn tutorial() -> Arc<dyn Module> {
        Tutorial::factory(ModuleBase::detached("tutorial"))
    }

    #[test]
    fn metadata_matches_python_class() {
        assert_eq!(meta::CLASS_NAME, "Tutorial");
        assert_eq!(meta::CATEGORY, "core");
        const _: () = assert!(!meta::UNSAFE);
        assert_eq!(meta::settings(), serde_json::json!({}));
        assert!(meta::tools().is_empty());
        assert!(meta::commands().is_empty());
        assert!(meta::ui_routes().is_empty());
        assert!(meta::DOC.starts_with("Guides you through openlumara if you're new!"));
    }

    #[test]
    fn contributes_nothing_to_prompts_or_tools() {
        let m = tutorial();
        assert_eq!(m.name(), "tutorial");
        assert!(m.tools().is_empty());
        assert!(m.commands().is_empty());
        assert!(m.ui_routes().is_empty());
        assert!(!m.has_background());
        assert_eq!(block_on(m.on_system_prompt()).unwrap(), None);
        assert_eq!(block_on(m.on_end_prompt()).unwrap(), None);
        assert_eq!(block_on(m.on_message_inject()).unwrap(), None);
    }

    #[test]
    fn on_ready_succeeds_without_side_effects() {
        let m = tutorial();
        block_on(m.on_ready()).expect("tutorial on_ready is a no-op that succeeds");
        assert!(m.disabled_tools().is_empty());
    }

    #[test]
    fn configured_value_wins_then_declared_default_then_true() {
        assert!(!ui_extension_enabled(Some(&json!(false)), Some(&json!(true))));
        assert!(ui_extension_enabled(Some(&json!(true)), Some(&json!(false))));
        assert!(!ui_extension_enabled(None, Some(&json!(false))));
        assert!(ui_extension_enabled(None, None));
        // a json null is python None: it falls through to the default
        assert!(!ui_extension_enabled(Some(&Value::Null), Some(&json!(false))));
        assert!(ui_extension_enabled(Some(&Value::Null), None));
    }

    #[test]
    fn string_values_use_python_falsey_words() {
        for off in ["false", "0", "no", "off", "", "  FALSE ", "Off"] {
            assert!(!ui_extension_enabled(Some(&json!(off)), None), "{off:?} should be off");
        }
        for on in ["true", "1", "yes", "anything"] {
            assert!(ui_extension_enabled(Some(&json!(on)), None), "{on:?} should be on");
        }
        assert!(!ui_extension_enabled(Some(&json!(0)), None));
        assert!(ui_extension_enabled(Some(&json!(1)), None));
    }

    #[test]
    fn webui_extension_is_off_for_a_module_the_manager_has_not_loaded() {
        // python skips a module that is not in `enabled` or not in manager.modules, whatever
        // its enable_webui_extension value: a detached base has no manager, so it is off
        let m = Tutorial::new(ModuleBase::detached("tutorial"));
        assert!(!m.webui_extension_enabled());
    }

    #[test]
    fn embedded_webui_assets_are_the_python_files_and_gate_the_css_url() {
        assert!(WELCOME_TEMPLATE.contains("{% macro below_message() %}"));
        assert!(WELCOME_TEMPLATE.contains("tutorial_welcome_hidden"));
        assert!(TUTORIAL_CSS.contains(".tut-panel"));
        assert_eq!(css_urls(true), vec!["/ext-assets/tutorial/css/tutorial.css".to_string()]);
        assert!(css_urls(false).is_empty());
    }
}
