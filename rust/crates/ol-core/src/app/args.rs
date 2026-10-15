//! Command line parsing (port of the argparse logic in main.py).
//!
//! Besides the fixed flags, every leaf of the config schema is accepted as
//! `--section.key value` (e.g. `--api.url http://...`, `--modules.settings.memory.x 1`),
//! overriding the live config for this run without saving it. Lists take comma
//! separated values, booleans accept yes/true/t/y/1 and no/false/f/n/0.

use crate::manager::Args;
use serde_json::{Map, Value};

pub struct PreArgs {
    pub config: Option<String>,
    pub quiet: bool,
}

pub struct Parsed {
    pub flags: Args,
    /// (dotted key, raw string value)
    pub settings: Vec<(String, String)>,
    pub unknown: Vec<String>,
}

const BOOL_FLAGS: [&str; 9] = ["pure", "tmp", "cli", "coder", "quiet", "insecure_tls", "debug", "debug_stream", "disable_auto_installer"];

fn split_kv(arg: &str) -> (String, Option<String>) {
    let body = arg.trim_start_matches("--");
    match body.split_once('=') {
        Some((k, v)) => (k.to_string(), Some(v.to_string())),
        None => (body.to_string(), None),
    }
}

pub fn pre_parse(argv: &[String]) -> PreArgs {
    let mut config = None;
    let mut quiet = false;
    let mut i = 0;
    while i < argv.len() {
        let (k, v) = split_kv(&argv[i]);
        if argv[i].starts_with("--") {
            if k == "config" {
                config = v.or_else(|| {
                    i += 1;
                    argv.get(i).cloned()
                });
            } else if k == "quiet" {
                quiet = true;
            }
        }
        i += 1;
    }
    PreArgs { config, quiet }
}

/// `--root <path>`: Rust-only flag to point at the openlumara checkout (assets, data, config)
pub fn root_override(argv: &[String]) -> Option<String> {
    let mut i = 0;
    while i < argv.len() {
        let (k, v) = split_kv(&argv[i]);
        if argv[i].starts_with("--") && k == "root" {
            return v.or_else(|| argv.get(i + 1).cloned());
        }
        i += 1;
    }
    None
}

/// every leaf path of the (flattened) config schema with its default value
fn schema_leaves() -> Vec<(String, Value)> {
    fn walk(prefix: &str, v: &Map<String, Value>, out: &mut Vec<(String, Value)>) {
        for (k, val) in v {
            let name = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
            match val {
                Value::Object(m) if m.contains_key("default") => out.push((name, m["default"].clone())),
                Value::Object(m) => walk(&name, m, out),
                other => out.push((name, other.clone())),
            }
        }
    }
    let mut out = Vec::new();
    walk("", &crate::config::get_schema(), &mut out);
    out
}

pub fn parse(argv: &[String]) -> anyhow::Result<Parsed> {
    let leaves = schema_leaves();
    let mut flags = Args::default();
    let mut settings = Vec::new();
    let mut unknown = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if !arg.starts_with("--") {
            unknown.push(arg.clone());
            i += 1;
            continue;
        }
        let (key, inline) = split_kv(arg);
        if BOOL_FLAGS.contains(&key.as_str()) {
            match key.as_str() {
                "pure" => flags.pure = true,
                "tmp" => flags.tmp = true,
                "cli" => flags.cli = true,
                "coder" => flags.coder = true,
                "quiet" => flags.quiet = true,
                "insecure_tls" => flags.insecure_tls = true,
                "debug" => flags.debug = true,
                "debug_stream" => flags.debug_stream = true,
                "disable_auto_installer" => flags.disable_auto_installer = true,
                _ => {}
            }
            i += 1;
            continue;
        }
        if key == "config" || key == "root" {
            let v = inline.or_else(|| {
                i += 1;
                argv.get(i).cloned()
            });
            if key == "config" {
                flags.config = v;
            }
            i += 1;
            continue;
        }
        if leaves.iter().any(|(k, _)| *k == key) {
            let v = match inline {
                Some(v) => v,
                None => {
                    i += 1;
                    match argv.get(i) {
                        Some(v) => v.clone(),
                        None => anyhow::bail!("argument --{}: expected one argument", key),
                    }
                }
            };
            settings.push((key, v));
            i += 1;
            continue;
        }
        unknown.push(arg.clone());
        i += 1;
    }
    Ok(Parsed { flags, settings, unknown })
}

fn str2bool(v: &str) -> anyhow::Result<bool> {
    match v.to_lowercase().as_str() {
        "yes" | "true" | "t" | "y" | "1" => Ok(true),
        "no" | "false" | "f" | "n" | "0" => Ok(false),
        _ => anyhow::bail!("Boolean value expected."),
    }
}

