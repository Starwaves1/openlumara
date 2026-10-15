//! OpenLumara core, ported from the Python `core/` package.
//!
//! Module map (Python -> Rust):
//! - core/__init__.py, core/functions.py -> [`globals`], [`paths`]
//! - core/exceptions.py -> [`errors`]
//! - core/config.py -> [`config`]
//! - core/storage.py -> [`storage`]
//! - core/search.py -> [`search`]
//! - core/api.py -> [`api`]
//! - core/chat.py, core/messages.py -> [`chat`]
//! - core/context.py -> [`context`]
//! - core/turns.py -> [`turns`]
//! - core/toolcalls.py -> [`toolcalls`]
//! - core/tool_loader.py -> [`tool_loader`]
//! - core/commands.py -> [`commands`]
//! - core/module.py, core/modules.py -> [`module`], [`registry`]
//! - core/channel.py -> [`channel`]
//! - core/manager.py -> [`manager`]
//! - core/module_manager.py -> [`module_manager`]

pub mod api;
pub mod channel;
pub mod chat;
pub mod commands;
pub mod config;
pub mod context;
pub mod errors;
pub mod globals;
pub mod lenient_json;
pub mod manager;
pub mod module;
pub mod module_manager;
pub mod paths;
pub mod pyjson;
pub mod registry;
pub mod search;
pub mod storage;
pub mod tool_loader;
pub mod toolcalls;
pub mod turns;

pub use globals::{debug, detail_error, log, quiet};
pub use paths::{get_data_path, get_path, sandbox_path};

/// Convenience re-exports for module/channel implementations.
pub mod prelude {
    pub use crate::api::{ApiClient, ApiError};
    pub use crate::channel::{Channel, ChannelBehavior, TokenStream};
    pub use crate::config::{self, ConfigView};
    pub use crate::globals::{detail_error, log};
    pub use crate::manager::Manager;
    pub use crate::module::{
        tool_result, CommandHelp, CommandSpec, Module, ModuleBase, ParamType, ToolArgs, ToolParam,
        ToolSpec, UiRoute, UserMessageHook,
    };
    pub use crate::paths::{get_data_path, get_path, sandbox_path};
    pub use crate::pyjson::{self, py_repr, py_str, truthy};
    pub use crate::storage::{StorageDict, StorageList, StorageText, StorageType};
    pub use async_trait::async_trait;
    pub use serde_json::{json, Map, Value};
    pub use std::sync::Arc;
}
