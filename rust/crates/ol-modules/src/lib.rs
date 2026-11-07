//! (being ported - see rust/PORTING.md)

pub mod channel;
pub mod chats;
pub mod docs;
pub mod identity;
pub mod lists;
pub mod models;
pub mod modules;
pub mod notes;
pub mod replacer;
pub mod token_threshold;
pub mod tutorial;
pub mod util;
pub mod webui_extension_example;
pub mod writing_style;

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
    r.add_module(ModuleDescriptor {
        name: "replacer",
        class_name: replacer::meta::CLASS_NAME,
        doc: replacer::meta::DOC,
        settings: replacer::meta::settings,
        unsafe_: replacer::meta::UNSAFE,
        category: replacer::meta::CATEGORY,
        factory: replacer::Replacer::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "notes",
        class_name: notes::meta::CLASS_NAME,
        doc: notes::meta::DOC,
        settings: notes::meta::settings,
        unsafe_: notes::meta::UNSAFE,
        category: notes::meta::CATEGORY,
        factory: notes::Notes::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "lists",
        class_name: lists::meta::CLASS_NAME,
        doc: lists::meta::DOC,
        settings: lists::meta::settings,
        unsafe_: lists::meta::UNSAFE,
        category: lists::meta::CATEGORY,
        factory: lists::Lists::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "chats",
        class_name: chats::meta::CLASS_NAME,
        doc: chats::meta::DOC,
        settings: chats::meta::settings,
        unsafe_: chats::meta::UNSAFE,
        category: chats::meta::CATEGORY,
        factory: chats::Chats::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "models",
        class_name: models::meta::CLASS_NAME,
        doc: models::meta::DOC,
        settings: models::meta::settings,
        unsafe_: models::meta::UNSAFE,
        category: models::meta::CATEGORY,
        factory: models::Models::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "modules",
        class_name: modules::meta::CLASS_NAME,
        doc: modules::meta::DOC,
        settings: modules::meta::settings,
        unsafe_: modules::meta::UNSAFE,
        category: modules::meta::CATEGORY,
        factory: modules::Modules::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "channel",
        class_name: channel::meta::CLASS_NAME,
        doc: channel::meta::DOC,
        settings: channel::meta::settings,
        unsafe_: channel::meta::UNSAFE,
        category: channel::meta::CATEGORY,
        factory: channel::ChannelModule::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "docs",
        class_name: docs::meta::CLASS_NAME,
        doc: docs::meta::DOC,
        settings: docs::meta::settings,
        unsafe_: docs::meta::UNSAFE,
        category: docs::meta::CATEGORY,
        factory: docs::Docs::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "writing_style",
        class_name: writing_style::meta::CLASS_NAME,
        doc: writing_style::meta::DOC,
        settings: writing_style::meta::settings,
        unsafe_: writing_style::meta::UNSAFE,
        category: writing_style::meta::CATEGORY,
        factory: writing_style::WritingStyle::factory,
    });
}
