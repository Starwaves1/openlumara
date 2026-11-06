//! (being ported - see rust/PORTING.md)

pub mod identity;
pub mod tutorial;
pub mod util;

use ol_core::registry::{ModuleDescriptor, Registry};

/// register this crate's compiled-in modules/channels
pub fn register(r: &mut Registry) {
    r.add_module(ModuleDescriptor {
        name: "identity",
        class_name: identity::meta::CLASS_NAME,
        doc: identity::meta::DOC,
        settings: identity::meta::settings,
        unsafe_: identity::meta::UNSAFE,
        category: identity::meta::CATEGORY,
        factory: identity::Identity::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "tutorial",
        class_name: tutorial::meta::CLASS_NAME,
        doc: tutorial::meta::DOC,
        settings: tutorial::meta::settings,
        unsafe_: tutorial::meta::UNSAFE,
        category: tutorial::meta::CATEGORY,
        factory: tutorial::Tutorial::factory,
    });
}
