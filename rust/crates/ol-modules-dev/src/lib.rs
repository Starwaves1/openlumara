//! Port of modules/{shell,coder,mcp}.
pub mod coder;
pub mod pyos;

use ol_core::registry::{ModuleDescriptor, Registry};

/// register this crate's compiled-in modules/channels
pub fn register(r: &mut Registry) {
    r.add_module(ModuleDescriptor {
        name: "coder",
        class_name: coder::meta::CLASS_NAME,
        doc: coder::meta::DOC,
        settings: coder::meta::settings,
        unsafe_: coder::meta::UNSAFE,
        category: coder::meta::CATEGORY,
        factory: coder::Coder::factory,
    });
}
