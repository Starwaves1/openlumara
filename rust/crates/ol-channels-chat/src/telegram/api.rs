//! thin Telegram Bot API client (replaces python-telegram-bot's `Bot`)

use serde_json::{json, Value};
use std::time::Duration;

pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// errors shaped like python-telegram-bot's exceptions (their str() is what gets logged)
#[derive(Debug, Clone, PartialEq)]
pub enum TgError {
    /// 401/404: the token was rejected
    InvalidToken,
    /// 400
    BadRequest(String),
    /// 403
    Forbidden(String),
    /// 409
    Conflict(String),
    /// 429
    RetryAfter(u64),
    /// transport problems
    Network(String),
    /// anything else
    Other(String),
}

impl std::fmt::Display for TgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TgError::InvalidToken => write!(f, "Invalid token"),
            TgError::BadRequest(m) | TgError::Forbidden(m) | TgError::Conflict(m) | TgError::Network(m) | TgError::Other(m) => write!(f, "{}", m),
            TgError::RetryAfter(s) => write!(f, "Flood control exceeded. Retry in {} seconds", s),
        }
    }
}

impl std::error::Error for TgError {}

/// python-telegram-bot strips these prefixes from error descriptions
fn clean_description(d: &str) -> String {
    let mut s = d;
    for p in ["Error: ", "[Error]: ", "Bad Request: "] {
        if let Some(rest) = s.strip_prefix(p) {
            s = rest;
        }
    }
    s.to_string()
}

/// map a Bot API response (http status + body) to a result
pub fn parse_response(status: u16, body: &Value) -> Result<Value, TgError> {
    if body.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(body.get("result").cloned().unwrap_or(Value::Null));
    }
    let desc = clean_description(body.get("description").and_then(|v| v.as_str()).unwrap_or("Unknown HTTPError"));
    if let Some(ra) = body.get("parameters").and_then(|p| p.get("retry_after")).and_then(|v| v.as_u64()) {
        return Err(TgError::RetryAfter(ra));
    }
    Err(match status {
        401 | 404 => TgError::InvalidToken,
        400 => TgError::BadRequest(desc),
        403 => TgError::Forbidden(desc),
        409 => TgError::Conflict(desc),
        502 => TgError::Network("Bad Gateway".into()),
        _ => TgError::Other(format!("{} ({})", desc, status)),
    })
}

#[derive(Clone)]
pub struct TgApi {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl TgApi {
    pub fn new(base: &str, token: &str) -> Self {
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build().unwrap_or_default();
        Self { http, base: base.trim_end_matches('/').to_string(), token: token.to_string() }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.base, self.token, method)
    }

    /// POST a Bot API method with JSON parameters
    pub async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, TgError> {
        let resp = self
            .http
            .post(self.method_url(method))
            .json(&params)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| TgError::Network(if e.is_timeout() { "Timed out".into() } else { e.to_string() }))?;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or_else(|_| json!({"ok": false, "description": "Invalid server response"}));
        parse_response(status, &body)
    }

    async fn call_default(&self, method: &str, params: Value) -> Result<Value, TgError> {
        self.call(method, params, Duration::from_secs(30)).await
    }

    pub async fn get_me(&self) -> Result<Value, TgError> {
        self.call_default("getMe", json!({})).await
    }

    pub async fn delete_webhook(&self, drop_pending_updates: bool) -> Result<Value, TgError> {
        self.call_default("deleteWebhook", json!({"drop_pending_updates": drop_pending_updates})).await
    }

    pub async fn get_updates(&self, offset: Option<i64>, timeout_secs: u64) -> Result<Vec<Value>, TgError> {
        let mut params = json!({"timeout": timeout_secs});
        if let Some(o) = offset {
            params["offset"] = json!(o);
        }
        let r = self.call("getUpdates", params, Duration::from_secs(timeout_secs + 15)).await?;
        Ok(r.as_array().cloned().unwrap_or_default())
    }

    /// sendMessage. returns the sent message's id
    pub async fn send_message(&self, chat_id: i64, text: &str, parse_mode: Option<&str>, reply_to: Option<i64>) -> Result<i64, TgError> {
        let mut params = json!({"chat_id": chat_id, "text": text});
        if let Some(pm) = parse_mode {
            params["parse_mode"] = json!(pm);
        }
        if let Some(r) = reply_to {
            params["reply_parameters"] = json!({"message_id": r});
        }
        let r = self.call_default("sendMessage", params).await?;
        Ok(r.get("message_id").and_then(|v| v.as_i64()).unwrap_or(0))
    }

    pub async fn edit_message_text(&self, chat_id: i64, message_id: i64, text: &str) -> Result<Value, TgError> {
        self.call_default("editMessageText", json!({"chat_id": chat_id, "message_id": message_id, "text": text})).await
    }

    pub async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<Value, TgError> {
        self.call_default("sendChatAction", json!({"chat_id": chat_id, "action": action})).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses() {
        assert_eq!(parse_response(200, &json!({"ok": true, "result": {"id": 1}})), Ok(json!({"id": 1})));
        assert_eq!(parse_response(401, &json!({"ok": false, "description": "Unauthorized"})), Err(TgError::InvalidToken));
        let e = parse_response(400, &json!({"ok": false, "description": "Bad Request: message is not modified"})).unwrap_err();
        assert_eq!(e.to_string(), "message is not modified");
        let e = parse_response(429, &json!({"ok": false, "description": "Too Many Requests", "parameters": {"retry_after": 5}})).unwrap_err();
        assert_eq!(e, TgError::RetryAfter(5));
    }

    #[test]
    fn urls() {
        let a = TgApi::new("https://api.telegram.org/", "123:abc");
        assert_eq!(a.method_url("getMe"), "https://api.telegram.org/bot123:abc/getMe");
    }
}
