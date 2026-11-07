//! Application entry point (port of main.py), shared by the `openlumara` binary and by
//! per-crate example runners used for smoke tests.

pub mod args;

use crate::manager::{Manager, RunResult};
use crate::registry::Registry;

async fn main_loop(argv: &[String]) -> anyhow::Result<RunResult> {
    let pre = args::pre_parse(argv);
    if !pre.quiet {
        let shown = pre.config.clone().unwrap_or_else(|| "config.yaml".into());
        crate::log("core", &format!("Loading settings from config {}", shown));
    }
    crate::config::load(pre.config.as_deref())?;

    let parsed = args::parse(argv)?;
    for unknown in &parsed.unknown {
        if unknown.starts_with("--") {
            crate::log(
                "core",
                &format!("Warning: Unrecognized argument '{}' will be ignored. The module could be disabled or the syntax could be wrong.", unknown),
            );
        }
    }
    args::override_config_with_args(&parsed.settings);

    let a = parsed.flags;
    if a.quiet {
        crate::globals::set_quiet(true);
    }
    if a.tmp {
        crate::storage::set_temporary(true);
        if !a.quiet {
            crate::log("core", "Temporary mode activated. Loading/saving of data disabled. Anything you store will not persist!");
        }
    }
    if a.debug {
        crate::globals::set_debug(true);
    }
    if a.debug_stream {
        crate::globals::set_debug_stream(true);
    }
    if a.insecure_tls {
        crate::globals::set_insecure_tls(true);
    }

    let manager = Manager::new(a);
    Ok(manager.run().await)
}

fn first_run_marker() {
    // core/__init__.py: create user folders and the firstrun marker
    let root = crate::paths::project_root();
    let _ = std::fs::create_dir_all(root.join("user_modules"));
    let _ = std::fs::create_dir_all(root.join("user_channels"));
    if let Ok(p) = crate::paths::get_data_path(Some("firstrun")) {
        if !std::path::Path::new(&p).exists() {
            crate::globals::set_firstrun(true);
            let _ = std::fs::write(&p, "");
        }
    }
}

/// run openlumara with the given compiled-in registry and command line arguments
/// (without the program name). loops while a restart is requested.
pub fn run(registry: Registry, argv: Vec<String>) {
    if let Some(root) = args::root_override(&argv) {
        crate::paths::set_project_root(root);
    }
    crate::registry::install(registry);
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        let _ = crate::config::load(args::pre_parse(&argv).config.as_deref());
        println!("{}", args::help_text());
        return;
    }
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
                println!("Error: {}", crate::detail_error(&e));
                break;
            }
        }
    }
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
}
