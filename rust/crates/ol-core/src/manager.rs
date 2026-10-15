//! Port of core/manager.py: the central object that connects channels, modules and the API.

use crate::api::ApiClient;
use crate::channel::Channel;
use crate::config;
use crate::module::{CommandSpec, Module, ModuleBase};
use crate::module_manager::ModuleManager;
use crate::storage::{StorageDict, StorageType};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

/// parsed command line flags (python argparse namespace)
#[derive(Clone, Debug, Default)]
pub struct Args {
    pub config: Option<String>,
    pub pure: bool,
    pub tmp: bool,
    pub cli: bool,
    pub coder: bool,
    pub quiet: bool,
    pub insecure_tls: bool,
    pub debug: bool,
    pub debug_stream: bool,
    pub disable_auto_installer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunResult {
    Exit,
    Restart,
}

static GLOBAL_INSTANCE: Lazy<Mutex<Weak<Manager>>> = Lazy::new(|| Mutex::new(Weak::new()));

/// the running manager, if any (core.manager.global_instance)
pub fn global_instance() -> Option<Arc<Manager>> {
    GLOBAL_INSTANCE.lock().upgrade()
}

pub struct Manager {
    pub args: Args,
    self_ref: Weak<Manager>,
    api: Arc<ApiClient>,
    savedata: Mutex<Option<StorageDict>>,
    channels: RwLock<IndexMap<String, Arc<Channel>>>,
    current: RwLock<Option<Arc<Channel>>>,
    modules: RwLock<IndexMap<String, Arc<dyn Module>>>,
    module_manager: ModuleManager,
    broken_modules: Mutex<Vec<String>>,
    pure_mode: AtomicBool,
    coding_mode: AtomicBool,
    restart_requested: AtomicBool,
    prevent_double_shutdown: AtomicBool,
    log_buffer: Mutex<Vec<(String, String)>>,
    started: AtomicBool,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    shutdown_notify: tokio::sync::Notify,
}

impl Manager {
    pub fn new(args: Args) -> Arc<Self> {
        Arc::new_cyclic(|w: &Weak<Manager>| {
            let api = Arc::new(ApiClient::new());
            api.set_manager(w.clone());
            Manager {
                args,
                self_ref: w.clone(),
                api,
                savedata: Mutex::new(None),
                channels: RwLock::new(IndexMap::new()),
                current: RwLock::new(None),
                modules: RwLock::new(IndexMap::new()),
                module_manager: ModuleManager::new(w.clone()),
                broken_modules: Mutex::new(Vec::new()),
                pure_mode: AtomicBool::new(false),
                coding_mode: AtomicBool::new(false),
                restart_requested: AtomicBool::new(false),
                prevent_double_shutdown: AtomicBool::new(false),
                log_buffer: Mutex::new(Vec::new()),
                started: AtomicBool::new(false),
                tasks: Mutex::new(Vec::new()),
                shutdown_notify: tokio::sync::Notify::new(),
            }
        })
    }

