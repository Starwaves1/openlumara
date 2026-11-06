//! Port of modules/{http,web_reader,web_search,file_manager,unsafe_shell}.

mod entities;
pub mod file_manager;
pub mod http;
mod netpolicy;
mod patterns;
pub mod pytext;
pub mod sanitize;
mod util;

use ol_core::registry::{ModuleDescriptor, Registry};

/// register this crate's compiled-in modules/channels
pub fn register(r: &mut Registry) {
    r.add_module(ModuleDescriptor {
        name: "http",
        class_name: http::meta::CLASS_NAME,
        doc: http::meta::DOC,
        settings: http::meta::settings,
        unsafe_: http::meta::UNSAFE,
        category: http::meta::CATEGORY,
        factory: http::Http::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "file_manager",
        class_name: file_manager::meta::CLASS_NAME,
        doc: file_manager::meta::DOC,
        settings: file_manager::meta::settings,
        unsafe_: file_manager::meta::UNSAFE,
        category: file_manager::meta::CATEGORY,
        factory: file_manager::FileManager::factory,
    });
}
