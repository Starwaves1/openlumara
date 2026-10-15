//! Port of modules/webui_extension_example/module.py (no hooks besides an empty on_ready; the module's
//! content lives in its webui/ folder, served by the webui).

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase};

#[path = "../../../generated/modules/webui_extension_example.rs"]
pub(crate) mod meta;

pub struct WebuiExtensionExample {
    base: ModuleBase,
}

impl WebuiExtensionExample {
    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }
}

#[async_trait]
impl Module for WebuiExtensionExample {
    fn base(&self) -> &ModuleBase {
        &self.base
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
