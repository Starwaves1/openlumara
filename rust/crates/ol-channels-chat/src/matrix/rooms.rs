//! room state tracking and /sync response parsing (what matrix-nio's MatrixRoom and
//! event callbacks provided)

use indexmap::IndexMap;
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct Member {
    pub membership: String,
    pub displayname: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Room {
    pub room_id: String,
    pub name: Option<String>,
    pub canonical_alias: Option<String>,
    pub encrypted: bool,
    pub members: IndexMap<String, Member>,
}

impl Room {
    fn apply_state(&mut self, ev: &Value) {
        let content = ev.get("content").cloned().unwrap_or(Value::Null);
        match ev.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "m.room.name" => self.name = content.get("name").and_then(|n| n.as_str()).filter(|n| !n.is_empty()).map(|s| s.to_string()),
            "m.room.canonical_alias" => self.canonical_alias = content.get("alias").and_then(|n| n.as_str()).filter(|n| !n.is_empty()).map(|s| s.to_string()),
            "m.room.encryption" => self.encrypted = true,
            "m.room.member" => {
                let Some(user) = ev.get("state_key").and_then(|s| s.as_str()) else { return };
                let membership = content.get("membership").and_then(|m| m.as_str()).unwrap_or("").to_string();
                if membership == "join" || membership == "invite" {
                    let displayname = content.get("displayname").and_then(|d| d.as_str()).map(|s| s.to_string());
                    self.members.insert(user.to_string(), Member { membership, displayname });
                } else {
                    self.members.shift_remove(user);
                }
            }
            _ => {}
        }
    }

    /// nio MatrixRoom.users: joined and invited members
    pub fn users(&self) -> usize {
        self.members.len()
    }

    /// nio MatrixRoom.display_name (simplified): name, alias, or computed from members
    pub fn display_name(&self, own_user: &str) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        if let Some(a) = &self.canonical_alias {
            return a.clone();
        }
        let mut names: Vec<String> = self
            .members
            .iter()
            .filter(|(u, _)| u.as_str() != own_user)
            .map(|(u, m)| m.displayname.clone().unwrap_or_else(|| u.clone()))
            .collect();
        names.sort();
        let others = names.len().saturating_sub(5);
        names.truncate(5);
        if others > 0 {
            format!("{}, and {} other{}", names.join(", "), others, if others == 1 { "" } else { "s" })
        } else if names.is_empty() {
            "Empty Room".into()
        } else if names.len() == 1 {
            names[0].clone()
        } else {
            format!("{} and {}", names[..names.len() - 1].join(", "), names[names.len() - 1])
        }
    }
}

/// events the channel reacts to, in the order they appeared in the sync
#[derive(Debug, Clone, PartialEq)]
pub enum RoomEvent {
    /// we were invited to a room
    Invite { room_id: String, sender: String },
    /// m.text / m.emote message
    Text { room_id: String, event: Value },
    /// m.image / m.audio / m.video / m.file message
    Media { room_id: String, event: Value },
    /// m.room.encrypted: can't be decrypted by the Rust port
    Encrypted { room_id: String, event: Value },
}

#[derive(Default)]
pub struct Rooms {
    pub joined: IndexMap<String, Room>,
}

impl Rooms {
    /// apply a /sync response: updates room state, returns the events to dispatch
    pub fn apply_sync(&mut self, sync: &Value, own_user: &str) -> Vec<RoomEvent> {
        let mut out = Vec::new();
        let rooms = sync.get("rooms").cloned().unwrap_or(Value::Null);

        // invites
        if let Some(invites) = rooms.get("invite").and_then(|i| i.as_object()) {
            for (room_id, data) in invites {
                let events = data.get("invite_state").and_then(|s| s.get("events")).and_then(|e| e.as_array()).cloned().unwrap_or_default();
                let invite = events.iter().find(|ev| {
                    ev.get("type").and_then(|t| t.as_str()) == Some("m.room.member")
                        && ev.get("state_key").and_then(|s| s.as_str()) == Some(own_user)
                        && ev.get("content").and_then(|c| c.get("membership")).and_then(|m| m.as_str()) == Some("invite")
                });
                if let Some(ev) = invite {
                    out.push(RoomEvent::Invite { room_id: room_id.clone(), sender: ev.get("sender").and_then(|s| s.as_str()).unwrap_or("").to_string() });
                }
            }
        }

        // joined rooms
        if let Some(joined) = rooms.get("join").and_then(|j| j.as_object()) {
            for (room_id, data) in joined {
                let room = self.joined.entry(room_id.clone()).or_insert_with(|| Room { room_id: room_id.clone(), ..Default::default() });
                for ev in data.get("state").and_then(|s| s.get("events")).and_then(|e| e.as_array()).cloned().unwrap_or_default() {
                    room.apply_state(&ev);
                }
                for ev in data.get("timeline").and_then(|s| s.get("events")).and_then(|e| e.as_array()).cloned().unwrap_or_default() {
                    if ev.get("state_key").is_some() {
                        room.apply_state(&ev);
                        continue;
                    }
                    match ev.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "m.room.message" => match ev.get("content").and_then(|c| c.get("msgtype")).and_then(|m| m.as_str()).unwrap_or("") {
                            "m.text" | "m.emote" => out.push(RoomEvent::Text { room_id: room_id.clone(), event: ev }),
                            "m.image" | "m.audio" | "m.video" | "m.file" => out.push(RoomEvent::Media { room_id: room_id.clone(), event: ev }),
                            _ => {}
                        },
                        "m.room.encrypted" => out.push(RoomEvent::Encrypted { room_id: room_id.clone(), event: ev }),
                        _ => {}
                    }
                }
            }
        }

