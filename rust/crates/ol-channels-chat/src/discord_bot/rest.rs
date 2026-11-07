//! minimal Discord REST client (the parts of discord.py's HTTPClient the channel uses)

use serde_json::{json, Value};
use std::time::Duration;

pub const DEFAULT_API_BASE: &str = "https://discord.com/api/v10";

/// errors formatted like discord.py's exceptions
#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    pub reason: String,
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 0 {
            return write!(f, "{}", self.message);
        }
        // discord.py HTTPException: "{status} {reason} (error code: {code})[: {text}]"
        write!(f, "{} {} (error code: {})", self.status, self.reason, self.code)?;
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpError {}

impl HttpError {
    fn transport(e: impl std::fmt::Display) -> Self {
        Self { status: 0, reason: String::new(), code: 0, message: e.to_string() }
    }
}

/// a sent/received message we may edit later (discord.py `Message`)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Msg {
    pub channel_id: String,
    pub id: String,
    pub content: String,
}

impl Msg {
    pub fn from_json(v: &Value) -> Self {
        Msg {
            channel_id: v.get("channel_id").and_then(|c| c.as_str()).unwrap_or("").to_string(),
            id: v.get("id").and_then(|c| c.as_str()).unwrap_or("").to_string(),
            content: v.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string(),
        }
    }
}

/// request body for creating a message (optionally as a reply)
pub fn create_message_body(content: &str, reply_to: Option<&str>) -> Value {
    let mut body = json!({"content": content});
    if let Some(mid) = reply_to {
        body["message_reference"] = json!({"message_id": mid, "fail_if_not_exists": false});
        body["allowed_mentions"] = json!({"parse": ["users", "roles", "everyone"], "replied_user": true});
    }
    body
}

#[derive(Clone)]
pub struct Rest {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Rest {
    pub fn new(base: &str, token: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .user_agent("DiscordBot (https://github.com/Rose22/openlumara, 1.0) OpenLumara-Rust")
            .build()
            .unwrap_or_default();
        Self { http, base: base.trim_end_matches('/').to_string(), token: token.to_string() }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// perform a request, retrying on rate limits (429)
    pub async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, HttpError> {
        for _ in 0..5 {
            let mut req = self.http.request(method.clone(), self.url(path)).header("Authorization", format!("Bot {}", self.token));
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(HttpError::transport)?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if status.as_u16() == 429 {
                let wait = data.get("retry_after").and_then(|v| v.as_f64()).unwrap_or(1.0);
                tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.05, 60.0))).await;
                continue;
            }
            if status.is_success() {
                return Ok(data);
            }
            return Err(HttpError {
                status: status.as_u16(),
                reason: status.canonical_reason().unwrap_or("").to_string(),
                code: data.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                message: data.get("message").and_then(|m| m.as_str()).map(|s| s.to_string()).unwrap_or(text),
            });
        }
        Err(HttpError { status: 429, reason: "Too Many Requests".into(), code: 0, message: "rate limited".into() })
    }

    pub async fn get_me(&self) -> Result<Value, HttpError> {
        self.request(reqwest::Method::GET, "/users/@me", None).await
    }

    pub async fn get_channel(&self, channel_id: &str) -> Result<Value, HttpError> {
        self.request(reqwest::Method::GET, &format!("/channels/{}", channel_id), None).await
    }

    pub async fn get_message(&self, channel_id: &str, message_id: &str) -> Result<Value, HttpError> {
        self.request(reqwest::Method::GET, &format!("/channels/{}/messages/{}", channel_id, message_id), None).await
    }

    pub async fn send_message(&self, channel_id: &str, content: &str, reply_to: Option<&str>) -> Result<Msg, HttpError> {
        let v = self.request(reqwest::Method::POST, &format!("/channels/{}/messages", channel_id), Some(create_message_body(content, reply_to))).await?;
        Ok(Msg::from_json(&v))
    }

    pub async fn edit_message(&self, msg: &Msg, content: &str) -> Result<Msg, HttpError> {
        let v = self
            .request(reqwest::Method::PATCH, &format!("/channels/{}/messages/{}", msg.channel_id, msg.id), Some(json!({"content": content})))
            .await?;
        let mut m = Msg::from_json(&v);
        if m.id.is_empty() {
            m = Msg { content: content.to_string(), ..msg.clone() };
        }
        Ok(m)
    }

    pub async fn delete_message(&self, msg: &Msg) -> Result<(), HttpError> {
        self.request(reqwest::Method::DELETE, &format!("/channels/{}/messages/{}", msg.channel_id, msg.id), None).await.map(|_| ())
    }

    pub async fn trigger_typing(&self, channel_id: &str) -> Result<(), HttpError> {
        self.request(reqwest::Method::POST, &format!("/channels/{}/typing", channel_id), None).await.map(|_| ())
    }

    /// download an attachment (attachment.read())
    pub async fn download(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        let resp = self.http.get(url).send().await.map_err(HttpError::transport)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(HttpError { status: status.as_u16(), reason: status.canonical_reason().unwrap_or("").into(), code: 0, message: String::new() });
        }
        resp.bytes().await.map(|b| b.to_vec()).map_err(HttpError::transport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies() {
        assert_eq!(create_message_body("hi", None), json!({"content": "hi"}));
        let b = create_message_body("hi", Some("42"));
        assert_eq!(b["message_reference"]["message_id"], "42");
    }

    #[test]
    fn error_format() {
        let e = HttpError { status: 404, reason: "Not Found".into(), code: 10003, message: "Unknown Channel".into() };
        assert_eq!(e.to_string(), "404 Not Found (error code: 10003): Unknown Channel");
    }
}
