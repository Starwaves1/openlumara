//! Data/life modules: memory, scheduler, calendar, auto_backup, characters
//! (ports of modules/<name>/module.py - see rust/PORTING.md)

pub mod auto_backup;
pub mod calendar;
pub mod characters;
pub mod memory;
pub mod scheduler;
pub mod util;

use ol_core::registry::{ModuleDescriptor, Registry};

/// register this crate's compiled-in modules/channels
pub fn register(r: &mut Registry) {
    r.add_module(ModuleDescriptor {
        name: "memory",
        class_name: memory::meta::CLASS_NAME,
        doc: memory::meta::DOC,
        settings: memory::meta::settings,
        unsafe_: memory::meta::UNSAFE,
        category: memory::meta::CATEGORY,
        factory: memory::Memory::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "scheduler",
        class_name: scheduler::meta::CLASS_NAME,
        doc: scheduler::meta::DOC,
        settings: scheduler::settings,
        unsafe_: scheduler::meta::UNSAFE,
        category: scheduler::meta::CATEGORY,
        factory: scheduler::Scheduler::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "calendar",
        class_name: calendar::meta::CLASS_NAME,
        doc: calendar::meta::DOC,
        settings: calendar::settings,
        unsafe_: calendar::meta::UNSAFE,
        category: calendar::meta::CATEGORY,
        factory: calendar::Calendar::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "auto_backup",
        class_name: auto_backup::meta::CLASS_NAME,
        doc: auto_backup::meta::DOC,
        settings: auto_backup::meta::settings,
        unsafe_: auto_backup::meta::UNSAFE,
        category: auto_backup::meta::CATEGORY,
        factory: auto_backup::AutoBackup::factory,
    });
    r.add_module(ModuleDescriptor {
        name: "characters",
        class_name: characters::meta::CLASS_NAME,
        doc: characters::meta::DOC,
        settings: characters::meta::settings,
        unsafe_: characters::meta::UNSAFE,
        category: characters::meta::CATEGORY,
        factory: characters::Characters::factory,
    });
}
