//! thin Matrix client-server API client (the parts of matrix-nio's AsyncClient used by
//! the channel). No end-to-end encryption.

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// an error response, formatted like matrix-nio's ErrorResponse (`LoginError: M_FORBIDDEN Invalid password`)
#[derive(Debug, Clone, PartialEq)]
pub struct MxError {
    pub kind: &'static str,
    /// http status, 0 for transport errors (nio raises those as exceptions)
    pub status: u16,
    pub errcode: String,
    pub message: String,
}

impl MxError {
    fn transport(kind: &'static str, e: impl std::fmt::Display) -> Self {
        Self { kind, status: 0, errcode: String::new(), message: e.to_string() }
    }
}

impl std::fmt::Display for MxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let e = match (self.errcode.is_empty(), self.message.is_empty()) {
            (false, false) => format!("{} {}", self.errcode, self.message),
            (true, false) => self.message.clone(),
            (false, true) => format!("{} unknown error", self.errcode),
            (true, true) => "unknown error".into(),
        };
        write!(f, "{}: {}", self.kind, e)
    }
}

impl std::error::Error for MxError {}

pub fn enc(segment: &str) -> String {
    utf8_percent_encode(segment, NON_ALPHANUMERIC).to_string()
}

/// login request body (nio AsyncClient.login with a password)
pub fn login_body(user: &str, password: &str, device_name: &str, device_id: Option<&str>) -> Value {
    let mut body = json!({
        "type": "m.login.password",
        "identifier": {"type": "m.id.user", "user": user},
        "password": password,
        "initial_device_display_name": device_name,
    });
    if let Some(d) = device_id.filter(|d| !d.is_empty()) {
        body["device_id"] = json!(d);
    }
    body
}

/// content of a plain text message
pub fn text_content(text: &str) -> Value {
    json!({"msgtype": "m.text", "body": text})
}

/// content of an edit (m.replace) of `event_id`
pub fn edit_content(event_id: &str, new_text: &str) -> Value {
    json!({
        "msgtype": "m.text",
        "body": format!("* {}", new_text),
        "m.new_content": {"msgtype": "m.text", "body": new_text},
        "m.relates_to": {"rel_type": "m.replace", "event_id": event_id},
    })
}

pub struct MxClient {
    http: reqwest::Client,
    pub homeserver: String,
    pub user_id: String,
    pub device_id: String,
    pub access_token: String,
    txn: AtomicU64,
}

impl MxClient {
    pub fn new(homeserver: &str, user_id: &str, device_id: &str) -> Self {
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).build().unwrap_or_default();
        Self { http, homeserver: homeserver.trim_end_matches('/').to_string(), user_id: user_id.into(), device_id: device_id.into(), access_token: String::new(), txn: AtomicU64::new(0) }
    }

    pub fn restore_login(&mut self, user_id: &str, device_id: &str, access_token: &str) {
        self.user_id = user_id.into();
        self.device_id = device_id.into();
        self.access_token = access_token.into();
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}/_matrix/client/v3{}", self.homeserver, path)
    }

    async fn request(&self, kind: &'static str, method: reqwest::Method, path: &str, query: &[(&str, String)], body: Option<Value>, timeout: Duration) -> Result<Value, MxError> {
        let mut req = self.http.request(method, self.url(path)).timeout(timeout);
        if !query.is_empty() {
            req = req.query(query);
        }
        if !self.access_token.is_empty() {
            req = req.bearer_auth(&self.access_token);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.map_err(|e| MxError::transport(kind, e))?;
        let status = resp.status();
        let data: Value = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(data);
        }
        Err(MxError {
            kind,
            status: status.as_u16(),
            errcode: data.get("errcode").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            message: data.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(|| status.to_string()),
        })
    }

    /// password login; on success the client is logged in
    pub async fn login(&mut self, password: &str, device_name: &str) -> Result<(String, String, String), MxError> {
        let body = login_body(&self.user_id, password, device_name, Some(&self.device_id));
        let r = self.request("LoginError", reqwest::Method::POST, "/login", &[], Some(body), Duration::from_secs(30)).await?;
        let user_id = r.get("user_id").and_then(|v| v.as_str()).unwrap_or(&self.user_id).to_string();
        let device_id = r.get("device_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let token = r.get("access_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
        self.restore_login(&user_id, &device_id, &token);
        Ok((user_id, device_id, token))
    }

    pub async fn sync(&self, since: Option<&str>, timeout_ms: u64, full_state: bool) -> Result<Value, MxError> {
        let mut q: Vec<(&str, String)> = vec![("timeout", timeout_ms.to_string())];
        if let Some(s) = since {
            q.push(("since", s.to_string()));
        }
        if full_state {
            q.push(("full_state", "true".into()));
        }
        self.request("SyncError", reqwest::Method::GET, "/sync", &q, None, Duration::from_millis(timeout_ms) + Duration::from_secs(30)).await
    }

    pub async fn join(&self, room_id: &str) -> Result<Value, MxError> {
        self.request("JoinError", reqwest::Method::POST, &format!("/join/{}", enc(room_id)), &[], Some(json!({})), Duration::from_secs(30)).await
    }

    pub async fn room_typing(&self, room_id: &str, typing: bool, timeout_ms: u64) -> Result<Value, MxError> {
        let body = if typing { json!({"typing": true, "timeout": timeout_ms}) } else { json!({"typing": false}) };
        self.request("RoomTypingError", reqwest::Method::PUT, &format!("/rooms/{}/typing/{}", enc(room_id), enc(&self.user_id)), &[], Some(body), Duration::from_secs(30)).await
    }

    fn next_txn_id(&self) -> String {
        let n = self.txn.fetch_add(1, Ordering::SeqCst);
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        format!("ol{}.{}", ms, n)
    }

    /// send a room event; returns the event id
    pub async fn room_send(&self, room_id: &str, event_type: &str, content: Value) -> Result<String, MxError> {
        let path = format!("/rooms/{}/send/{}/{}", enc(room_id), enc(event_type), enc(&self.next_txn_id()));
        let r = self.request("RoomSendError", reqwest::Method::PUT, &path, &[], Some(content), Duration::from_secs(30)).await?;
        Ok(r.get("event_id").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_format() {
        let e = MxError { kind: "LoginError", status: 403, errcode: "M_FORBIDDEN".into(), message: "Invalid username or password".into() };
        assert_eq!(e.to_string(), "LoginError: M_FORBIDDEN Invalid username or password");
    }

    #[test]
    fn contents() {
        let e = edit_content("$ev", "new");
        assert_eq!(e["body"], "* new");
        assert_eq!(e["m.new_content"]["body"], "new");
        assert_eq!(e["m.relates_to"]["rel_type"], "m.replace");
        assert_eq!(text_content("hi"), json!({"msgtype": "m.text", "body": "hi"}));
        let b = login_body("@bot:x", "pw", "OpenLumara", Some(""));
        assert!(b.get("device_id").is_none());
        assert_eq!(b["identifier"]["user"], "@bot:x");
    }

    #[test]
    fn urls() {
        let c = MxClient::new("https://matrix.org/", "@bot:matrix.org", "");
        assert_eq!(c.url("/sync"), "https://matrix.org/_matrix/client/v3/sync");
        assert_eq!(enc("!abc:matrix.org"), "%21abc%3Amatrix%2Eorg");
    }
}
