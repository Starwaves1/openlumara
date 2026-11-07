//! minimal Discord gateway client (identify / heartbeat / resume / reconnect),
//! replacing discord.py's DiscordWebSocket. Dispatch events are forwarded over a channel.

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub const DEFAULT_GATEWAY_URL: &str = "wss://gateway.discord.gg";

/// gateway intent bits (discord.py `Intents` flag values)
pub mod intents {
    pub const GUILDS: u64 = 1 << 0;
    pub const MEMBERS: u64 = 1 << 1;
    pub const MODERATION: u64 = 1 << 2;
    pub const EMOJIS_AND_STICKERS: u64 = 1 << 3;
    pub const INTEGRATIONS: u64 = 1 << 4;
    pub const WEBHOOKS: u64 = 1 << 5;
    pub const INVITES: u64 = 1 << 6;
    pub const VOICE_STATES: u64 = 1 << 7;
    pub const PRESENCES: u64 = 1 << 8;
    pub const GUILD_MESSAGES: u64 = 1 << 9;
    pub const GUILD_REACTIONS: u64 = 1 << 10;
    pub const GUILD_TYPING: u64 = 1 << 11;
    pub const DM_MESSAGES: u64 = 1 << 12;
    pub const DM_REACTIONS: u64 = 1 << 13;
    pub const DM_TYPING: u64 = 1 << 14;
    pub const MESSAGE_CONTENT: u64 = 1 << 15;
    pub const GUILD_SCHEDULED_EVENTS: u64 = 1 << 16;
    pub const AUTO_MODERATION_CONFIGURATION: u64 = 1 << 20;
    pub const AUTO_MODERATION_EXECUTION: u64 = 1 << 21;
    pub const GUILD_POLLS: u64 = 1 << 24;
    pub const DM_POLLS: u64 = 1 << 25;

    /// discord.Intents.default(): everything except the privileged intents
    pub const fn default_intents() -> u64 {
        GUILDS
            | MODERATION
            | EMOJIS_AND_STICKERS
            | INTEGRATIONS
            | WEBHOOKS
            | INVITES
            | VOICE_STATES
            | GUILD_MESSAGES
            | GUILD_REACTIONS
            | GUILD_TYPING
            | DM_MESSAGES
            | DM_REACTIONS
            | DM_TYPING
            | GUILD_SCHEDULED_EVENTS
            | AUTO_MODERATION_CONFIGURATION
            | AUTO_MODERATION_EXECUTION
            | GUILD_POLLS
            | DM_POLLS
    }
}

/// dispatch events forwarded to the channel
#[derive(Debug, Clone)]
pub enum GatewayEvent {
    Ready(Value),
    MessageCreate(Value),
}

pub const PRIVILEGED_INTENTS_ERROR: &str = "Shard ID None is requesting privileged intents that have not been explicitly enabled in the developer portal. It is recommended to go to https://discord.com/developers/applications/ and explicitly enable the privileged intents within your application's page. If this is not possible, then consider disabling the privileged intents instead.";

/// what to do after a websocket close code: Err(fatal message) or Ok(resumable)
pub fn classify_close(code: u16) -> Result<bool, String> {
    match code {
        4004 => Err("Improper token has been passed.".into()),
        4010 => Err("Invalid shard".into()),
        4011 => Err("Sharding required".into()),
        4012 => Err("Invalid API version".into()),
        4013 => Err("Invalid intents".into()),
        4014 => Err(PRIVILEGED_INTENTS_ERROR.into()),
        // session no longer valid: start a fresh one
        1000 | 4007 | 4009 => Ok(false),
        _ => Ok(true),
    }
}

pub fn identify_payload(token: &str, intents: u64) -> Value {
    json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": intents,
            "properties": {"os": std::env::consts::OS, "browser": "openlumara", "device": "openlumara"},
        }
    })
}

pub fn resume_payload(token: &str, session_id: &str, seq: Option<i64>) -> Value {
    json!({"op": 6, "d": {"token": token, "session_id": session_id, "seq": seq}})
}

pub fn gateway_url(base: &str) -> String {
    format!("{}/?v=10&encoding=json", base.trim_end_matches('/'))
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    resume_url: Option<String>,
    seq: Option<i64>,
}

enum Outcome {
    Stop,
    Reconnect { resume: bool },
    Fatal(String),
}

