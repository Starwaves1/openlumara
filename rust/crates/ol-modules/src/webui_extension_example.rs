//! Port of modules/webui_extension_example/module.py.
//!
//! The python class has no tools, commands, routes, settings or prompt hooks: its only code is
//! `async def on_ready(self): pass`. Its content lives in modules/webui_extension_example/webui/
//! (slot templates, modals, sidebars, chat/settings canaries and assets/css/test.css). The webui
//! crate (ol-webui) discovers those templates and assets from the module folder on disk, so this
//! port does not embed them. `enable_webui_extension` is added to the effective settings by
//! `ol_core::registry::module_settings` because the folder ships a `webui/` directory.

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase};
use std::sync::Arc;

#[path = "../../../generated/modules/webui_extension_example.rs"]
pub(crate) mod meta;

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
    use serde_json::json;

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
        assert!(!meta::UNSAFE);
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
}
