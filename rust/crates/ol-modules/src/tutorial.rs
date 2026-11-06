//! Port of modules/tutorial/module.py. The python module has no tools, commands, routes,
//! settings or prompt hooks: its only code is an empty `on_ready`. Its content lives in
//! modules/tutorial/webui/ (welcome panel template + css), which the webui serves.

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase};
use std::sync::Arc;

#[path = "../../../generated/modules/tutorial.rs"]
pub(crate) mod meta;

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
        assert!(!meta::UNSAFE);
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
}
