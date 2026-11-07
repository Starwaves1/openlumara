//! (being ported - see rust/PORTING.md)

pub mod docs;
pub mod util;

use ol_core::registry::{ModuleDescriptor, Registry};

/// register this crate's compiled-in modules/channels
pub fn register(r: &mut Registry) {
    r.add_module(ModuleDescriptor {
        name: "docs",
        class_name: docs::meta::CLASS_NAME,
        doc: docs::meta::DOC,
        settings: docs::meta::settings,
        unsafe_: docs::meta::UNSAFE,
        category: docs::meta::CATEGORY,
        factory: docs::Docs::factory,
    });
}
