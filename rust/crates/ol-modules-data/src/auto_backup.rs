//! Port of modules/auto_backup/module.py: periodic zip backups of the data folder.
//!
//! The background loop runs in `on_background` (started by the manager at startup).
//! `on_shutdown` cancels it; since the manager doesn't restart background tasks after
//! `reload_module`, a reload's `on_ready` respawns the loop itself.

#[path = "../../../generated/modules/auto_backup.rs"]
pub mod meta;

use crate::util::{expanduser, fromisoformat, now, round2};
use anyhow::anyhow;
use async_trait::async_trait;
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec};
use ol_core::pyjson::{as_f64, py_str, truthy};
use ol_core::storage::StorageText;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct Inner {
    base: ModuleBase,
    last_backup: Mutex<Option<StorageText>>,
    backup_path: Mutex<String>,
    cancel: Mutex<CancellationToken>,
    ready_count: AtomicUsize,
}

pub struct AutoBackup {
    inner: Arc<Inner>,
}

/// writes every file below `data_folder` into a deflated zip at `zip_path`
/// (arcnames relative to the data folder, like python's os.walk + zipfile.write)
pub fn write_zip(data_folder: &Path, zip_path: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let file = std::fs::File::create(zip_path)?;
    let mut zip = zip::ZipWriter::new(file);
    for entry in walkdir::WalkDir::new(data_folder).sort_by_file_name() {
        let entry = entry?;
        let path = entry.path();
        // os.walk lists symlinks to files under `files` (zipfile follows them);
        // symlinked directories are not descended into (walkdir default)
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        if path == zip_path {
            continue;
        }
        let rel = path.strip_prefix(data_folder).unwrap_or(path);
        let arcname = rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
        let mut opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        if let Ok(mtime) = meta.modified() {
            let local: chrono::DateTime<chrono::Local> = mtime.into();
            use chrono::{Datelike, Timelike};
            if let Ok(dt) = zip::DateTime::from_date_and_time(
                local.year().clamp(1980, 2107) as u16,
                local.month() as u8,
                local.day() as u8,
                local.hour() as u8,
                local.minute() as u8,
                local.second() as u8,
            ) {
                opts = opts.last_modified_time(dt);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            opts = opts.unix_permissions(meta.permissions().mode());
        }
        if meta.len() >= 0xFFFF_FFFF {
            opts = opts.large_file(true);
        }
        zip.start_file(arcname, opts)?;
        let data = std::fs::read(path)?;
        zip.write_all(&data)?;
    }
    zip.finish()?;
    Ok(())
}

impl AutoBackup {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self {
            inner: Arc::new(Inner {
                base,
                last_backup: Mutex::new(None),
                backup_path: Mutex::new(String::new()),
                cancel: Mutex::new(CancellationToken::new()),
                ready_count: AtomicUsize::new(0),
            }),
        }
    }

    /// run one backup now (python _perform_backup)
    pub async fn perform_backup(&self) {
        self.inner.perform_backup().await
    }
}

impl Inner {
    /// python `self.channel.log(self.name, msg)` (channel.log == manager.log)
    fn log(&self, msg: &str) {
        self.base.log(&self.base.name, msg);
    }

    async fn push(&self, msg: String) {
        // python: self.channel.push(...) on the active channel
        if let Some(ch) = self.base.channel() {
            ch.push(json!(msg), true).await;
        }
    }

    fn interval(&self) -> Option<Value> {
        self.base.config.get1("backup_interval_days")
    }

    /// `not interval or interval <= 0`
    fn interval_seconds(&self) -> Option<f64> {
        let v = self.interval()?;
        if !truthy(&v) {
            return None;
        }
        let f = as_f64(&v)?;
        if f <= 0.0 {
            return None;
        }
        Some(f * 86400.0)
    }

    fn last_backup_str(&self) -> String {
        self.last_backup.lock().as_mut().map(|s| s.get()).unwrap_or_default()
    }

    fn backup_path(&self) -> String {
        self.backup_path.lock().clone()
    }