        // left rooms
        if let Some(left) = rooms.get("leave").and_then(|j| j.as_object()) {
            for room_id in left.keys() {
                self.joined.shift_remove(room_id);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sync_fixture() -> Value {
        json!({
            "next_batch": "s1",
            "rooms": {
                "invite": {"!inv:x": {"invite_state": {"events": [
                    {"type": "m.room.member", "state_key": "@alice:x", "sender": "@alice:x", "content": {"membership": "join"}},
                    {"type": "m.room.member", "state_key": "@bot:x", "sender": "@alice:x", "content": {"membership": "invite"}}
                ]}}},
                "join": {"!r:x": {
                    "state": {"events": [
                        {"type": "m.room.member", "state_key": "@bot:x", "content": {"membership": "join", "displayname": "Bot"}},
                        {"type": "m.room.member", "state_key": "@alice:x", "content": {"membership": "join", "displayname": "Alice"}}
                    ]},
                    "timeline": {"events": [
                        {"type": "m.room.message", "event_id": "$1", "sender": "@alice:x", "origin_server_ts": 1, "content": {"msgtype": "m.text", "body": "hello"}},
                        {"type": "m.room.message", "event_id": "$2", "sender": "@alice:x", "content": {"msgtype": "m.notice", "body": "notice"}},
                        {"type": "m.room.message", "event_id": "$3", "sender": "@alice:x", "content": {"msgtype": "m.image", "body": "cat.png", "url": "mxc://x/y"}},
                        {"type": "m.room.encryption", "state_key": "", "content": {"algorithm": "m.megolm.v1.aes-sha2"}},
                        {"type": "m.room.encrypted", "event_id": "$4", "sender": "@alice:x", "content": {"session_id": "abcdefghijkl"}}
                    ]}
                }}
            }
        })
    }

    #[test]
    fn parse_sync() {
        let mut rooms = Rooms::default();
        let evs = rooms.apply_sync(&sync_fixture(), "@bot:x");
        assert_eq!(evs.len(), 4);
        assert_eq!(evs[0], RoomEvent::Invite { room_id: "!inv:x".into(), sender: "@alice:x".into() });
        assert!(matches!(&evs[1], RoomEvent::Text { event, .. } if event["event_id"] == "$1"));
        assert!(matches!(&evs[2], RoomEvent::Media { .. }));
        assert!(matches!(&evs[3], RoomEvent::Encrypted { .. }));
        let r = &rooms.joined["!r:x"];
        assert!(r.encrypted);
        assert_eq!(r.users(), 2);
        assert_eq!(r.display_name("@bot:x"), "Alice");

        let leave = json!({"rooms": {"leave": {"!r:x": {}}}});
        rooms.apply_sync(&leave, "@bot:x");
        assert!(rooms.joined.is_empty());
    }

    #[test]
    fn display_names() {
        let mut r = Room { room_id: "!r".into(), ..Default::default() };
        assert_eq!(r.display_name("@me"), "Empty Room");
        r.members.insert("@b".into(), Member { membership: "join".into(), displayname: Some("B".into()) });
        r.members.insert("@a".into(), Member { membership: "join".into(), displayname: None });
        assert_eq!(r.display_name("@me"), "@a and B");
        r.canonical_alias = Some("#room:x".into());
        assert_eq!(r.display_name("@me"), "#room:x");
        r.name = Some("Room".into());
        assert_eq!(r.display_name("@me"), "Room");
    }
}
