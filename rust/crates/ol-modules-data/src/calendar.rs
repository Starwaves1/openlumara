//! Port of modules/calendar/module.py: calendar events with notifications.
//!
//! Python schedules notifications with `loop.call_later`; those timers hold a reference
//! to the event dict, so later edits (or a fired notification clearing `notify`) are
//! seen by pending timers. The Rust port spawns a tokio task per scheduled
//! notification that re-reads the event from storage by id when it fires (falling
//! back to the captured copy if it was deleted, like python's dangling dict).
//! Pending timers are aborted on shutdown/reload.

#[path = "../../../generated/modules/calendar.rs"]
pub mod meta;

use crate::util::{body_get, body_str, fromisoformat, fromisoformat_value, iso, midnight, now, py_datetime, query_int, total_seconds};
use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{Duration, NaiveDateTime};
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::{py_str, py_str_opt, truthy};
use ol_core::storage::{StorageList, StorageType};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Weak};

/// the class-level settings dict (computed: channel options depend on available channels)
pub fn settings() -> Value {
    json!({
        "insert_system_prompt": {
            "description": "Whether to add the calendar events within your configured range (defined below) to the system prompt. This will make your AI aware of your upcoming appointments at all times!",
            "default": true
        },
        "range": {
            "description": "The range of days relative to today that you want the AI to see the events of",
            "default": 7,
            "depends": "insert_system_prompt"
        },
        "include_past_events": {
            "description": "Whether or not to make the AI aware of calendar events that have already passed. Many local AI's don't handle this very well, so it's recommended to leave this off, or they will start to remind you of appointments that have already happened. It's still included for posterity, in case you're using a model that handles this better.",
            "default": false,
            "depends": "insert_system_prompt"
        },
        "notifications": {
            "description": "Whether to receive notifications about upcoming events",
            "default": true
        },
        "notification_channel": {
            "type": "select",
            "default": "webui",
            "description": "Which channel to send calendar notifications to",
            "options": crate::scheduler::channel_options(),
            "depends": "notifications"
        },
        "notification_window": {
            "description": "Amount of minutes in advance you should be notified. You can set this to 0 to be notified at the time of the event.",
            "default": 30,
            "depends": "notifications"
        }
    })
}

/// minutes-left phrase used in notifications
pub fn notify_window_str(minutes_left: i64) -> String {
    if minutes_left <= 0 {
        "now!".into()
    } else if minutes_left == 1 {
        "in 1 minute".into()
    } else {
        format!("in {} minutes", minutes_left)
    }
}

/// events whose date lies within [start, end]; unparsable dates are skipped
pub fn events_between(events: &[Value], start: &NaiveDateTime, end: &NaiveDateTime) -> Vec<Value> {
    events
        .iter()
        .filter(|e| match fromisoformat_value(e.get("date")) {
            Ok(d) => *start <= d && d <= *end,
            Err(_) => false,
        })
        .cloned()
        .collect()
}

/// (past_boundary, future_boundary) of the configured range relative to `today`
pub fn configured_range(today: &NaiveDateTime, range_days: i64, include_past: bool) -> (NaiveDateTime, NaiveDateTime) {
    let future = *today + Duration::days(range_days);
    let past = if include_past { *today - Duration::days(range_days) } else { *today };
    (past, future)
}

fn event_line(e: &Value) -> String {
    format!("{}: on {}: {}", py_str_opt(e.get("id")), py_str_opt(e.get("date")), py_str_opt(e.get("title")))
}

struct Inner {
    base: ModuleBase,
    events: Mutex<Option<StorageList>>,
    timers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    me: Weak<Inner>,
}

pub struct Calendar {
    inner: Arc<Inner>,
}

impl Calendar {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { inner: Arc::new_cyclic(|me| Inner { base, events: Mutex::new(None), timers: Mutex::new(Vec::new()), me: me.clone() }) }
    }

    /// snapshot of all events
    pub fn events(&self) -> Vec<Value> {
        self.inner.events()
    }
}

impl Inner {
    fn with<R>(&self, f: impl FnOnce(&mut StorageList) -> R) -> anyhow::Result<R> {
        let mut g = self.events.lock();
        let s = g.as_mut().ok_or_else(|| anyhow!("'Calendar' object has no attribute 'events'"))?;
        Ok(f(s))
    }

    fn events(&self) -> Vec<Value> {
        self.with(|s| s.data.clone()).unwrap_or_default()
    }

