//! (being ported - see rust/PORTING.md)

pub mod listing;
pub mod routes;
pub mod server;
pub mod session;
pub mod settings;
pub mod static_routes;
pub mod templates;
pub mod ui_routes;
pub mod util;
pub mod ws;

#[cfg(test)]
mod route_tests;

/// register this crate's compiled-in modules/channels
pub fn register(r: &mut ol_core::registry::Registry) {
    r.add_channel(server::descriptor());
}
