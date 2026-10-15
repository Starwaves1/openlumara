//! Port of modules/channel/module.py

use crate::util;
use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase};
use ol_core::pyjson::py_str;
use serde_json::Value;

#[path = "../../../generated/modules/channel.rs"]
pub(crate) mod meta;

/// `Channel.instructions` (verbatim python strings; `.strip()`ped when used)
pub const INSTRUCTIONS: &[(&str, &str)] = &[
    ("discord", r#"
            Type `/help` for help. Type `/restart` to restart openlumara. Discord bot can tell who is talking to it if the `enable group chat` setting is turned on, and can show reasoning/thinking if the `show reasoning` setting is turned on.
        "#),
    ("cli", r#"
        Type `/help` for help. CLI channel uses python's prompt_toolkit library, and supports common readline keyboard shortcuts. User can use arrow up/down to navigate typing history.
        "#),
    ("webui", r#"
        Desktop & Mobile:
        - Input bar (at bottom):
            - Press send button or press enter to send message to AI
            - Press upload button to upload a file for the AI to read
        - Top of chat window:
            - Press gear icon to open settings
            - Press icon with arrow down to export chat history
            - Press trashcan icon to clear chat
        - Chat list (hidden behind swipe gesture on mobile, always visible on the left on desktop)
            - Type into search box to search chats within current category
            - Press page icon to search within full content instead of just name
            - Press tag icon to filter by tags
        - Ask AI to rename, tag, or categorize chat (if `chats` module enabled) to auto-sort a chat

        Desktop exclusive:
        - Top of chat window:
            - Click folder icon at top of chat window to open storage editor which lets user view and edit all of openlumara's data files
            - Click keyboard icon or press Ctrl+/ for list of keyboard shortcuts
            - Press Ctrl+Space for Global Search (searches across all chats)
        - Sidebar:
            - Can be hidden using Ctrl+B
            - Split into two columns:
                - Right side (chat list)
                - Left side (category list)
                    - Click category to switch to it
                    - Ask AI to sort chat into a new category to create a new category
                    - Drag and drop a chat from the chat list onto a category to sort it
            - Border between chat list and category list can be clicked to show/hide category list
        - Input bar (at bottom):
            - Press Ctrl+Up and Ctrl+Down to navigate input history
            - You can paste media like photos and screenshots into the input field

        Mobile exclusive:
        - Swipe from left to open menu that contains all previous chats. Tap a chat to switch to it. Tap menu's header to see chat category list.
        "#),
];

pub struct ChannelModule {
    base: ModuleBase,
}

impl ChannelModule {
    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }
}

/// builds the prompt for channel `chan`
pub fn build_prompt(chan: &str, available_chans: &[String], enable_tutorial_prompts: bool) -> String {
    let mut output: Vec<String> = Vec::new();
    let chan_display = match chan {
        "cli" => "Command Line Interface (CLI)",
        "webui" => "WebUI",
        "discord" => "Discord",
        "telegram" => "Telegram",
        "matrix" => "Matrix",
        other => other,
    };
    output.push(format!("Current channel: {}", chan_display));
    if chan == "cli" || chan == "matrix" {
        output.push("**DO NOT USE MARKDOWN IN YOUR MESSAGES**.".into());
    }
    output.push(format!("Available channels: {}", available_chans.join(", ")));
    if enable_tutorial_prompts {
        if let Some((_, text)) = INSTRUCTIONS.iter().find(|(k, _)| *k == chan) {
            output.push(String::new());
            output.push("Instructions for the user:".into());
            output.push(text.trim().to_string());
        }
    }
    output.join("\n")
}

#[async_trait]
impl Module for ChannelModule {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        let Some(ch) = self.base.channel() else { return Ok(None) };
        if util::chat_has_character(&ch)? {
            return Ok(None);
        }
        let available: Vec<String> = match ol_core::config::get(&["channels", "enabled"]) {
            Some(Value::Array(a)) => a.iter().map(py_str).collect(),
            Some(Value::String(s)) => s.chars().map(|c| c.to_string()).collect(),
            None => Vec::new(),
            Some(_) => anyhow::bail!("can only join an iterable"),
        };
        Ok(Some(build_prompt(&ch.name, &available, self.base.config.get_bool("enable_tutorial_prompts"))))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt() {
        let p = build_prompt("cli", &["cli".into(), "webui".into()], true);
        assert!(p.starts_with("Current channel: Command Line Interface (CLI)\n**DO NOT USE MARKDOWN IN YOUR MESSAGES**.\nAvailable channels: cli, webui\n\nInstructions for the user:\nType `/help` for help. CLI channel"));
        let p = build_prompt("webui", &["webui".into()], true);
        assert!(p.contains("Instructions for the user:\nDesktop & Mobile:\n        - Input bar (at bottom):\n"), "{}", p);
        assert_eq!(build_prompt("ntfy", &[], true), "Current channel: ntfy\nAvailable channels: ");
    }
}
