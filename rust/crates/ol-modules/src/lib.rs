//! (being ported - see rust/PORTING.md)

pub mod identity;
pub mod token_threshold;
pub mod tutorial;
pub mod util;
pub mod webui_extension_example;

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
    r.add_module(ModuleDescriptor {
        name: "webui_extension_example",
        class_name: webui_extension_example::meta::CLASS_NAME,
        doc: webui_extension_example::meta::DOC,
        settings: webui_extension_example::meta::settings,
        unsafe_: webui_extension_example::meta::UNSAFE,
        category: webui_extension_example::meta::CATEGORY,
        factory: webui_extension_example::WebuiExtensionExample::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "token_threshold",
        class_name: token_threshold::meta::CLASS_NAME,
        doc: token_threshold::meta::DOC,
        settings: token_threshold::meta::settings,
        unsafe_: token_threshold::meta::UNSAFE,
        category: token_threshold::meta::CATEGORY,
        factory: token_threshold::TokenThreshold::factory,
    });
}
