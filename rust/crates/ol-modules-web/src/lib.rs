//! Port of modules/{http,web_reader,web_search,file_manager,unsafe_shell}.

mod entities;
mod patterns;
pub mod pytext;
pub mod sanitize;

/// register this crate's compiled-in modules/channels
pub fn register(_r: &mut ol_core::registry::Registry) {}