    fn get_event_by_id(data: &[Value], id: &str) -> i64 {
        for (i, e) in data.iter().enumerate() {
            if py_str_opt(e.get("id")).trim() == id.trim() {
                return i as i64;
            }
        }
        -1
    }

    fn index_of(&self, id: &str) -> i64 {
        self.with(|s| Self::get_event_by_id(&s.data, id)).unwrap_or(-1)
    }

    /// the live (stored) version of an event, or the given copy if it was deleted
    fn live(&self, event: &Value) -> Value {
        let id = py_str_opt(event.get("id"));
        self.with(|s| {
            let i = Self::get_event_by_id(&s.data, &id);
            if i >= 0 {
                s.data[i as usize].clone()
            } else {
                event.clone()
            }
        })
        .unwrap_or_else(|_| event.clone())
    }

    /// schedules a notification for an event
    async fn schedule_notification(&self, event: Value) -> anyhow::Result<()> {
        let event_time = fromisoformat_value(event.get("date")).map_err(|e| anyhow!(e))?;
        let window = match event.get("notification_window") {
            Some(v) => v.clone(),
            None => self.base.config.get1("notification_window").unwrap_or(Value::Null),
        };
        let window_minutes = crate::util::py_int(&window).ok_or_else(|| anyhow!("int() argument must be a string, a bytes-like object or a real number, not '{}'", crate::util::py_type(&window)))?;
        let delay = total_seconds(&event_time, &now()) - (window_minutes * 60) as f64;
        if delay <= 0.0 {
            // happening right now or already passed: trigger immediately
            return self.notify_user(&event).await;
        }
        let me = self.me.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs_f64(delay)).await;
            let Some(inner) = me.upgrade() else { return };
            let live = inner.live(&event);
            if let Err(e) = inner.notify_user(&live).await {
                inner.base.log("calendar", &format!("failed to notify: {}", ol_core::detail_error(&e)));
            }
        });
        let mut timers = self.timers.lock();
        timers.retain(|t| !t.is_finished());
        timers.push(handle);
        Ok(())
    }

    async fn notify_user(&self, event: &Value) -> anyhow::Result<()> {
        if !event.get("notify").map(truthy).unwrap_or(false) {
            return Ok(());
        }
        let channel_name = match event.get("notify_channel").filter(|v| truthy(v)) {
            Some(c) => Some(c.clone()),
            None => self.base.config.get1("notification_channel"),
        };
        let manager = self.base.manager().ok_or_else(|| anyhow!("manager is gone"))?;
        let channel = channel_name.as_ref().and_then(|n| n.as_str()).and_then(|n| manager.channel(n));
        let Some(channel) = channel else { return Ok(()) };

        let event_time = fromisoformat_value(event.get("date")).map_err(|e| anyhow!(e))?;
        let diff = total_seconds(&event_time, &now());
        let minutes_left = (diff / 60.0).trunc() as i64;
        let message = format!("🔔 **Calendar**: {} is starting {}", py_str_opt(event.get("title")), notify_window_str(minutes_left));
        channel.push(json!(message), true).await;
        // add to context so the AI knows it just notified the user
        channel.chat.messages_add(&json!({"role": "assistant", "content": message}), false, false).await;

        // disable notification
        let id = py_str_opt(event.get("id"));
        self.with(|s| {
            let i = Self::get_event_by_id(&s.data, &id);
            if i != -1 {
                s.data[i as usize]["notify"] = json!(false);
                s.save();
            }
        })?;
        Ok(())
    }

    fn range_settings(&self) -> anyhow::Result<(i64, bool)> {
        let range = match self.base.config.get1("range") {
            Some(v) if !v.is_null() => crate::util::py_int(&v).ok_or_else(|| anyhow!("invalid literal for int() with base 10: {}", ol_core::pyjson::py_repr(&v)))?,
            _ => 7,
        };
        Ok((range, self.base.config.get_bool("include_past_events")))
    }

    fn events_in_range(&self) -> anyhow::Result<Vec<Value>> {
        let (range, include_past) = self.range_settings()?;
        let (past, future) = configured_range(&now(), range, include_past);
        let mut out = Vec::new();
        for e in self.events() {
            // python doesn't catch parse errors here
            let d = fromisoformat_value(e.get("date")).map_err(|err| anyhow!(err))?;
            if past <= d && d <= future {
                out.push(e);
            }
        }
        Ok(out)
    }

    async fn add_event(&self, args: &ToolArgs) -> anyhow::Result<Value> {
        let g = |k: &str| args.i64(k).ok_or_else(|| anyhow!("'{}' argument must be an integer", k));
        let date = py_datetime(g("year")?, g("month")?, g("day")?, g("hour")?, g("minute")?).map_err(|e| anyhow!(e))?;
        let should_notify = args.0.get("should_notify").cloned().unwrap_or(json!(true));
        let notify_channel = match args.raw("notify_channel").filter(|v| truthy(v)) {
            Some(c) => c.clone(),
            None => self.base.config.get1("notification_channel").unwrap_or(Value::Null),
        };
        let event = json!({
            "id": ol_core::globals::new_ulid(),
            "title": args.0.get("title").cloned().unwrap_or(Value::Null),
            "date": iso(&date),
            "notify": should_notify,
            "notify_channel": notify_channel,
        });
        self.with(|s| {
            s.data.push(event.clone());
            s.save();
        })?;
        if truthy(&should_notify) {
            self.schedule_notification(event.clone()).await?;
        }
        Ok(self.base.result(format!("appointment added with ID {}", py_str(&event["id"])), true))
    }

    async fn edit_event(&self, args: &ToolArgs) -> anyhow::Result<Value> {
        let id = args.str_or("id", "");
        let index = self.index_of(&id);
        if index < 0 {
            return Ok(self.base.result("Error: Event with that ID does not exist", false));
        }
        let event = self.with(|s| s.data[index as usize].clone())?;
        let ed = fromisoformat_value(event.get("date")).map_err(|e| anyhow!(e))?;
        use chrono::{Datelike, Timelike};
        // python uses `x or default`, so 0 falls back to the existing value
        let pick = |k: &str, d: i64| args.i64(k).filter(|v| *v != 0).unwrap_or(d);
        let new_date = py_datetime(
            pick("year", ed.year() as i64),
            pick("month", ed.month() as i64),
            pick("day", ed.day() as i64),
            pick("hour", ed.hour() as i64),
            pick("minute", ed.minute() as i64),
        )
        .map_err(|e| anyhow!(e))?;
        let or = |k: &str, existing: &str| -> Value {
            match args.raw(k).filter(|v| truthy(v)) {
                Some(v) => v.clone(),
                None => event.get(existing).cloned().unwrap_or(Value::Null),
            }
        };
        let title = or("title", "title");
        let notify = or("should_notify", "notify");
        let notify_channel = or("notify_channel", "notify_channel");
        let updated = self.with(|s| {
            let e = &mut s.data[index as usize];
            e["title"] = title;
            e["date"] = json!(iso(&new_date));
            e["notify"] = notify;
            e["notify_channel"] = notify_channel;
            let out = e.clone();
            s.save();
            out
        })?;
        if args.raw("should_notify").map(truthy).unwrap_or(false) {
            self.schedule_notification(updated.clone()).await?;
        }
        Ok(self.base.result(format!("event {} edited", py_str_opt(event.get("id"))), true))
    }

    async fn delete_event(&self, id: &str) -> anyhow::Result<Value> {
        let removed = self.with(|s| {
            let i = Self::get_event_by_id(&s.data, id);
            if i < 0 {
                return false;
            }
            s.data.remove(i as usize);
            s.save();
            true
        })?;
        if !removed {
            return Ok(self.base.result("Error: Event with that ID does not exist", false));
        }
        Ok(self.base.result(format!("event {} deleted", id), true))
    }

    fn cmd_calendar(&self, args: &[String]) -> anyhow::Result<String> {
        let today = now();
        let (past, future) = if args.is_empty() {
            let (range, include_past) = self.range_settings()?;
            configured_range(&today, range, include_past)
        } else {
            let parsed = (|| {
                let past = fromisoformat(&args[0])?;
                let future = if args.len() > 1 { fromisoformat(&args[1])? } else { past + Duration::days(1) };
                Ok::<_, String>((past, future))
            })();
            match parsed {
                Ok(p) => p,
                Err(_) => return Ok("Error: Invalid date format. Use ISO format (e.g., 2024-01-15).".into()),
            }
        };
        let matches = events_between(&self.events(), &past, &future);
        if matches.is_empty() {
            return Ok("No events found in this range.".into());
        }
        let lines: Vec<String> = matches
            .iter()
            .map(|e| {
                let d = fromisoformat_value(e.get("date")).unwrap();
                format!("{}: {}", d.format("%x at %X"), py_str_opt(e.get("title")))
            })
            .collect();
        Ok(lines.join("\n"))
    }

    fn read_calendar_range(&self, start: &str, end: &str) -> Value {
        let (Ok(past), Ok(future)) = (fromisoformat(start), fromisoformat(end)) else {
            return self.base.result("Error: Invalid date format. Use ISO format (e.g., 2024-01-15).", false);
        };
        let matches = events_between(&self.events(), &past, &future);
        if matches.is_empty() {
            return self.base.result("No events found in this range.", false);
        }
        let lines: Vec<String> = matches.iter().map(event_line).collect();
        self.base.result(lines.join("\n"), true)
    }

    // ---------------- webui routes ----------------

    fn route_upcoming(&self, query: &Map<String, Value>) -> anyhow::Result<Value> {
        self.with(|s| s.load())?;
        let days = query_int(query, "days", 14).clamp(1, 365);
        let n = now();
        let start = midnight(&n);
        let end = n + Duration::days(days);
        let mut items: Vec<Value> = events_between(&self.events(), &start, &end)
            .iter()
            .map(|e| {
                json!({
                    "id": e.get("id").cloned().unwrap_or(Value::Null),
                    "title": e.get("title").cloned().unwrap_or(json!("")),
                    "date": e.get("date").cloned().unwrap_or(json!("")),
                    "notify": e.get("notify").map(truthy).unwrap_or(false),
                    "notify_channel": e.get("notify_channel").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        items.sort_by_key(|e| py_str(&e["date"]));
        Ok(Value::Array(items))
    }

    /// parses 'YYYY-MM-DD' + 'HH:MM' (python strptime "%Y-%m-%d %H:%M")
    fn ui_parse_dt(date: &str, time: &str) -> Option<NaiveDateTime> {
        NaiveDateTime::parse_from_str(&format!("{} {}", date.trim(), time.trim()), "%Y-%m-%d %H:%M").ok()
    }

    async fn route_create(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let title = body_str(body, "title", "").trim().to_string();
        if title.is_empty() {
            return Ok(self.base.result("event title cannot be empty", false));
        }
        let Some(dt) = Self::ui_parse_dt(&body_str(body, "date", ""), &body_str(body, "time", "00:00")) else {
            return Ok(self.base.result("invalid date/time. expected YYYY-MM-DD and HH:MM", false));
        };
        use chrono::{Datelike, Timelike};
        let mut args = Map::new();
        args.insert("title".into(), json!(title));
        args.insert("year".into(), json!(dt.year()));
        args.insert("month".into(), json!(dt.month()));
        args.insert("day".into(), json!(dt.day()));
        args.insert("hour".into(), json!(dt.hour()));
        args.insert("minute".into(), json!(dt.minute()));
        args.insert("should_notify".into(), json!(body_get(body, "notify").map(|v| truthy(&v)).unwrap_or(true)));
        args.insert("notify_channel".into(), body_get(body, "notify_channel").filter(truthy).unwrap_or(Value::Null));
        self.add_event(&ToolArgs::new(args)).await
    }

    async fn route_update(&self, body: &Option<Value>) -> anyhow::Result<Value> {
        let Some(event_id) = body_get(body, "id").filter(truthy) else {
            return Ok(self.base.result("no event id given", false));
        };
        self.with(|s| s.load())?;
        let index = self.index_of(&py_str(&event_id));
        if index < 0 {
            return Ok(self.base.result("event with that ID does not exist", false));
        }
        let mut event = self.with(|s| s.data[index as usize].clone())?;
        let title = body_str(body, "title", "").trim().to_string();
        if !title.is_empty() {
            event["title"] = json!(title);
        }
        if body_get(body, "date").map(|v| truthy(&v)).unwrap_or(false) {
            let Some(dt) = Self::ui_parse_dt(&body_str(body, "date", ""), &body_str(body, "time", "00:00")) else {
                return Ok(self.base.result("invalid date/time. expected YYYY-MM-DD and HH:MM", false));
            };
            event["date"] = json!(iso(&dt));
        }
        if let Some(n) = body_get(body, "notify") {
            event["notify"] = json!(truthy(&n));
        }
        if let Some(c) = body_get(body, "notify_channel").filter(truthy) {
            event["notify_channel"] = c;
        }
        let stored = event.clone();
        self.with(|s| {
            if let Some(slot) = s.data.get_mut(index as usize) {
                *slot = stored;
            }
            s.save();
        })?;
        if event.get("notify").map(truthy).unwrap_or(false) {
            self.schedule_notification(event.clone()).await?;
        }
        Ok(self.base.result(format!("event {} edited", py_str_opt(event.get("id"))), true))
    }
}

#[async_trait]
impl Module for Calendar {
    fn base(&self) -> &ModuleBase {
        &self.inner.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "add_event" => self.inner.add_event(&args).await,
            "edit_event" => self.inner.edit_event(&args).await,
            "delete_event" => self.inner.delete_event(&args.str_or("id", "")).await,
            "read_calendar_range" => Ok(self.inner.read_calendar_range(&args.str_or("start_date", ""), &args.str_or("end_date", ""))),
            _ => anyhow::bail!("'Calendar' object has no attribute '{}'", method),
        }
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        *self.inner.events.lock() = Some(StorageList::new("calendar", StorageType::Json, None)?);
        // schedule all event notifications (python: one asyncio task per event)
        for event in self.inner.events() {
            if event.get("notify").map(truthy).unwrap_or(false) {
                let inner = self.inner.clone();
                let h = tokio::spawn(async move {
                    if let Err(e) = inner.schedule_notification(event).await {
                        inner.base.log("calendar", &format!("failed to schedule notification: {}", ol_core::detail_error(&e)));
                    }
                });
                self.inner.timers.lock().push(h);
            }
        }
        Ok(())
    }

    async fn on_shutdown(&self) -> anyhow::Result<()> {
        let timers: Vec<_> = std::mem::take(&mut *self.inner.timers.lock());
        for t in timers {
            t.abort();
        }
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        let matches = self.inner.events_in_range()?;
        if matches.is_empty() {
            return Ok(None);
        }
        Ok(Some(matches.iter().map(event_line).collect::<Vec<_>>().join("\n")))
    }

    fn commands(&self) -> Vec<CommandSpec> {
        meta::commands().into_iter().map(|(c, _)| c).collect()
    }

    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        match name {
            "calendar" => Ok(Some(self.inner.cmd_calendar(&args)?)),
            _ => anyhow::bail!("unknown command {}", name),
        }
    }

    fn ui_routes(&self) -> Vec<UiRoute> {
        meta::ui_routes().into_iter().map(|(r, _)| r).collect()
    }

    async fn ui_route(&self, path: &str, method: &str, body: Option<Value>, query: Map<String, Value>) -> anyhow::Result<Value> {
        match (path, method) {
            ("upcoming", "GET") => self.inner.route_upcoming(&query),
            ("channels", "GET") => Ok(json!(crate::scheduler::available_channels())),
            ("create", "POST") => self.inner.route_create(&body).await,
            ("update", "POST") => self.inner.route_update(&body).await,
            ("delete", "POST") => {
                let Some(id) = body_get(&body, "id").filter(truthy) else {
                    return Ok(self.inner.base.result("no event id given", false));
                };
                self.inner.with(|s| s.load())?;
                self.inner.delete_event(&py_str(&id)).await
            }
            _ => anyhow::bail!("route not found: {}", path),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        fromisoformat(s).unwrap()
    }

    #[test]
    fn ranges() {
        let today = dt("2026-10-14T12:00:00");
        let (p, f) = configured_range(&today, 7, false);
        assert_eq!(p, today);
        assert_eq!(f, dt("2026-10-21T12:00:00"));
        let (p, _) = configured_range(&today, 7, true);
        assert_eq!(p, dt("2026-10-07T12:00:00"));
        let events = vec![
            json!({"id": "a", "date": "2026-10-14T13:00:00", "title": "A"}),
            json!({"id": "b", "date": "2026-10-13T13:00:00", "title": "B"}),
            json!({"id": "c", "date": "garbage", "title": "C"}),
            json!({"id": "d", "date": "2026-10-21T12:00:00", "title": "D"}),
        ];
        let m = events_between(&events, &today, &dt("2026-10-21T12:00:00"));
        assert_eq!(m.iter().map(|e| e["id"].as_str().unwrap()).collect::<Vec<_>>(), vec!["a", "d"]);
        assert_eq!(event_line(&events[0]), "a: on 2026-10-14T13:00:00: A");
    }

    #[test]
    fn window_strings() {
        assert_eq!(notify_window_str(-3), "now!");
        assert_eq!(notify_window_str(1), "in 1 minute");
        assert_eq!(notify_window_str(30), "in 30 minutes");
    }

    #[test]
    fn ui_dates() {
        assert_eq!(Inner::ui_parse_dt("2026-10-14", "09:05"), Some(dt("2026-10-14T09:05:00")));
        assert_eq!(Inner::ui_parse_dt(" 2026-10-14 ", " 9:05 "), Some(dt("2026-10-14T09:05:00")));
        assert_eq!(Inner::ui_parse_dt("2026-10-14", "None"), None);
    }
}
