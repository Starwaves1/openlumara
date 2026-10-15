//! Chat-service channels ported from python: `channels/telegram`, `channels/discord_bot`
//! and `channels/matrix`. See rust/PORTING.md.

pub mod discord_bot;
pub mod matrix;
pub mod telegram;
mod util;

/// register this crate's compiled-in channels
pub fn register(r: &mut ol_core::registry::Registry) {
    telegram::register(r);
    discord_bot::register(r);
    matrix::register(r);
}
