//! OpenLumara! A modular, token-efficient AI agent framework. (Rust port of main.py)
//!
//! Made by Rose22 (https://github.com/Rose22/openlumara). Rust port: see rust/PORTING.md.
//!
//! This program is free software: you can redistribute it and/or modify it under the
//! terms of the GNU General Public License as published by the Free Software Foundation,
//! either version 2.0 of the License, or (at your option) any later version.

fn build_registry() -> ol_core::registry::Registry {
    let mut r = ol_core::registry::Registry::new();
    ol_modules::register(&mut r);
    ol_modules_data::register(&mut r);
    ol_modules_web::register(&mut r);
    ol_modules_dev::register(&mut r);
    ol_channels::register(&mut r);
    ol_channels_chat::register(&mut r);
    ol_webui::register(&mut r);
    r
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    ol_core::app::run(build_registry(), argv);
}
