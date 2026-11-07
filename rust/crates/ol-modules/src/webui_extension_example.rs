//! Port of modules/webui_extension_example/module.py.
//!
//! The python class has no tools, commands, routes, settings or prompt hooks: its only code is
//! `async def on_ready(self): pass`. Its content lives in modules/webui_extension_example/webui/
//! (slot templates, modals, sidebars, chat/settings canaries and assets/css/test.css).
//!
//! Templates: the webui crate (ol-webui) discovers the template folders from the module folder
//! on disk, gated by `WebuiExtensionExample::webui_extension_enabled()`. `enable_webui_extension`
//! is added to the effective settings by `ol_core::registry::module_settings` because the folder
//! ships a `webui/` directory.
//!
//! Assets: unlike templates, assets are NOT discovered from disk by ol-webui. python serves
//! `<module>/webui/assets/` under `/ext-assets/` and appends the css urls to the page's css_files.
//! As with tutorial, the one shipped asset (css/test.css) is embedded byte-for-byte here. The webui
//! port must append `css_urls()` to the page css files and route `/ext-assets/<module>/<path>` to
//! `asset()`. Until it does, the pink canary styling is not applied.

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase};
use std::sync::Arc;

#[path = "../../../generated/modules/webui_extension_example.rs"]
pub(crate) mod meta;

/// modules/webui_extension_example/webui/assets/css/test.css
pub const TEST_CSS: &str = include_str!("../../../../modules/webui_extension_example/webui/assets/css/test.css");

/// python `_ui_asset_urls("css")` entry for the canary stylesheet
pub const CSS_URL: &str = "/ext-assets/webui_extension_example/css/test.css";

pub struct WebuiExtensionExample {
    base: ModuleBase,
}

impl WebuiExtensionExample {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }

    /// whether the webui may load this module's templates, assets and /ext-assets urls.
    /// python `_ui_extension_dirs`/`_ui_asset_dirs` skip a module unless it is in its `enabled`
    /// config list and loaded by the manager, and then call `_ui_extension_enabled`. Both checks
    /// are applied here, so the webui caller needs no more gating.
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
        crate::util::ui_extension_enabled(configured.as_ref(), default)
    }

    /// python `_ui_asset_urls("css")` for this module: the css urls to append to the page's css
    /// files (empty when the extension is off)
    pub fn css_urls(&self) -> Vec<String> {
        if self.webui_extension_enabled() {
            vec![CSS_URL.to_string()]
        } else {
            Vec::new()
        }
    }

    /// python `ext_module_assets` (GET /ext-assets/<module>/<asset_path>): the content of a file
    /// under <module>/webui/assets/, or None when python would answer 404 (extension off, or no
    /// such file). The embedded set is the files shipped in that folder today: css/test.css.
    pub fn asset(&self, asset_path: &str) -> Option<&'static str> {
        if !self.webui_extension_enabled() {
            return None;
        }
        match asset_path {
            "css/test.css" => Some(TEST_CSS),
            _ => None,
        }
    }
}

#[async_trait]
impl Module for WebuiExtensionExample {
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
    use crate::util::ui_extension_enabled;
    use serde_json::{json, Value};

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn module() -> Arc<dyn Module> {
        WebuiExtensionExample::factory(ModuleBase::detached("webui_extension_example"))
    }

    fn module_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../modules/webui_extension_example")
    }

    #[test]
    fn metadata_matches_python_class() {
        assert_eq!(meta::CLASS_NAME, "WebuiExtensionExample");
        assert_eq!(meta::CATEGORY, "development");
        const _: () = assert!(!meta::UNSAFE);
        assert_eq!(meta::settings(), json!({}));
        assert!(meta::tools().is_empty());
        assert!(meta::commands().is_empty());
        assert!(meta::ui_routes().is_empty());
        assert!(meta::DOC.starts_with("Example module for the webui extension system."));
        assert!(meta::DOC.ends_with("and to verify that it all works."));
    }

    #[test]
    fn contributes_nothing_to_tools_or_prompts() {
        let m = module();
        assert_eq!(m.name(), "webui_extension_example");
        assert!(m.tools().is_empty());
        assert!(m.commands().is_empty());
        assert!(m.ui_routes().is_empty());
        assert!(!m.has_background());
        assert_eq!(block_on(m.on_system_prompt()).unwrap(), None);
        assert_eq!(block_on(m.on_end_prompt()).unwrap(), None);
        assert_eq!(block_on(m.on_message_inject()).unwrap(), None);
    }

    #[test]
    fn on_ready_is_a_noop_that_succeeds() {
        let m = module();
        block_on(m.on_ready()).expect("python on_ready is `pass`, so it must succeed");
        assert!(m.disabled_tools().is_empty());
    }

    #[test]
    fn shipped_webui_folder_has_the_canary_templates_the_module_describes() {
        let dir = module_dir().join("webui");
        for rel in [
            "templates/index.html",
            "templates/chat/header.html",
            "templates/modals/extension_test.html",
            "templates/sidebars/test_panel.html",
            "assets/css/test.css",
        ] {
            assert!(dir.join(rel).is_file(), "missing {rel}");
        }
        let css = std::fs::read_to_string(dir.join("assets/css/test.css")).unwrap();
        assert!(css.contains(".ext-test-canary"));
        let index = std::fs::read_to_string(dir.join("templates/index.html")).unwrap();
        assert!(index.contains("{% macro head() %}"));
        assert!(index.contains("ext-test-head"));
    }

    #[test]
    fn embedded_stylesheet_is_the_python_file_and_its_url_is_the_ext_assets_path() {
        let css = std::fs::read_to_string(module_dir().join("webui/assets/css/test.css")).unwrap();
        assert_eq!(TEST_CSS, css);
        assert_eq!(CSS_URL, format!("/ext-assets/{}/css/test.css", module().name()));
    }

    #[test]
    fn detached_module_serves_no_assets_and_adds_no_css() {
        // a detached base has no manager and is not in `enabled`, so python would skip it
        let m = WebuiExtensionExample::new(ModuleBase::detached("webui_extension_example"));
        assert!(!m.webui_extension_enabled());
        assert!(m.css_urls().is_empty());
        assert_eq!(m.asset("css/test.css"), None);
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
}
