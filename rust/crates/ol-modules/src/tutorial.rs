//! Port of modules/tutorial/module.py (no hooks besides an empty on_ready; the module's
//! content lives in its webui/ folder, served by the webui).

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase};

#[path = "../../../generated/modules/tutorial.rs"]
pub(crate) mod meta;

pub struct Tutorial {
    base: ModuleBase,
}

impl Tutorial {
    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }
}

#[async_trait]
impl Module for Tutorial {
    fn base(&self) -> &ModuleBase {
        &self.base
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
