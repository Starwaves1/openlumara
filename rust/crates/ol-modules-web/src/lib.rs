//! Port of modules/{http,web_reader,web_search,file_manager,unsafe_shell}.

mod entities;
pub mod file_manager;
pub mod http;
mod netpolicy;
mod patterns;
pub mod pytext;
pub mod sanitize;
mod util;
pub mod unsafe_shell;
pub mod web_reader;

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
    r.add_module(ModuleDescriptor {
        name: "web_reader",
        class_name: web_reader::meta::CLASS_NAME,
        doc: web_reader::meta::DOC,
        // python WebReader has no settings of its own, so `self.settings` resolves to Http's
        settings: http::meta::settings,
        unsafe_: web_reader::meta::UNSAFE,
        category: web_reader::meta::CATEGORY,
        factory: web_reader::WebReader::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "unsafe_shell",
        class_name: unsafe_shell::meta::CLASS_NAME,
        doc: unsafe_shell::meta::DOC,
        settings: unsafe_shell::meta::settings,
        unsafe_: unsafe_shell::meta::UNSAFE,
        category: unsafe_shell::meta::CATEGORY,
        factory: unsafe_shell::UnsafeShell::factory,
    });
}
