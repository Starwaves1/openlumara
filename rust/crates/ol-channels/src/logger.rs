//! Port of channels/logger: appends every console log line to a file.
//!
//! File format (identical to python): `"{%c timestamp} | {CATEGORY} | {message}\n"`,
//! with a 40-dash divider line written when appending to an existing file. A relative
//! `path` is resolved against the process working directory, like python's `open()`.

#[path = "../../../generated/channels/logger.rs"]
mod meta;

use ol_core::channel::{Channel, ChannelBehavior};
use parking_lot::Mutex;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;

crate::descriptor_from_meta!("logger", meta::settings, factory);

fn factory() -> Arc<dyn ChannelBehavior> {
    let path = ol_core::config::get_str(&["channels", "settings", "logger", "path"]).unwrap_or_else(|| "lumara.log".into());
    Arc::new(Logger::open(&path))
}

pub struct Logger {
    pub path: String,
    file: Mutex<Option<File>>,
    /// python logs "started logging to file ..." from __init__, before the channel is
    /// registered: the manager prints and buffers it, and the startup drain writes it
    /// to the file right before "loaded channel : logger". The Rust factory has no
    /// manager access, so the line is printed here and written to the file at that
    /// same position when the drain replays the buffer.
    pending_started: Mutex<Option<String>>,
}

impl Logger {
    pub fn open(path: &str) -> Self {
        let mut pending = None;
        let file = match open_logfile(path) {
            Ok(f) => {
                let msg = format!("started logging to file {}", path);
                if !ol_core::quiet() {
                    crate::util::print_flush(&format!("[LOGGER] {}\n", msg));
                    pending = Some(msg);
                }
                Some(f)
            }
            Err(e) => {
                crate::util::print_flush(&format!("Error while opening log file: {}\n", ol_core::detail_error(&e)));
                None
            }
        };
        Logger { path: path.to_string(), file: Mutex::new(file), pending_started: Mutex::new(pending) }
    }

    fn write_line(&self, category: &str, message: &str) {
        let mut guard = self.file.lock();
        let Some(f) = guard.as_mut() else { return };
        let timestamp = chrono::Local::now().format("%c");
        let _ = f.write_all(format_line(&timestamp.to_string(), category, message).as_bytes());
        let _ = f.flush();
    }
}

/// open in append mode; write the divider if the file already existed
fn open_logfile(path: &str) -> std::io::Result<File> {
    let existed = std::path::Path::new(path).exists();
    let mut f = std::fs::OpenOptions::new().append(true).create(true).open(path)?;
    if existed {
        f.write_all(format!("{}\n", "-".repeat(40)).as_bytes())?;
        f.flush()?;
    }
    Ok(f)
}

pub fn format_line(timestamp: &str, category: &str, message: &str) -> String {
    format!("{} | {} | {}\n", timestamp, category.to_uppercase(), message)
}

#[async_trait::async_trait]
impl ChannelBehavior for Logger {
    fn on_log(&self, _ch: &Arc<Channel>, category: &str, message: &str) {
        if category == "core" && message == "loaded channel : logger" {
            if let Some(started) = self.pending_started.lock().take() {
                self.write_line("logger", &started);
            }
        }
        self.write_line(category, message);
    }

    async fn on_shutdown(&self, _ch: &Arc<Channel>) -> anyhow::Result<()> {
        // python closes the file; later logs are ignored
        self.file.lock().take();
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_format() {
        assert_eq!(format_line("Wed Oct 14 12:00:00 2026", "api", "hi"), "Wed Oct 14 12:00:00 2026 | API | hi\n");
        assert_eq!(format_line("t", "", "----"), "t |  | ----\n");
    }

    #[test]
    fn divider_only_for_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.log");
        let ps = p.to_str().unwrap();
        let l = Logger::open(ps);
        l.write_line("core", "one");
        drop(l);
        let l = Logger::open(ps);
        l.write_line("core", "two");
        drop(l);
        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with(" | CORE | one"));
        assert_eq!(lines[1], "-".repeat(40));
        assert!(lines[2].ends_with(" | CORE | two"));
    }
}
