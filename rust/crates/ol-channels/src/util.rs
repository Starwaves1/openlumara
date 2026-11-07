//! helpers shared by the channels

use ol_core::config::ConfigView;

/// python's `match network_mode` host selection used by api_bridge and simple_webui
pub fn resolve_host(config: &ConfigView) -> String {
    match config.get_str("network_mode").as_deref() {
        Some("internet") => "0.0.0.0".into(),
        Some("custom") => config.get_str("custom_host").unwrap_or_default(),
        _ => "127.0.0.1".into(),
    }
}

/// port setting (python passes the raw config value to the socket)
pub fn resolve_port(config: &ConfigView, default: u16) -> u16 {
    config
        .get1("port")
        .and_then(|v| ol_core::pyjson::as_i64(&v))
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(default)
}

/// python `print(..., flush=True)`
pub fn print_flush(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}