    async fn setup_backup(&self) {
        let path = self.backup_path();
        if !Path::new(&path).exists() {
            self.log(&format!("Backup directory created: {}", path));
            if let Err(e) = std::fs::create_dir_all(&path) {
                self.log(&format!("Warning: Could not create backup directory '{}': {}", path, e));
            }
        }
        let interval = self.base.config.get1("backup_interval_days").unwrap_or(json!(7));
        if truthy(&interval) && as_f64(&interval).map(|f| f > 0.0).unwrap_or(false) {
            self.log(&format!("Automatic backups enabled every {} day(s)", py_str(&interval)));
        } else {
            self.log("Automatic backups are disabled (interval set to 0)");
        }
    }

    async fn calculate_wait_time(&self, interval_seconds: f64) -> f64 {
        let last = self.last_backup_str();
        if last.is_empty() || last == "None" {
            // no previous backup exists, wait for the full interval
            return self.interval().and_then(|v| as_f64(&v)).unwrap_or(0.0) * 86400.0;
        }
        match fromisoformat(&last) {
            Ok(t) => {
                let elapsed = crate::util::total_seconds(&now(), &t);
                let remaining = interval_seconds - elapsed;
                if remaining <= 0.0 {
                    self.log("Backup interval has passed, backing up immediately");
                    return 0.0;
                }
                remaining
            }
            Err(e) => {
                self.log(&format!("Invalid last backup timestamp: {}, waiting full interval", e));
                interval_seconds
            }
        }
    }

    async fn background_loop(self: Arc<Self>, token: CancellationToken) {
        loop {
            let step = async {
                let Some(interval_seconds) = self.interval_seconds() else {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    return;
                };
                let wait_time = self.calculate_wait_time(interval_seconds).await;
                let wait_days = (wait_time / 86400.0).trunc() as i64;
                if wait_time > 0.0 {
                    self.log(&format!("Waiting {} day(s) until next backup", wait_days));
                    tokio::time::sleep(std::time::Duration::from_secs_f64(wait_time)).await;
                }
                if self.interval_seconds().is_none() {
                    return;
                }
                self.perform_backup().await;
            };
            tokio::select! {
                _ = token.cancelled() => break,
                _ = step => {}
            }
        }
    }

    async fn perform_backup(&self) {
        let notify = self.base.config.get_bool("enable_notifications");
        let data_folder = ol_core::paths::get_data_path(None).unwrap_or_default();
        if data_folder.is_empty() || !Path::new(&data_folder).exists() {
            let msg = "Backup failed: Data folder not found.\n".to_string();
            self.log(&msg);
            if notify {
                self.push(format!("⚠️ Data Backup Failed\n{}", msg)).await;
            }
            return;
        }
        let zip_filename = format!("openlumara_backup_{}.zip", now().format("%Y%m%d_%H%M%S"));
        let zip_path = PathBuf::from(self.backup_path()).join(&zip_filename);
        let zp = zip_path.clone();
        let df = PathBuf::from(&data_folder);
        let res = tokio::task::spawn_blocking(move || write_zip(&df, &zp)).await.map_err(|e| anyhow!(e)).and_then(|r| r);
        let res = res.and_then(|_| Ok(std::fs::metadata(&zip_path)?.len()));
        match res {
            Ok(size) => {
                let size_mb = size as f64 / (1024.0 * 1024.0);
                self.log(&format!("Backup completed: {} ({:.2} MB)", zip_filename, size_mb));
                if notify {
                    self.push(format!(
                        "✅ Data Backup Complete\nFile: {}\nSize: {:.2} MB\nLocation: {}",
                        zip_filename,
                        size_mb,
                        zip_path.to_string_lossy()
                    ))
                    .await;
                }
                let stamp = crate::util::iso(&now());
                if let Some(s) = self.last_backup.lock().as_mut() {
                    s.set(&stamp);
                }
                self.log(&format!("Last backup time saved: {}", self.last_backup_str()));
            }
            Err(e) => {
                let msg = format!("Backup failed with error: {}", e);
                self.log(&msg);
                if notify {
                    self.push(format!("⚠️ Data Backup Failed\n{}", msg)).await;
                }
            }
        }
    }

