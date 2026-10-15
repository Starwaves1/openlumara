//! Port of channels/ntfy: forwards pushed messages to an ntfy server as push
//! notifications (`POST {server}/{topic}` with `Title` and `Priority` headers).

#[path = "../../../generated/channels/ntfy.rs"]
mod meta;

use ol_core::channel::{Channel, ChannelBehavior};
use ol_core::pyjson::py_str;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

crate::descriptor_from_meta!("ntfy", meta::settings, factory);

fn factory() -> Arc<dyn ChannelBehavior> {
    Arc::new(Ntfy)
}

pub struct Ntfy;

/// `{server.rstrip('/')}/{topic}`
pub fn ntfy_url(server: &str, topic: &str) -> String {
    format!("{}/{}", server.trim_end_matches('/'), topic)
}

#[async_trait::async_trait]
impl ChannelBehavior for Ntfy {
    async fn on_push(&self, ch: &Arc<Channel>, message: Value) -> anyhow::Result<()> {
        let content = match message.get("content") {
            Some(c) if ol_core::pyjson::truthy(c) => py_str(c),
            _ => return Ok(()),
        };
        let url = ntfy_url(&ch.config.get_str("server").unwrap_or_default(), &ch.config.get1("topic").map(|v| py_str(&v)).unwrap_or_default());
        let title = ch.config.get1("title").map(|v| py_str(&v)).unwrap_or_default();

        let client = reqwest::Client::new();
        let mut req = client.post(&url).body(content).timeout(Duration::from_secs(5)).header("Priority", "default");
        // non-ascii titles are sent as raw utf-8 bytes
        if let Ok(v) = reqwest::header::HeaderValue::from_bytes(title.as_bytes()) {
            req = req.header("Title", v);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if matches!(status, 200 | 201 | 204) {
                    ch.log(&ch.name, "Sent push message to NTFY");
                } else {
                    ch.log(&ch.name, &format!("Failed to send NTFY notification. Status: {}", status));
                }
            }
            Err(e) => ch.log(&ch.name, &format!("Error forwarding to NTFY: {}", ol_core::detail_error(&e))),
        }
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
    fn url() {
        assert_eq!(ntfy_url("http://localhost:3050/", "openlumara"), "http://localhost:3050/openlumara");
        assert_eq!(ntfy_url("https://ntfy.sh//", "abc"), "https://ntfy.sh/abc");
    }
}