/// typed conversion based on the current value's type (argparse type=type(default))
fn convert(current: &Value, raw: &str) -> anyhow::Result<Value> {
    Ok(match current {
        Value::Array(_) => Value::Array(raw.split(',').map(|s| Value::String(s.trim().to_string())).collect()),
        Value::Bool(_) => Value::Bool(str2bool(raw)?),
        Value::Number(n) if n.is_i64() || n.is_u64() => Value::from(raw.trim().parse::<i64>().map_err(|_| anyhow::anyhow!("invalid int value: '{}'", raw))?),
        Value::Number(_) => serde_json::Number::from_f64(raw.trim().parse::<f64>().map_err(|_| anyhow::anyhow!("invalid float value: '{}'", raw))?)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        _ => Value::String(raw.to_string()),
    })
}

/// update the live config in place (not saved), only where the path already exists
pub fn override_config_with_args(settings: &[(String, String)]) {
    for (key, raw) in settings {
        let parts: Vec<&str> = key.split('.').collect();
        let mut root = Value::Object(crate::config::snapshot());
        let pointer: String = parts.iter().map(|p| format!("/{}", p.replace('~', "~0").replace('/', "~1"))).collect();
        let parent_ptr: String = parts[..parts.len() - 1].iter().map(|p| format!("/{}", p.replace('~', "~0").replace('/', "~1"))).collect();
        let last = parts[parts.len() - 1];
        let parent_is_obj = root.pointer(&parent_ptr).map(|v| v.is_object()).unwrap_or(false);
        let Some(current) = root.pointer(&pointer).cloned() else { continue };
        if !parent_is_obj {
            continue;
        }
        match convert(&current, raw) {
            Ok(v) => {
                *root.pointer_mut(&pointer).unwrap() = v;
                if let Value::Object(m) = root {
                    crate::config::replace_in_memory(m);
                }
                crate::log("core", &format!("overrode setting {} with: {}", last, raw));
            }
            Err(e) => {
                eprintln!("error: argument --{}: {}", key, e);
                std::process::exit(2);
            }
        }
    }
}

pub fn help_text() -> String {
    let mut out = String::new();
    out.push_str("usage: openlumara [-h] [--config <path>] [--root <path>] [--pure] [--tmp] [--cli] [--coder] [--quiet]\n");
    out.push_str("                  [--insecure_tls] [--debug] [--debug_stream] [--disable_auto_installer] [--<setting> <value> ...]\n\n");
    out.push_str("main:\n");
    for (f, h) in [
        ("--config <path>", "specify a specific config file to load"),
        ("--root <path>", "path of the openlumara checkout (web assets, data, config). Rust port only"),
        ("--pure", "disables all non-essential modules so that system prompt is blank and you're talking to the bare model"),
        ("--tmp", "temporary session, discards all data after shutdown"),
        ("--cli", "CLI-only mode"),
        ("--coder", "enable only the coder module (coding agent mode)"),
        ("--quiet", "surpress logs"),
        ("--insecure_tls", "Disable verification for SSL/TLS certs. Use when your API uses self-signed or unvalid certificates."),
        ("--debug", "Enable debug mode (display all warnings and errors)"),
        ("--debug_stream", "Display debug information for all streamed tokens"),
        ("--disable_auto_installer", "Accepted for compatibility; the Rust port has no dependency auto-installer"),
    ] {
        out.push_str(&format!("  {:<28} {}\n", f, h));
    }
    out.push_str("\nsettings:\n");
    for (k, v) in schema_leaves() {
        let meta = match v {
            Value::Array(_) => "<multiple,values>".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => "<value>".to_string(),
            other => crate::pyjson::py_str(&other),
        };
        out.push_str(&format!("  --{} {}\n", k, meta));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_types() {
        assert_eq!(convert(&Value::from(3), "7").unwrap(), Value::from(7));
        assert_eq!(convert(&Value::Bool(true), "no").unwrap(), Value::Bool(false));
        assert_eq!(convert(&serde_json::json!(["a"]), "x, y").unwrap(), serde_json::json!(["x", "y"]));
        assert!(convert(&Value::Bool(true), "maybe").is_err());
    }

    #[test]
    fn preparse() {
        let a: Vec<String> = ["--config=foo.yaml", "--quiet"].iter().map(|s| s.to_string()).collect();
        let p = pre_parse(&a);
        assert_eq!(p.config.as_deref(), Some("foo.yaml"));
        assert!(p.quiet);
    }
}