    fn get_backup_status(&self) -> anyhow::Result<Value> {
        let interval = self.interval().unwrap_or(Value::Null);
        let notify = self.base.config.get1("enable_notifications").unwrap_or(Value::Null);
        let data_folder = ol_core::paths::get_data_path(None)?;
        let backup_path = self.backup_path();

        let mut existing: Vec<Value> = Vec::new();
        let listing: anyhow::Result<()> = (|| {
            if Path::new(&backup_path).exists() {
                for e in std::fs::read_dir(&backup_path)? {
                    let e = e?;
                    let f = e.file_name().to_string_lossy().to_string();
                    if f.ends_with(".zip") && f.starts_with("openlumara_backup_") {
                        let meta = std::fs::metadata(e.path())?;
                        let size_mb = meta.len() as f64 / (1024.0 * 1024.0);
                        let mtime: chrono::DateTime<chrono::Local> = meta.modified()?.into();
                        existing.push(json!({
                            "filename": f,
                            "size_mb": round2(size_mb),
                            "modified": mtime.format("%Y-%m-%d %H:%M:%S").to_string(),
                        }));
                    }
                }
            }
            existing.sort_by(|a, b| py_str(&b["modified"]).cmp(&py_str(&a["modified"])));
            Ok(())
        })();
        if let Err(e) = listing {
            existing = vec![json!({"error": e.to_string()})];
        }

        let last = self.last_backup_str();
        let last_backup = if !last.is_empty() && last != "None" {
            fromisoformat(&last).map_err(|e| anyhow!(e))?.format("%Y-%m-%d %H:%M:%S").to_string()
        } else {
            "Never".to_string()
        };
        let count = existing.len();
        existing.truncate(5);
        let status = json!({
            "backup_interval_days": interval,
            "backup_path": backup_path,
            "enable_notifications": notify,
            "data_folder_path": if data_folder.is_empty() { "default (auto-detected)".to_string() } else { data_folder },
            "last_backup": last_backup,
            "existing_backups_count": count,
            "existing_backups": existing,
        });
        Ok(self.base.result(status, true))
    }
}

#[async_trait]
impl Module for AutoBackup {
    fn base(&self) -> &ModuleBase {
        &self.inner.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn call_tool(&self, method: &str, _args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "get_backup_status" => self.inner.get_backup_status(),
            _ => anyhow::bail!("'AutoBackup' object has no attribute '{}'", method),
        }
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        *self.inner.last_backup.lock() = Some(StorageText::new("last_backup", None)?);
        let configured = self.inner.base.config.get_str("backup_path").unwrap_or_default();
        *self.inner.backup_path.lock() = ol_core::paths::get_path(&expanduser(&configured))?;
        let token = CancellationToken::new();
        *self.inner.cancel.lock() = token.clone();
        self.inner.setup_backup().await;
        // the manager only starts on_background() once at startup; after a reload
        // (on_shutdown cancelled the loop) restart it ourselves
        if self.inner.ready_count.fetch_add(1, Ordering::SeqCst) > 0 {
            tokio::spawn(self.inner.clone().background_loop(token));
        }
        Ok(())
    }

    async fn on_shutdown(&self) -> anyhow::Result<()> {
        self.inner.cancel.lock().cancel();
        Ok(())
    }

    fn has_background(&self) -> bool {
        true
    }

    async fn on_background(&self) -> anyhow::Result<()> {
        let token = self.inner.cancel.lock().clone();
        self.inner.clone().background_loop(token).await;
        Ok(())
    }

    fn commands(&self) -> Vec<CommandSpec> {
        meta::commands().into_iter().map(|(c, _)| c).collect()
    }

    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        if name != "backup" {
            anyhow::bail!("unknown command {}", name);
        }
        // python returns the result dict; the command system renders it with str()
        if args.first().map(|a| a != "trigger").unwrap_or(true) {
            let status = self.inner.get_backup_status()?;
            return Ok(Some(py_str(&status)));
        }
        self.inner.perform_backup().await;
        Ok(Some(py_str(&self.inner.base.result("Manual backup triggered successfully!", true))))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