/// run the gateway until `stop` is notified (Ok) or a fatal error happens (Err)
pub async fn run(base_url: String, token: String, intents: u64, tx: tokio::sync::mpsc::UnboundedSender<GatewayEvent>, stop: Arc<tokio::sync::Notify>) -> Result<(), String> {
    let mut session = Session::default();
    let mut backoff = 1u64;
    loop {
        let base = session.resume_url.clone().filter(|_| session.id.is_some()).unwrap_or_else(|| base_url.clone());
        let connect = tokio::select! {
            r = tokio_tungstenite::connect_async(gateway_url(&base)) => r,
            _ = stop.notified() => return Ok(()),
        };
        let ws = match connect {
            Ok((ws, _)) => ws,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        match connection(ws, &token, intents, &mut session, &tx, &stop).await {
            Outcome::Stop => return Ok(()),
            Outcome::Fatal(e) => return Err(e),
            Outcome::Reconnect { resume } => {
                if !resume {
                    session = Session::default();
                } else {
                    backoff = 1;
                }
                tokio::time::sleep(Duration::from_millis(if resume { 500 } else { 2000 })).await;
            }
        }
    }
}

async fn connection<S>(ws: tokio_tungstenite::WebSocketStream<S>, token: &str, intents: u64, session: &mut Session, tx: &tokio::sync::mpsc::UnboundedSender<GatewayEvent>, stop: &tokio::sync::Notify) -> Outcome
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut sink, mut stream) = ws.split();

    // Hello
    let hello = match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
        Ok(Some(Ok(WsMessage::Text(t)))) => serde_json::from_str::<Value>(&t).unwrap_or(Value::Null),
        _ => return Outcome::Reconnect { resume: true },
    };
    if hello.get("op").and_then(|o| o.as_i64()) != Some(10) {
        return Outcome::Reconnect { resume: true };
    }
    let interval = Duration::from_millis(hello["d"]["heartbeat_interval"].as_u64().unwrap_or(41250));

    let first = match &session.id {
        Some(id) => resume_payload(token, id, session.seq),
        None => identify_payload(token, intents),
    };
    if sink.send(WsMessage::Text(first.to_string().into())).await.is_err() {
        return Outcome::Reconnect { resume: true };
    }

    // first heartbeat after interval * jitter
    let jitter = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) % 1000) as f64 / 1000.0;
    let mut next_hb = Instant::now() + interval.mul_f64(jitter);
    let mut acked = true;

    loop {
        tokio::select! {
            _ = stop.notified() => {
                let _ = sink.send(WsMessage::Close(None)).await;
                return Outcome::Stop;
            }
            _ = tokio::time::sleep_until(next_hb) => {
                if !acked {
                    // zombied connection
                    return Outcome::Reconnect { resume: true };
                }
                if sink.send(WsMessage::Text(json!({"op": 1, "d": session.seq}).to_string().into())).await.is_err() {
                    return Outcome::Reconnect { resume: true };
                }
                acked = false;
                next_hb = Instant::now() + interval;
            }
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    _ => return Outcome::Reconnect { resume: true },
                };
                let payload: Value = match msg {
                    WsMessage::Text(t) => serde_json::from_str(&t).unwrap_or(Value::Null),
                    WsMessage::Binary(b) => serde_json::from_slice(&b).unwrap_or(Value::Null),
                    WsMessage::Close(frame) => {
                        let code = frame.map(|f| u16::from(f.code)).unwrap_or(1006);
                        return match classify_close(code) {
                            Ok(resume) => Outcome::Reconnect { resume },
                            Err(e) => Outcome::Fatal(e),
                        };
                    }
                    _ => continue,
                };
                if let Some(s) = payload.get("s").and_then(|s| s.as_i64()) {
                    session.seq = Some(s);
                }
                match payload.get("op").and_then(|o| o.as_i64()).unwrap_or(-1) {
                    0 => {
                        let d = payload.get("d").cloned().unwrap_or(Value::Null);
                        match payload.get("t").and_then(|t| t.as_str()).unwrap_or("") {
                            "READY" => {
                                session.id = d.get("session_id").and_then(|s| s.as_str()).map(|s| s.to_string());
                                session.resume_url = d.get("resume_gateway_url").and_then(|s| s.as_str()).map(|s| s.to_string());
                                let _ = tx.send(GatewayEvent::Ready(d));
                            }
                            "MESSAGE_CREATE" => {
                                let _ = tx.send(GatewayEvent::MessageCreate(d));
                            }
                            _ => {}
                        }
                    }
                    1 => {
                        let _ = sink.send(WsMessage::Text(json!({"op": 1, "d": session.seq}).to_string().into())).await;
                    }
                    7 => return Outcome::Reconnect { resume: true },
                    9 => {
                        let resumable = payload.get("d").and_then(|d| d.as_bool()).unwrap_or(false);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        return Outcome::Reconnect { resume: resumable };
                    }
                    11 => acked = true,
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_intents_value() {
        // discord.Intents.default() with message_content enabled
        let v = intents::default_intents() | intents::MESSAGE_CONTENT;
        assert_eq!(v & intents::MEMBERS, 0);
        assert_eq!(v & intents::PRESENCES, 0);
        assert_ne!(v & intents::MESSAGE_CONTENT, 0);
        assert_eq!(v, 53608189);
    }

    #[test]
    fn close_codes() {
        assert_eq!(classify_close(4004), Err("Improper token has been passed.".to_string()));
        assert_eq!(classify_close(4000), Ok(true));
        assert_eq!(classify_close(4009), Ok(false));
        assert!(classify_close(4014).is_err());
    }

    #[test]
    fn payloads() {
        let p = identify_payload("tok", 513);
        assert_eq!(p["op"], 2);
        assert_eq!(p["d"]["intents"], 513);
        let r = resume_payload("tok", "sess", Some(5));
        assert_eq!(r["d"]["seq"], 5);
        assert_eq!(gateway_url("wss://gateway.discord.gg/"), "wss://gateway.discord.gg/?v=10&encoding=json");
    }
}
