//! (being ported - see rust/PORTING.md)

pub mod util;

use ol_core::registry::{ModuleDescriptor, Registry};

/// register this crate's compiled-in modules/channels
pub fn register(_r: &mut Registry) {}