    // --- accessors ---
    pub fn api(&self) -> Arc<ApiClient> {
        self.api.clone()
    }
    pub fn module_manager(&self) -> &ModuleManager {
        &self.module_manager
    }
    pub fn pure_mode(&self) -> bool {
        self.pure_mode.load(Ordering::SeqCst)
    }
    pub fn coding_mode(&self) -> bool {
        self.coding_mode.load(Ordering::SeqCst)
    }
    pub fn started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }
    pub fn modules(&self) -> IndexMap<String, Arc<dyn Module>> {
        self.modules.read().clone()
    }
    pub fn module(&self, name: &str) -> Option<Arc<dyn Module>> {
        self.modules.read().get(name).cloned()
    }
    /// downcast a loaded module to its concrete type
    pub fn module_as<T: 'static>(&self, name: &str) -> Option<Arc<dyn Module>> {
        self.module(name).filter(|m| m.as_any().is::<T>())
    }
    pub fn channels(&self) -> IndexMap<String, Arc<Channel>> {
        self.channels.read().clone()
    }
    pub fn channel(&self, name: &str) -> Option<Arc<Channel>> {
        self.channels.read().get(name).cloned()
    }
    /// the currently active channel (python: manager.channel)
    pub fn current_channel(&self) -> Option<Arc<Channel>> {
        self.current.read().clone()
    }
    /// make a channel the active one and remember it in the save data
    pub fn set_current_channel(&self, ch: Arc<Channel>) {
        let name = ch.name.clone();
        *self.current.write() = Some(ch);
        self.savedata_set("last_channel", json!(name));
    }
    pub fn is_broken(&self, module: &str) -> bool {
        self.broken_modules.lock().iter().any(|m| m == module)
    }
    pub fn mark_broken(&self, module: &str) {
        self.broken_modules.lock().push(module.to_string());
    }
    pub fn savedata_get(&self, key: &str) -> Option<Value> {
        self.savedata.lock().as_mut().and_then(|s| s.get(key).cloned())
    }
    pub fn savedata_set(&self, key: &str, value: Value) {
        if let Some(s) = self.savedata.lock().as_mut() {
            s.set(key, value);
            let _ = s.save();
        }
    }

    /// active tools of the current channel (python: manager.tools)
    pub fn tools(&self) -> Vec<Value> {
        self.current_channel().map(|c| c.tool_loader.active_tools()).unwrap_or_default()
    }
    pub fn tool_names(&self) -> Vec<String> {
        self.current_channel().map(|c| c.tool_loader.active_names()).unwrap_or_default()
    }

    /// commands declared by all loaded modules
    pub fn all_module_commands(&self) -> Vec<CommandSpec> {
        self.modules().values().flat_map(|m| m.commands()).collect()
    }

    /// propagate a log line to every channel (or print it before startup / without a cli channel)
    pub fn log(&self, category: &str, message: &str) {
        let has_cli = self.channels.read().contains_key("cli");
        if (!self.started() || !has_cli) && !crate::globals::quiet() {
            let cat = if category.is_empty() { String::new() } else { format!("[{}] ", category.to_uppercase()) };
            println!("{}{}", cat, message);
            let mut buf = self.log_buffer.lock();
            buf.push((category.to_string(), message.to_string()));
            // python lets this grow forever when no cli channel exists; cap it
            if buf.len() > 5000 {
                let excess = buf.len() - 5000;
                buf.drain(..excess);
            }
            return;
        }
        for ch in self.channels().values() {
            ch.behavior.on_log(ch, category, message);
        }
    }

    pub fn log_error(&self, message: &str, e: &str) {
        for ch in self.channels().values() {
            ch.log_error(message, e);
        }
    }

    fn drain_log_buffers(&self) {
        let buf: Vec<(String, String)> = std::mem::take(&mut *self.log_buffer.lock());
        for (cat, msg) in buf {
            for (name, ch) in self.channels().iter() {
                if name == "cli" {
                    continue;
                }
                ch.behavior.on_log(ch, &cat, &msg);
            }
        }
    }

    fn spawn(&self, fut: impl std::future::Future<Output = ()> + Send + 'static) {
        self.tasks.lock().push(tokio::spawn(fut));
    }

    async fn load_channels(self: &Arc<Self>, section: &str, enabled: &[String], is_user: bool) {
        let mut names: Vec<String> = enabled.to_vec();
        names.sort();
        names.dedup();
        let is_user_str = if is_user { "user " } else { "" };
        for name in names {
            let Some(desc) = (if is_user { None } else { crate::registry::channel(&name) }) else {
                if config::available_names(section).contains(&name) {
                    self.log(&name, "Warning: loading skipped: this channel is not available in the Rust port of OpenLumara");
                }
                continue;
            };
            if name == "cli" && !std::io::stdout().is_terminal() {
                continue;
            }
            let behavior = (desc.factory)();
            let ch = match Channel::new(self, &name, is_user, behavior) {
                Ok(c) => c,
                Err(e) => {
                    crate::log(&name, &format!("Error while loading channel: {}", crate::detail_error(&e)));
                    continue;
                }
            };
            ch.init().await;
            self.channels.write().insert(name.clone(), ch);
            self.log("core", &format!("loaded {}channel : {}", is_user_str, name));
        }
    }

    async fn load_modules(self: &Arc<Self>, section: &str, enabled: &[String], is_user: bool) {
        let mut names: Vec<String> = enabled.to_vec();
        names.sort();
        names.dedup();
        let is_user_str = if is_user { "user " } else { "" };
        for name in names {
            let Some(desc) = (if is_user { None } else { crate::registry::module(&name) }) else {
                if config::available_names(section).contains(&name) {
                    self.log(&name, "Warning: loading skipped: this module is not available in the Rust port of OpenLumara");
                }
                continue;
            };
            let base = ModuleBase::new(&name, is_user, self.self_ref.clone());
            let module = (desc.factory)(base);
            if let Err(e) = module.on_ready().await {
                self.log("module error", &format!("{}: in on_ready(): {}", name, crate::detail_error(&e)));
            }
            if module.has_background() {
                let m = module.clone();
                let mgr = self.self_ref.clone();
                let n = name.clone();
                self.spawn(async move {
                    if let Err(e) = m.on_background().await {
                        if let Some(mgr) = mgr.upgrade() {
                            mgr.log("module error", &format!("{}: in on_background(): {}", n, crate::detail_error(&e)));
                        }
                    }
                    if let Some(mgr) = mgr.upgrade() {
                        mgr.log("task", &format!("background task completed: {}", n));
                    }
                });
                self.log("core", &format!("Started background task {}", name));
            }
            self.register_module_tools(&module);
            self.modules.write().insert(name.clone(), module);
            self.log("core", &format!("loaded {}module : {}", is_user_str, name));
        }
    }

    /// main loop
    pub async fn run(self: &Arc<Self>) -> RunResult {
        let startup = Instant::now();
        self.prevent_double_shutdown.store(false, Ordering::SeqCst);
        if self.args.pure {
            self.pure_mode.store(true, Ordering::SeqCst);
        } else if self.args.coder {
            self.coding_mode.store(true, Ordering::SeqCst);
        }
        if !crate::globals::quiet() {
            self.log("core", "Starting OpenLumara");
        }
        *self.savedata.lock() = StorageDict::new("save", StorageType::Msgpack, None).ok();

        let mut enabled_channels = config::get_str_list(&["channels", "enabled"]);
        let mut enabled_user_channels = config::get_str_list(&["user_channels", "enabled"]);
        if self.args.cli {
            enabled_channels = vec!["cli".into()];
            enabled_user_channels = vec![];
        }
        if enabled_channels.is_empty() && enabled_user_channels.is_empty() {
            println!("ERROR: At least one channel must be enabled in the config! Try the `cli` channel for a basic terminal UI.");
            std::process::exit(1);
        }
        let mut enabled_modules = config::get_str_list(&["modules", "enabled"]);
        let mut enabled_user_modules = config::get_str_list(&["user_modules", "enabled"]);
        if self.pure_mode() {
            enabled_modules.clear();
            enabled_user_modules.clear();
        } else if self.coding_mode() {
            enabled_modules = vec!["coder".into()];
            enabled_user_modules.clear();
        }

        if !crate::globals::quiet() {
            self.log("core", "Loading core channels..");
        }
        self.load_channels("channels", &enabled_channels, false).await;

        if self.current_channel().is_none() {
            let last = self.savedata_get("last_channel").and_then(|v| v.as_str().map(|s| s.to_string()));
            let chosen = match last.and_then(|l| self.channel(&l)) {
                Some(c) => Some(c),
                None => {
                    let target = if self.channel("cli").is_some() { "cli".to_string() } else { enabled_channels.first().cloned().unwrap_or_default() };
                    self.savedata_set("last_channel", json!(target));
                    self.channel(&target)
                }
            };
            if let Some(c) = chosen {
                *self.current.write() = Some(c);
            }
        }

        if !enabled_user_channels.is_empty() {
            self.log("core", "Loading user channels..");
            self.load_channels("user_channels", &enabled_user_channels, true).await;
        }

        *GLOBAL_INSTANCE.lock() = self.self_ref.clone();

        self.log("core", "Loading modules..");
        if !enabled_modules.is_empty() {
            self.load_modules("modules", &enabled_modules, false).await;
        }
        if !enabled_user_modules.is_empty() {
            self.log("core", "Loading user modules..");
            self.load_modules("user_modules", &enabled_user_modules, true).await;
        }

        if !config::get_bool(&["model", "dynamic_tool_loading"], true) {
            self.log("core", "Dynamic tool loading is disabled. Loading all tools at startup.");
            for ch in self.channels().values() {
                ch.tool_loader.load_all_tools();
            }
        } else {
            for ch in self.channels().values() {
                ch.tool_loader.refresh_meta_tool_descriptions();
                ch.tool_loader.load_default_tools();
                ch.tool_loader.restore_chat_tools();
            }
        }

        self.log("core", &format!("Startup completed in {:.2}s", startup.elapsed().as_secs_f64()));
        self.log("", &"-".repeat(40));
        self.log("API", "Connecting..");
        if let Err(e) = self.api.connect().await {
            self.log("API", &e.to_string());
        }

        let channels = self.channels();
        let order: Vec<(String, Arc<Channel>)> = channels
            .iter()
            .filter(|(n, _)| n.as_str() != "cli")
            .chain(channels.iter().filter(|(n, _)| n.as_str() == "cli"))
            .map(|(n, c)| (n.clone(), c.clone()))
            .collect();
        for (name, ch) in order {
            self.log("core", &format!("Starting channel: {}", if name == "cli" { "CLI".to_string() } else { name.clone() }));
            if let Err(e) = ch.behavior.on_ready(&ch).await {
                self.log("core", &format!("Error in on_ready of channel {}: {}", name, crate::detail_error(&e)));
            }
            let c2 = ch.clone();
            let mgr = self.self_ref.clone();
            self.spawn(async move {
                if let Err(e) = c2.behavior.run(&c2).await {
                    if let Some(m) = mgr.upgrade() {
                        m.log("core", &format!("channel {} stopped with an error: {}", c2.name, crate::detail_error(&e)));
                    }
                }
            });
            ch.start_push_queue();
        }

        self.drain_log_buffers();
        self.started.store(true, Ordering::SeqCst);

        // wait until every task finished, or a shutdown/restart was requested, or ctrl-c
        let tasks: Vec<tokio::task::JoinHandle<()>> = std::mem::take(&mut *self.tasks.lock());
        let abort_handles: Vec<_> = tasks.iter().map(|t| t.abort_handle()).collect();
        let all = futures::future::join_all(tasks);
        tokio::select! {
            _ = all => {}
            _ = self.shutdown_notify.notified() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        self.shutdown().await;
        for h in abort_handles {
            h.abort();
        }
        for t in std::mem::take(&mut *self.tasks.lock()) {
            t.abort();
        }
        if self.restart_requested.load(Ordering::SeqCst) {
            RunResult::Restart
        } else {
            RunResult::Exit
        }
    }

    /// request a restart (the run loop shuts down and main() runs again)
    pub fn restart(&self) {
        self.log("core", "Restarting server..");
        self.restart_requested.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_one();
    }

    /// request a full shutdown
    pub fn request_shutdown(&self) {
        self.shutdown_notify.notify_one();
    }

    pub async fn shutdown(&self) -> bool {
        if self.prevent_double_shutdown.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.log("core", "Shutting down..");
        for (name, m) in self.modules() {
            if let Err(e) = m.on_shutdown().await {
                self.log_error(&format!("Error shutting down {}", name), &e.to_string());
            }
        }
        for (name, ch) in self.channels() {
            self.log("core", &format!("Shutting down channel {}", name));
            ch.shutdown_internal().await;
            if let Err(e) = ch.behavior.on_shutdown(&ch).await {
                self.log_error(&format!("Error shutting down {}", name), &e.to_string());
            }
        }
        *GLOBAL_INSTANCE.lock() = Weak::new();
        for t in std::mem::take(&mut *self.tasks.lock()) {
            t.abort();
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        self.started.store(false, Ordering::SeqCst);
        self.log("core", "Shutdown complete");
        true
    }

    fn toggle_in_sections(&self, sections: [&str; 2], name: &str) -> bool {
        let name = name.to_string();
        let mut toggled = false;
        let mut cfg = config::snapshot();
        for sec in sections {
            let Some(s) = cfg.get_mut(sec).and_then(|v| v.as_object_mut()) else { continue };
            let mut enabled: Vec<Value> = s.get("enabled").and_then(|v| v.as_array().cloned()).unwrap_or_default();
            let mut disabled: Vec<Value> = s.get("disabled").and_then(|v| v.as_array().cloned()).unwrap_or_default();
            let jv = json!(name);
            if let Some(pos) = enabled.iter().position(|v| *v == jv) {
                enabled.remove(pos);
                disabled.push(jv);
                toggled = true;
            } else if let Some(pos) = disabled.iter().position(|v| *v == jv) {
                disabled.remove(pos);
                enabled.push(jv);
                toggled = true;
            } else {
                continue;
            }
            s.insert("enabled".into(), Value::Array(enabled));
            s.insert("disabled".into(), Value::Array(disabled));
        }
        if toggled {
            config::replace(cfg);
        }
        toggled
    }

    pub fn toggle_module(&self, name: &str) -> bool {
        self.toggle_in_sections(["modules", "user_modules"], name)
    }

    pub fn toggle_channel(&self, name: &str) -> bool {
        self.toggle_in_sections(["channels", "user_channels"], name)
    }

    /// reload a module: unregister tools, on_shutdown, on_ready, re-register tools
    pub async fn reload_module(&self, module_name: &str) -> bool {
        let Some(module) = self.module(module_name) else {
            self.log("core", &format!("Module {} not loaded, cannot reload", module_name));
            return false;
        };
        self.log("core", &format!("Reloading module: {}", module_name));
        let prefix = format!("{}_", module_name);
        let was_active: IndexMap<String, bool> = self
            .channels()
            .iter()
            .map(|(n, c)| (n.clone(), c.tool_loader.active_names().iter().any(|t| t.starts_with(&prefix))))
            .collect();
        self.unregister_module_tools(module_name);
        if let Err(e) = module.on_shutdown().await {
            self.log("core", &format!("Error running on_shutdown for {}: {}", module_name, crate::detail_error(&e)));
        }
        if let Err(e) = module.on_ready().await {
            self.log("core", &format!("Error running on_ready for {}: {}", module_name, crate::detail_error(&e)));
            return false;
        }
        self.register_module_tools(&module);
        for (name, ch) in self.channels() {
            let persisted = ch.chat.get_loaded_modules().iter().any(|m| m == module_name);
            if was_active.get(&name).copied().unwrap_or(false) || persisted {
                ch.tool_loader.activate_module_tools(module_name);
            }
        }
        for ch in self.channels().values() {
            ch.tool_loader.load_default_tools();
        }
        true
    }

    /// settings of each loaded module (get_settings_structure)
    pub fn get_settings_structure(&self) -> IndexMap<String, Value> {
        self.modules()
            .keys()
            .map(|n| (n.clone(), crate::registry::module_settings(n)))
            .collect()
    }

    /// insert an already constructed module (tests / embedding); registers its tools
    pub fn insert_module(&self, module: Arc<dyn Module>) {
        self.register_module_tools(&module);
        self.modules.write().insert(module.name().to_string(), module);
    }

    /// insert an already constructed channel (tests / embedding)
    pub fn insert_channel(&self, ch: Arc<Channel>) {
        self.channels.write().insert(ch.name.clone(), ch);
    }

    pub fn register_module_tools(&self, module: &Arc<dyn Module>) {
        for ch in self.channels().values() {
            ch.tool_loader.register_module(module);
            ch.tool_loader.refresh_meta_tool_descriptions();
        }
    }

    pub fn unregister_module_tools(&self, module_name: &str) {
        for ch in self.channels().values() {
            ch.tool_loader.unregister_module(module_name);
            ch.tool_loader.refresh_meta_tool_descriptions();
        }
    }
}
