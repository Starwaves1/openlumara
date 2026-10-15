//! HMAC-signed session cookie (replacement for starlette's SessionMiddleware).
//!
//! Cookie value: `<base64url(json session)>.<unix timestamp>.<base64url(hmac-sha256)>`,
//! signed over `<payload>.<timestamp>`. Not wire-compatible with starlette/itsdangerous
//! (only the Rust server reads it), but the behaviour matches: the cookie is named
//! `session`, expires after `max_age` seconds, is re-issued on every response while the
//! session is non-empty and cleared once it becomes empty.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{Map, Value};
use sha2::Sha256;

pub const COOKIE_NAME: &str = "session";
pub const DEFAULT_SECRET: &str = "openlumara-default-session-secret-change-me";
const SECURITY_FLAGS: &str = "httponly; samesite=lax";

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct SessionSigner {
    secret: Vec<u8>,
    pub max_age: i64,
}

impl SessionSigner {
    pub fn new(secret: &str, max_age: i64) -> Self {
        Self { secret: secret.as_bytes().to_vec(), max_age }
    }

    fn mac(&self, msg: &str) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.secret).expect("hmac accepts any key length");
        mac.update(msg.as_bytes());
        mac
    }

    pub fn sign_at(&self, session: &Map<String, Value>, now: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(session).unwrap_or_default());
        let msg = format!("{}.{}", payload, now);
        let sig = URL_SAFE_NO_PAD.encode(self.mac(&msg).finalize().into_bytes());
        format!("{}.{}", msg, sig)
    }

    pub fn sign(&self, session: &Map<String, Value>) -> String {
        self.sign_at(session, now())
    }

    /// verify signature + age; None if invalid/expired
    pub fn unsign_at(&self, value: &str, now: i64) -> Option<Map<String, Value>> {
        let (msg, sig) = value.rsplit_once('.')?;
        let (payload, ts) = msg.rsplit_once('.')?;
        let sig = URL_SAFE_NO_PAD.decode(sig).ok()?;
        self.mac(msg).verify_slice(&sig).ok()?;
        let ts: i64 = ts.parse().ok()?;
        if now - ts > self.max_age {
            return None;
        }
        match serde_json::from_slice::<Value>(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()? {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn unsign(&self, value: &str) -> Option<Map<String, Value>> {
        self.unsign_at(value, now())
    }

    /// the Set-Cookie header value for a non-empty session
    pub fn set_cookie(&self, session: &Map<String, Value>) -> String {
        format!("{}={}; path=/; Max-Age={}; {}", COOKIE_NAME, self.sign(session), self.max_age, SECURITY_FLAGS)
    }

    /// the Set-Cookie header value that clears the session
    pub fn clear_cookie() -> String {
        format!("{}=null; path=/; expires=Thu, 01 Jan 1970 00:00:00 GMT; {}", COOKIE_NAME, SECURITY_FLAGS)
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// value of a cookie from a Cookie header
pub fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    for h in headers.get_all(axum::http::header::COOKIE) {
        let Ok(s) = h.to_str() else { continue };
        for part in s.split(';') {
            if let Some((k, v)) = part.trim().split_once('=') {
                if k.trim() == name {
                    let v = v.trim();
                    let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v);
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// timing-safe string comparison (python secrets.compare_digest)
pub fn compare_digest(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        // still do a comparison of equal work so the timing doesn't leak much
        let _ = a.as_bytes().ct_eq(a.as_bytes());
        return false;
    }
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sign_round_trip() {
        let s = SessionSigner::new("secret", 86400);
        let sess = json!({"authenticated": true}).as_object().unwrap().clone();
        let v = s.sign_at(&sess, 1000);
        assert_eq!(s.unsign_at(&v, 1000 + 86400).unwrap(), sess);
        // expired
        assert!(s.unsign_at(&v, 1000 + 86401).is_none());
        // wrong secret
        assert!(SessionSigner::new("other", 86400).unsign_at(&v, 1000).is_none());
        // tampered payload
        let forged_payload = URL_SAFE_NO_PAD.encode(br#"{"authenticated":true,"x":1}"#);
        let parts: Vec<&str> = v.split('.').collect();
        let forged = format!("{}.{}.{}", forged_payload, parts[1], parts[2]);
        assert!(s.unsign_at(&forged, 1000).is_none());
        // tampered timestamp
        let forged = format!("{}.{}.{}", parts[0], 999999, parts[2]);
        assert!(s.unsign_at(&forged, 1000).is_none());
        assert!(s.unsign_at("garbage", 1000).is_none());
        assert!(s.set_cookie(&sess).starts_with("session="));
        assert!(s.set_cookie(&sess).ends_with("; path=/; Max-Age=86400; httponly; samesite=lax"));
    }

    #[test]
    fn digest_and_cookies() {
        assert!(compare_digest("admin", "admin"));
        assert!(!compare_digest("admin", "admin2"));
        assert!(!compare_digest("admin", "admiN"));
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::COOKIE, "a=1; session=abc.def; b=2".parse().unwrap());
        assert_eq!(cookie_value(&h, "session").as_deref(), Some("abc.def"));
        assert_eq!(cookie_value(&h, "nope"), None);
    }
}
