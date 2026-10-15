//! Port of the "simple" python channels: cli, cli_lite, logger, ntfy, api_bridge and
//! simple_webui (see rust/PORTING.md). One file per channel; each registers a
//! [`ChannelDescriptor`] carrying the class metadata generated from the python source.

pub mod api_bridge;
pub mod cli;
pub mod cli_lite;
pub mod logger;
pub mod ntfy;
pub mod simple_webui;
mod util;

use ol_core::registry::Registry;

/// register this crate's compiled-in channels
pub fn register(r: &mut Registry) {
    r.add_channel(cli::descriptor());
    r.add_channel(cli_lite::descriptor());
    r.add_channel(logger::descriptor());
    r.add_channel(ntfy::descriptor());
    r.add_channel(api_bridge::descriptor());
    r.add_channel(simple_webui::descriptor());
}

/// builds a descriptor from a generated `meta` module
macro_rules! descriptor_from_meta {
    ($name:literal, $settings:expr, $factory:expr) => {
        pub fn descriptor() -> ol_core::registry::ChannelDescriptor {
            ol_core::registry::ChannelDescriptor {
                name: $name,
                class_name: meta::CLASS_NAME,
                doc: meta::DOC,
                settings: $settings,
                unsafe_: meta::UNSAFE,
                category: meta::CATEGORY,
                factory: $factory,
            }
        }
    };
}
pub(crate) use descriptor_from_meta;
