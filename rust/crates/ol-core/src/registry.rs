//! Compiled-in module/channel registry.
//!
//! Python discovers modules/channels at runtime by scanning `modules/`, `channels/`,
//! `user_modules/` and `user_channels/` and importing `module.py`/`channel.py`.
//! Rust has no sane equivalent of importing arbitrary source at runtime, so every
//! module/channel is compiled in and registered here by the binary at startup
//! (see `openlumara` crate `main.rs`). The registry carries the same class-level
//! metadata Python reads off the classes: docstring, settings schema, `unsafe`,
//! `category`.
//!
//! Folders on disk that are not in the registry (unported modules, Python user
//! modules) are still *discovered* so their config entries are preserved and they
//! show up in the settings UI, but they can't be loaded.

use crate::channel::ChannelBehavior;
use crate::module::{Module, ModuleBase};
use indexmap::IndexMap;
use once_cell::sync::OnceCell;
use serde_json::Value;
use std::sync::Arc;

pub type ModuleFactory = fn(ModuleBase) -> Arc<dyn Module>;
pub type ChannelFactory = fn() -> Arc<dyn ChannelBehavior>;

#[derive(Clone)]
pub struct ModuleDescriptor {
    /// folder name, e.g. "calendar" (the canonical module name)
    pub name: &'static str,
    /// python class name, used in python-style error messages ("Calendar.add_event() missing ...")
    pub class_name: &'static str,
    /// class docstring (inspect.getdoc - dedented)
    pub doc: &'static str,
    /// the class-level `settings` dict (schema with defaults/descriptions)
    pub settings: fn() -> Value,
    pub unsafe_: bool,
    pub category: &'static str,
    pub factory: ModuleFactory,
}

#[derive(Clone)]
pub struct ChannelDescriptor {
    pub name: &'static str,
    pub class_name: &'static str,
    pub doc: &'static str,
    pub settings: fn() -> Value,
    pub unsafe_: bool,
    pub category: &'static str,
    pub factory: ChannelFactory,
}

#[derive(Default)]
pub struct Registry {
    pub modules: IndexMap<String, ModuleDescriptor>,
    pub channels: IndexMap<String, ChannelDescriptor>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add_module(&mut self, d: ModuleDescriptor) -> &mut Self {
        self.modules.insert(d.name.to_string(), d);
        self
    }
    pub fn add_channel(&mut self, d: ChannelDescriptor) -> &mut Self {
        self.channels.insert(d.name.to_string(), d);
        self
    }
}

static REGISTRY: OnceCell<Registry> = OnceCell::new();

/// install the registry (once, at startup, before config::load)
pub fn install(r: Registry) {
    let _ = REGISTRY.set(r);
}

pub fn get() -> &'static Registry {
    REGISTRY.get_or_init(Registry::default)
}

pub fn module(name: &str) -> Option<&'static ModuleDescriptor> {
    get().modules.get(name)
}

pub fn channel(name: &str) -> Option<&'static ChannelDescriptor> {
    get().channels.get(name)
}

/// effective `settings` of a module: the class settings plus the auto-inserted
/// `enable_webui_extension` setting when the module folder ships a `webui/` folder
/// (core/modules.py _ensure_webui_extension_setting)
pub fn module_settings(name: &str) -> Value {
    let Some(d) = module(name) else { return Value::Object(Default::default()) };
    let mut settings = match (d.settings)() {
        Value::Object(m) => m,
        _ => Default::default(),
    };
    let webui_dir = std::path::Path::new(crate::paths::project_root()).join("modules").join(name).join("webui");
    if webui_dir.is_dir() && !settings.contains_key("enable_webui_extension") {
        let mut new = serde_json::Map::new();
        new.insert(
            "enable_webui_extension".into(),
            serde_json::json!({
                "default": true,
                "description": "Whether to load this module's WebUI Extensions, which extend the WebUI with module-specific features, sidebars, panels, etc"
            }),
        );
        new.extend(settings);
        settings = new;
    }
    Value::Object(settings)
}

pub fn channel_settings(name: &str) -> Value {
    channel(name).map(|d| (d.settings)()).unwrap_or_else(|| Value::Object(Default::default()))
}

/// Python `inspect.getdoc`-style cleanup of a docstring (dedent, strip)
pub fn clean_doc(doc: &str) -> String {
    let lines: Vec<&str> = doc.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let indent = lines
        .iter()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out: Vec<String> = vec![lines[0].trim().to_string()];
    for l in lines.iter().skip(1) {
        out.push(if l.len() >= indent { l[indent..].trim_end().to_string() } else { l.trim().to_string() });
    }
    while out.first().map(|l| l.is_empty()).unwrap_or(false) {
        out.remove(0);
    }
    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}
