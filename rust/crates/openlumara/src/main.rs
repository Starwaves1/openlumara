//! OpenLumara! A modular, token-efficient AI agent framework. (Rust port of main.py)
//!
//! Made by Rose22 (https://github.com/Rose22/openlumara). Rust port: see rust/PORTING.md.
//!
//! This program is free software: you can redistribute it and/or modify it under the
//! terms of the GNU General Public License as published by the Free Software Foundation,
//! either version 2.0 of the License, or (at your option) any later version.

mod args;

use ol_core::manager::{Manager, RunResult};

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

async fn main_loop(argv: &[String]) -> anyhow::Result<RunResult> {
    let pre = args::pre_parse(argv);
    if !pre.quiet {
        let shown = pre.config.clone().unwrap_or_else(|| "config.yaml".into());
        ol_core::log("core", &format!("Loading settings from config {}", shown));
    }
    ol_core::config::load(pre.config.as_deref())?;

    let parsed = args::parse(argv)?;
    for unknown in &parsed.unknown {
        if unknown.starts_with("--") {
            ol_core::log(
                "core",
                &format!("Warning: Unrecognized argument '{}' will be ignored. The module could be disabled or the syntax could be wrong.", unknown),
            );
        }
    }
    args::override_config_with_args(&parsed.settings);

    let a = parsed.flags;
    if a.quiet {
        ol_core::globals::set_quiet(true);
    }
    if a.tmp {
        ol_core::storage::set_temporary(true);
        if !a.quiet {
            ol_core::log("core", "Temporary mode activated. Loading/saving of data disabled. Anything you store will not persist!");
        }
    }
    if a.debug {
        ol_core::globals::set_debug(true);
    }
    if a.debug_stream {
        ol_core::globals::set_debug_stream(true);
    }
    if a.insecure_tls {
        ol_core::globals::set_insecure_tls(true);
    }

    let manager = Manager::new(a);
    Ok(manager.run().await)
}

fn first_run_marker() {
    // core/__init__.py: create user folders and the firstrun marker
    let root = ol_core::paths::project_root();
    let _ = std::fs::create_dir_all(root.join("user_modules"));
    let _ = std::fs::create_dir_all(root.join("user_channels"));
    if let Ok(p) = ol_core::paths::get_data_path(Some("firstrun")) {
        if !std::path::Path::new(&p).exists() {
            ol_core::globals::set_firstrun(true);
            let _ = std::fs::write(&p, "");
        }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(root) = args::root_override(&argv) {
        ol_core::paths::set_project_root(root);
    }
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        ol_core::registry::install(build_registry());
        let _ = ol_core::config::load(args::pre_parse(&argv).config.as_deref());
        println!("{}", args::help_text());
        return;
    }
    ol_core::registry::install(build_registry());
    first_run_marker();

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    loop {
        let result = rt.block_on(main_loop(&argv));
        match result {
            Ok(RunResult::Restart) => {
                println!("{}", "-".repeat(40));
                continue;
            }
            Ok(RunResult::Exit) => break,
            Err(e) => {
                println!("Error: {}", ol_core::detail_error(&e));
                break;
            }
        }
    }
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
}
