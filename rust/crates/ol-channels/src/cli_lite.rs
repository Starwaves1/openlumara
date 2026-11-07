//! Port of channels/cli_lite: plain `input("> ")` loop using non-streaming send().

#[path = "../../../generated/channels/cli_lite.rs"]
mod meta;

use crate::util::print_flush;
use ol_core::channel::{Channel, ChannelBehavior, SendArgs};
use ol_core::pyjson::py_str;
use serde_json::{json, Value};
use std::sync::Arc;

crate::descriptor_from_meta!("cli_lite", meta::settings, factory);

fn factory() -> Arc<dyn ChannelBehavior> {
    Arc::new(CliLite)
}

pub struct CliLite;

/// python `input(prompt)`: prints the prompt, reads one line (without the newline).
/// EOF raises python's EOFError, which ends the channel's run loop.
async fn input(prompt: &str) -> anyhow::Result<String> {
    print_flush(prompt);
    let line = tokio::task::spawn_blocking(|| {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).map(|n| (n, s))
    })
    .await??;
    match line {
        (0, _) => anyhow::bail!("EOF when reading a line"),
        (_, s) => Ok(s.strip_suffix('\n').map(|x| x.strip_suffix('\r').unwrap_or(x)).unwrap_or(&s).to_string()),
    }
}

/// `[CATEGORY] ` prefix (hidden when the category is empty)
pub fn format_log(category: &str, message: &str) -> String {
    if category.is_empty() {
        message.to_string()
    } else {
        format!("[{}] {}", category.to_uppercase(), message)
    }
}

#[async_trait::async_trait]
impl ChannelBehavior for CliLite {
    async fn run(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        loop {
            let user_input = input("> ").await?;
            let response = ch.send(json!(user_input), SendArgs::authorized()).await;
            // python would crash with AttributeError on a None response (blank
            // command results); the port prints nothing instead
            if let Some(r) = response {
                print_flush(&format!("{}\n", r.get("content").map(py_str).unwrap_or_else(|| "None".into())));
            }
        }
    }

    fn on_log(&self, _ch: &Arc<Channel>, category: &str, message: &str) {
        if ol_core::quiet() {
            return;
        }
        print_flush(&format!("{}\n", format_log(category, message)));
    }

    async fn on_push(&self, _ch: &Arc<Channel>, message: Value) -> anyhow::Result<()> {
        print_flush(&format!("\n{}\n", message.get("content").map(py_str).unwrap_or_else(|| "None".into())));
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
    fn log_format() {
        assert_eq!(format_log("core", "hi"), "[CORE] hi");
        assert_eq!(format_log("", "----"), "----");
    }
}
