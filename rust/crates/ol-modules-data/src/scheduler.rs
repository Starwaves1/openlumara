//! Port of modules/scheduler/module.py: scheduled (optionally recurring) AI jobs.
//!
//! Python starts one dispatcher task in `on_ready` that sleeps until the next job is
//! due (woken early by an asyncio.Event whenever the schedule changes). The Rust port
//! does the same with a tokio task + `Notify`; the task is aborted in `on_shutdown`
//! (python never cancels it, so reloads leaked dispatchers).

#[path = "../../../generated/modules/scheduler.rs"]
pub mod meta;

use crate::util::{body_get, body_str, fromisoformat_value, iso, now, py_int, py_int_str, replace_time, total_seconds, weekday};
use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{Duration, NaiveDateTime};
use futures::StreamExt;
use ol_core::api::SendOpts;
use ol_core::channel::Channel;
use ol_core::context::GetOpts;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec, UiRoute};
use ol_core::pyjson::{py_str, py_str_opt, truthy};
use ol_core::storage::{StorageList, StorageType};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// names of all available channels. Equivalent to core.channel.get_available_channels(),
/// but without going through config::get_module_structure() (which evaluates this
/// module's own settings -> infinite recursion).
pub fn available_channels() -> Vec<String> {
    let mut out = ol_core::config::available_names("channels");
    out.extend(ol_core::config::available_names("user_channels"));
    out
}

/// channel select options: `{name: f"Send notifications via {name}"}`
pub fn channel_options() -> Value {
    let mut m = Map::new();
    for name in available_channels() {
        m.insert(name.clone(), json!(format!("Send notifications via {}", name)));
    }
    Value::Object(m)
}

/// the class-level settings dict (computed: channel options depend on available channels)
pub fn settings() -> Value {
    json!({
        "notification_channel": {
            "type": "select",
            "default": "webui",
            "description": "Which channel to send notifications to by default",
            "options": channel_options()
        },
        "insert_system_prompt": {
            "description": "Whether to insert a list of all currently scheduled jobs in the system prompt. This will make your AI aware of upcoming scheduled jobs at all times!",
            "default": true
        },
        "prompt_strategy": {
            "type": "select",
            "default": "system prompt + instruction",
            "options": {
                "system prompt + instruction": "Sends the system prompt and the scheduled instruction, without chat history. Token-efficient, but may induce prompt reprocessing due to context switching.",
                "full context": "Sends the entire context window + the scheduled instruction. This makes many API's use the prompt cache to prevent reprocessing, but it will also send your entire history along with the scheduled instructions, which is a lot of tokens.",
                "instruction only": "The most token-efficient option. sends only the scheduled instruction. Drastically reduces token use and cost, but may induce prompt reprocessing due to context switching."
            }
        },
        "allow_recurring_jobs": true
    })
}

// ---------------------------------------------------------
// Pure time / parsing logic
// ---------------------------------------------------------

/// `re.findall(r'(\d+)\s*([wdhms])', s)` -> (weeks, days, hours, minutes, seconds); None if no match
pub fn parse_duration(s: &str) -> Option<[i64; 5]> {
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| regex::Regex::new(r"(\d+)\s*([wdhms])").unwrap());
    let mut out = [0i64; 5];
    let mut any = false;
    for c in RE.captures_iter(s) {
        any = true;
        let val: i64 = c[1].parse().unwrap_or(0);
        let idx = match &c[2] {
            "w" => 0,
            "d" => 1,
            "h" => 2,
            "m" => 3,
            _ => 4,
        };
        out[idx] = val;
    }
    if any {
        Some(out)
    } else {
        None
    }
}

/// parses "HH:MM[:SS]" like add_job/edit_job. Ok((hour, minute, second)) or the error message.
pub fn parse_target_time(target_time: &str) -> Result<(i64, i64, Option<i64>), &'static str> {
    const FMT: &str = "error: invalid time format, use HH:MM or HH:MM:SS";
    let parts: Vec<&str> = target_time.split(':').collect();
    if parts.len() < 2 {
        return Err(FMT);
    }
    let hour = py_int_str(parts[0]).ok_or(FMT)?;
    let minute = py_int_str(parts[1]).ok_or(FMT)?;
    let second = if parts.len() == 3 { Some(py_int_str(parts[2]).ok_or(FMT)?) } else { None };
    if !(0..=23).contains(&hour) {
        return Err("error: hour must be between 0 and 23");
    }
    if !(0..=59).contains(&minute) {
        return Err("error: minute must be between 0 and 59");
    }
    if let Some(s) = second {
        if !(0..=59).contains(&s) {
            return Err("error: second must be between 0 and 59");
        }
    }
    Ok((hour, minute, second))
}

fn num(recur: &Value, key: &str) -> f64 {
    recur.get(key).and_then(ol_core::pyjson::as_f64).unwrap_or(0.0)
}

fn int_or(recur: &Value, key: &str, default: i64) -> i64 {
    match recur.get(key) {
        Some(v) if !v.is_null() => py_int(v).unwrap_or(default),
        _ => default,
    }
}

/// Calculate the next trigger time strictly after `base` (python _calculate_next_from).
/// Ok(None) means an invalid (zero) interval.
pub fn calculate_next_from(base: &NaiveDateTime, recur: &Value) -> Result<Option<NaiveDateTime>, String> {
    if recur.is_null() || !recur.is_object() {
        return Ok(None);
    }
    // MODE 1: specific clock time
    if recur.get("target_hour").map(|v| !v.is_null()).unwrap_or(false) {
        let hour = int_or(recur, "target_hour", 0);
        let minute = int_or(recur, "target_minute", 0);
        let second = int_or(recur, "target_second", 0);
        let mut candidate = replace_time(base, hour, minute, second)?;
        if recur.get("target_weekday").map(|v| !v.is_null()).unwrap_or(false) {
            let target = int_or(recur, "target_weekday", 0);
            let mut days_until = (target - weekday(&candidate)).rem_euclid(7);
            if days_until == 0 && candidate <= *base {
                days_until = 7;
            }
            candidate += Duration::days(days_until);
        } else if recur.get("weekdays_only").map(truthy).unwrap_or(false) {
            if candidate <= *base {
                candidate += Duration::days(1);
            }
            while weekday(&candidate) >= 5 {
                candidate += Duration::days(1);
            }
        } else {
            let mut interval_days = int_or(recur, "days", 0);
            if interval_days == 0 {
                interval_days = 1;
            }
            if candidate <= *base {
                candidate += Duration::days(interval_days);
            }
        }
        return Ok(Some(candidate));
    }
    // MODE 2: relative delta
    let secs = num(recur, "weeks") * 604800.0 + num(recur, "days") * 86400.0 + num(recur, "hours") * 3600.0 + num(recur, "minutes") * 60.0 + num(recur, "seconds");
    if secs == 0.0 {
        return Ok(None);
    }
    Ok(Some(*base + Duration::microseconds((secs * 1e6).round() as i64)))
}

fn weekday_name(wd: i64) -> &'static str {
    const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    if (0..7).contains(&wd) {
        DAYS[wd as usize]
    } else {
        "Unknown"
    }
}

pub fn format_recurring_time(recur: &Value) -> String {
    if recur.get("target_hour").map(|v| !v.is_null()).unwrap_or(false) {
        let hour = int_or(recur, "target_hour", 0);
        let minute = int_or(recur, "target_minute", 0);
        let period = if hour < 12 { "AM" } else { "PM" };
        let mut h = if hour <= 12 { hour } else { hour - 12 };
        if hour == 0 {
            h = 12;
        }
        let time_str = format!("{}:{:02} {}", h, minute, period);
        if recur.get("target_weekday").map(|v| !v.is_null()).unwrap_or(false) {
            return format!("every {} at {}", weekday_name(int_or(recur, "target_weekday", -1)), time_str);
        } else if recur.get("weekdays_only").map(truthy).unwrap_or(false) {
            return format!("every weekday at {}", time_str);
        }
        return format!("every day at {}", time_str);
    }
    let mut parts = Vec::new();
    for k in ["weeks", "days", "hours", "minutes", "seconds"] {
        if let Some(v) = recur.get(k).filter(|v| truthy(v)) {
            parts.push(format!("{} {}", py_str(v), k));
        }
    }
    if parts.is_empty() {
        "invalid schedule".into()
    } else {
        format!("every {}", parts.join(", "))
    }
}

pub fn format_job_time(job: &Value) -> String {
    if job.get("recurring").map(truthy).unwrap_or(false) {
        return format_recurring_time(job.get("recurs_in").unwrap_or(&json!({})));
    }
    match fromisoformat_value(Some(job.get("trigger_time").unwrap_or(&json!("")))) {
        Ok(dt) => format!("one-time, due {}", dt.format("%Y-%m-%d %H:%M")),
        Err(_) => "one-time, invalid time".into(),
    }
}

/// python Scheduler.__str__
pub fn schedule_to_string(schedule: &[Value]) -> String {
    let lines: Vec<String> = schedule
        .iter()
        .map(|job| {
            let id = job.get("id").map(py_str).unwrap_or_else(|| "unknown".into());
            let action = job.get("action").map(py_str).unwrap_or_default();
            let chan = job.get("channel").map(py_str).unwrap_or_else(|| "unknown channel".into());
            format!("{}: {} on {}: {}", id, format_job_time(job), chan, action)
        })
        .collect();
    lines.join("\n")
}

/// builds a recurrence dict from the webui modal's form fields (python _ui_build_recur)
pub fn ui_build_recur(mode: &str, duration: &str, time_str: &str, weekday: Option<&Value>) -> Result<Value, &'static str> {
    let mut recur = json!({
        "weeks": 0, "days": 0, "hours": 0, "minutes": 0, "seconds": 0,
        "target_weekday": null, "weekdays_only": false,
    });
    if mode == "relative" {
        let Some(d) = parse_duration(duration.trim()) else {
            return Err("invalid duration. use something like '2d 4h 30m'");
        };
        // python only overwrites the units that were present; values default to 0 anyway
        for (i, k) in ["weeks", "days", "hours", "minutes", "seconds"].iter().enumerate() {
            recur[*k] = json!(d[i]);
        }
        return Ok(recur);
    }
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() < 2 {
        return Err("invalid time format, use HH:MM");
    }
    let hour = py_int_str(parts[0]).ok_or("invalid time format, use HH:MM")?;
    recur["target_hour"] = json!(hour);
    let minute = py_int_str(parts[1]).ok_or("invalid time format, use HH:MM")?;
    recur["target_minute"] = json!(minute);
    if parts.len() == 3 {
        recur["target_second"] = json!(py_int_str(parts[2]).ok_or("invalid time format, use HH:MM")?);
    }
    if !((0..=23).contains(&hour) && (0..=59).contains(&minute)) {
        return Err("time out of range");
    }
    match weekday {
        Some(Value::String(s)) if s == "weekdays" => recur["weekdays_only"] = json!(true),
        Some(w) if !w.is_null() && py_str(w) != "" => {
            let wd = py_int(w).ok_or("invalid weekday")?;
            if !(0..=6).contains(&wd) {
                return Err("weekday must be between 0 (Monday) and 6 (Sunday)");
            }
            recur["target_weekday"] = json!(wd);
        }
        _ => {}
    }
    Ok(recur)
}

// ---------------------------------------------------------
// Module
// ---------------------------------------------------------

struct Inner {
    base: ModuleBase,
    schedule: Mutex<Option<StorageList>>,
    reschedule: tokio::sync::Notify,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

pub struct Scheduler {
    inner: Arc<Inner>,
}

const RECURRING_DISABLED: &str = "Error: Recurring scheduler jobs are disabled by security policy. Please inform the user.";

impl Scheduler {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self {
            inner: Arc::new(Inner { base, schedule: Mutex::new(None), reschedule: tokio::sync::Notify::new(), task: Mutex::new(None) }),
        }
    }

    /// snapshot of the job list
    pub fn jobs(&self) -> Vec<Value> {
        self.inner.jobs()
    }
}

impl Inner {
    fn with<R>(&self, f: impl FnOnce(&mut StorageList) -> R) -> anyhow::Result<R> {
        let mut g = self.schedule.lock();
        let s = g.as_mut().ok_or_else(|| anyhow!("'Scheduler' object has no attribute 'schedule'"))?;
        Ok(f(s))
    }

    fn jobs(&self) -> Vec<Value> {
        self.with(|s| s.data.clone()).unwrap_or_default()
    }

    fn signal_reschedule(&self) {
        self.reschedule.notify_one();
    }

    fn log(&self, msg: &str) {
        self.base.log("scheduler", msg);
    }

    fn log_error(&self, msg: &str, e: &str) {
        match self.base.manager() {
            Some(m) => m.log_error(msg, e),
            None => ol_core::log("error", &format!("{}: {}", msg, e)),
        }
    }

    fn index_in(data: &[Value], job_id: &Value) -> i64 {
        if job_id.is_null() {
            return -1;
        }
        let want = py_str(job_id);
        for (i, job) in data.iter().enumerate() {
            if job.get("id").map(py_str).unwrap_or_default() == want {
                return i as i64;
            }
        }
        -1
    }

    fn get_index(&self, job_id: &Value) -> i64 {
        self.with(|s| Self::index_in(&s.data, job_id)).unwrap_or(-1)
    }

    fn job_at(&self, idx: i64) -> Option<Value> {
        self.with(|s| s.data.get(idx as usize).cloned()).ok().flatten()
    }

    fn config_allows_recurring(&self) -> bool {
        self.base.config.get_bool("allow_recurring_jobs")
    }

    // ------------- dispatcher -------------

    fn get_next_due_job(&self) -> Option<(Value, f64)> {
        let now = now();
        let mut best: Option<(Value, f64)> = None;
        for job in self.jobs() {
            let Ok(t) = fromisoformat_value(Some(job.get("trigger_time").unwrap_or(&json!("")))) else { continue };
            let delay = total_seconds(&t, &now);
            if best.as_ref().map(|(_, d)| delay < *d).unwrap_or(true) {
                best = Some((job, delay));
            }
        }
        best
    }

    async fn dispatcher_loop(self: Arc<Self>) {
        loop {
            if let Err(e) = self.dispatch_once().await {
                self.log_error("[SCHEDULER] error in dispatcher loop", &e.to_string());
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }

    async fn dispatch_once(self: &Arc<Self>) -> anyhow::Result<()> {
        let Some((next_job, delay)) = self.get_next_due_job() else {
            // no jobs scheduled; wait for a signal that the schedule changed
            self.reschedule.notified().await;
            return Ok(());
        };
        let job_id = next_job.get("id").cloned().unwrap_or(Value::Null);
        if delay <= 0.0 {
            let idx = self.get_index(&job_id);
            let Some(job) = (if idx == -1 { None } else { self.job_at(idx) }) else { return Ok(()) };
            if job.get("recurring").map(truthy).unwrap_or(false) {
                // advance recurring job to future without executing missed instances
                if !self.advance_recurring_to_future(&job) {
                    // python would spin forever here; back off instead
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                self.log(&format!("advanced missed recurring job {} to next future time", py_str(&job_id)));
            } else {
                self.job_wrapper(job).await;
            }
            return Ok(());
        }
        // wait until the job is due or until the schedule changes
        tokio::select! {
            _ = self.reschedule.notified() => return Ok(()),
            _ = tokio::time::sleep(std::time::Duration::from_secs_f64(delay)) => {}
        }
        let idx = self.get_index(&job_id);
        if idx == -1 {
            return Ok(());
        }
        if let Some(job) = self.job_at(idx) {
            self.job_wrapper(job).await;
        }
        Ok(())
    }

    /// executes the job and handles cleanup or recurrence
    async fn job_wrapper(self: &Arc<Self>, job: Value) {
        let Some(job_id) = job.get("id").filter(|v| truthy(v)).cloned() else { return };
        if let Err(e) = self.execute_job(&job).await {
            self.log_error(&format!("[SCHEDULER] error executing job {}", py_str(&job_id)), &e.to_string());
            return;
        }
        let idx = self.get_index(&job_id);
        if idx == -1 {
            return;
        }
        if job.get("recurring").map(truthy).unwrap_or(false) {
            if let Some(current) = self.job_at(idx) {
                self.reschedule_recurring_job(&current);
            }
        } else {
            self.remove_job_from_storage(&job_id);
        }
    }

    fn store_trigger(&self, job: &Value, next: &NaiveDateTime) {
        let id = job.get("id").cloned().unwrap_or(Value::Null);
        let _ = self.with(|s| {
            let idx = Self::index_in(&s.data, &id);
            if idx >= 0 {
                s.data[idx as usize]["trigger_time"] = json!(iso(next));
                s.save();
            }
        });
        self.signal_reschedule();
    }

    fn reschedule_recurring_job(&self, job: &Value) {
        let recur = job.get("recurs_in").cloned().unwrap_or(json!({}));
        let id = job.get("id").map(py_str).unwrap_or_else(|| "None".into());
        let current = fromisoformat_value(Some(job.get("trigger_time").unwrap_or(&json!("")))).unwrap_or_else(|_| now());
        let invalid = || self.log(&format!("could not reschedule recurring job {}: invalid interval", id));
        let mut next = match calculate_next_from(&current, &recur) {
            Ok(Some(n)) => n,
            _ => return invalid(),
        };
        let now = now();
        let max_iterations = 10000;
        let mut iteration = 0;
        while next <= now && iteration < max_iterations {
            next = match calculate_next_from(&next, &recur) {
                Ok(Some(n)) => n,
                _ => return invalid(),
            };
            iteration += 1;
        }
        if next <= now {
            self.log(&format!("could not advance job {} to future after {} iterations", id, max_iterations));
            return;
        }
        self.store_trigger(job, &next);
    }

    /// returns false when the job could not be advanced
    fn advance_recurring_to_future(&self, job: &Value) -> bool {
        let recur = job.get("recurs_in").cloned().unwrap_or(json!({}));
        let id = job.get("id").map(py_str).unwrap_or_else(|| "None".into());
        let current = fromisoformat_value(Some(job.get("trigger_time").unwrap_or(&json!("")))).unwrap_or_else(|_| now());
        let now = now();
        let mut next = current;
        let max_iterations = 10000;
        let mut iteration = 0;
        while next <= now && iteration < max_iterations {
            match calculate_next_from(&next, &recur) {
                Ok(Some(c)) => next = c,
                _ => {
                    self.log(&format!("could not advance job {}: invalid interval", id));
                    return false;
                }
            }
            iteration += 1;
        }
        if next <= now {
            self.log(&format!("could not advance job {} to future after {} iterations", id, max_iterations));
            return false;
        }
        self.store_trigger(job, &next);
        true
    }

    fn remove_job_from_storage(&self, job_id: &Value) {
        let _ = self.with(|s| {
            let idx = Self::index_in(&s.data, job_id);
            if idx >= 0 {
                s.data.remove(idx as usize);
                s.save();
            }
        });
        self.signal_reschedule();
    }

    /// performs the actual action of the job using a private context copy
    async fn execute_job(self: &Arc<Self>, job: &Value) -> anyhow::Result<()> {
        let job_id = py_str_opt(job.get("id"));
        let manager = self.base.manager().ok_or_else(|| anyhow!("manager is gone"))?;
        let channel_name = job.get("channel").filter(|v| truthy(v)).map(py_str).unwrap_or_default().to_lowercase().trim().to_string();
        let job_channel: Option<Arc<Channel>> = manager.channel(&channel_name).or_else(|| manager.current_channel());
        let Some(job_channel) = job_channel else {
            self.log(&format!("error executing job {}: no channel available", job_id));
            return Ok(());
        };

        // filter out scheduler tools to prevent circular scheduling
        let tools: Vec<Value> = manager
            .tools()
            .into_iter()
            .filter(|t| !t.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("").starts_with("scheduler_"))
            .collect();

        let action = job.get("action").cloned().unwrap_or(Value::Null);
        let instr_str = format!(
            "\n[AUTOMATED SYSTEM INSTRUCTION]\nPlease follow these instructions:\n\n{}\nUse tools if needed. For simple reminders, do not use tools.\n        ",
            py_str(&action)
        )
        .trim()
        .to_string();
        let api = manager.api();
        let instr_role = if api.supports_developer_role() { "developer" } else { "user" };
        let instruction_message = json!({"role": instr_role, "content": instr_str});
        let instruction_message_pure = json!({"role": "system", "content": action});

        let base_delay = 5u64;
        let response: Value = loop {
            let attempt: Result<Value, String> = async {
                let final_messages: Vec<Value> = match self.base.config.get_str("prompt_strategy").as_deref() {
                    Some("instruction only") => vec![instruction_message_pure.clone()],
                    Some("system prompt + instruction") => {
                        let opts = GetOpts { system_prompt: true, end_prompt: false, history: false, ..Default::default() };
                        let mut m = job_channel
                            .context
                            .get(opts)
                            .await
                            .map_err(|e| e.to_string())?
                            .ok_or("unsupported operand type(s) for +: 'NoneType' and 'list'")?;
                        m.push(instruction_message.clone());
                        m
                    }
                    Some("full context") => {
                        let opts = GetOpts { end_prompt: false, ..Default::default() };
                        let mut m = job_channel
                            .context
                            .get(opts)
                            .await
                            .map_err(|e| e.to_string())?
                            .ok_or("'NoneType' object is not iterable")?;
                        m.push(instruction_message.clone());
                        m
                    }
                    _ => vec![],
                };
                let opts = SendOpts { use_tools: true, tools: Some(tools.clone()), ..Default::default() };
                api.send(&final_messages, opts).await.map_err(|e| e.to_string())
            }
            .await;
            match attempt {
                Ok(r) if truthy(&r) => break r,
                Ok(_) => self.log(&format!("job {}: empty response, retrying in {}s", job_id, base_delay)),
                Err(e) => self.log(&format!("job {}: failed: {}, retrying in {}s", job_id, e, base_delay)),
            }
            tokio::time::sleep(std::time::Duration::from_secs(base_delay)).await;
        };

        // process response
        let mut final_content: Value = response.get("content").cloned().unwrap_or(json!(""));
        let has_tool_calls = response.get("tool_calls").map(truthy).unwrap_or(false);
        if has_tool_calls {
            let mut stream = ol_core::toolcalls::process(job_channel.clone(), response.clone(), true);
            let mut parts = Vec::new();
            while let Some(token) = stream.next().await {
                if token.get("type").and_then(|t| t.as_str()) == Some("content") {
                    parts.push(token.get("content").map(py_str).unwrap_or_default());
                }
            }
            let tool_content = parts.concat();
            if !tool_content.is_empty() {
                final_content = json!(format!("{}\n{}", py_str(&final_content), tool_content).trim());
            }
        }
        if truthy(&final_content) {
            let msg = match final_content {
                Value::String(_) | Value::Object(_) => final_content,
                other => json!(py_str(&other)),
            };
            job_channel.push(msg, true).await;
        }
        Ok(())
    }

    // ------------- tools -------------

    fn resolve_channel(&self, explicit: Option<&Value>) -> Option<Value> {
        if let Some(c) = explicit.filter(|v| truthy(v)) {
            return Some(c.clone());
        }
        if let Some(c) = self.base.config.get1("notification_channel").filter(truthy) {
            return Some(c);
        }
        self.base.channel().map(|c| json!(c.name))
    }

    async fn add_job(&self, args: &ToolArgs) -> anyhow::Result<Value> {
        let recurring = args.0.get("recurring").cloned().unwrap_or(json!(false));
        if truthy(&recurring) && !self.config_allows_recurring() {
            return Ok(self.base.result(RECURRING_DISABLED, false));
        }
        let mut d = [0i64; 5];
        if let Some(rd) = args.str("relative_duration").filter(|s| !s.is_empty()) {
            if let Some(p) = parse_duration(&rd) {
                d = p;
            }
        }
        let mut target: Option<(i64, i64, Option<i64>)> = None;
        if let Some(tt) = args.str("target_time").filter(|s| !s.is_empty()) {
            match parse_target_time(&tt) {
                Ok(t) => target = Some(t),
                Err(e) => return Ok(self.base.result(e, false)),
            }
        }
        let target_weekday = args.i64("target_weekday");
        if let Some(wd) = target_weekday {
            if !(0..=6).contains(&wd) {
                return Ok(self.base.result("error: target_weekday must be between 0 (Monday) and 6 (Sunday)", false));
            }
        }
        let weekdays_only = args.0.get("weekdays_only").cloned().unwrap_or(json!(false));
        let mut recur = json!({
            "weeks": d[0], "days": d[1], "hours": d[2], "minutes": d[3], "seconds": d[4],
            "target_weekday": target_weekday,
            "weekdays_only": weekdays_only,
        });
        if let Some((h, m, s)) = target {
            recur["target_hour"] = json!(h);
            recur["target_minute"] = json!(m);
            if let Some(s) = s {
                recur["target_second"] = json!(s);
            }
        }
        let trigger = match calculate_next_from(&now(), &recur) {
            Ok(Some(t)) => t,
            Ok(None) => return Ok(self.base.result("error: invalid schedule parameters (zero interval)", false)),
            Err(e) => return Ok(self.base.result(format!("error: {}", e), false)),
        };
        let Some(channel) = self.resolve_channel(args.raw("channel")) else {
            return Ok(self.base.result("error: no channel context available", false));
        };
        let job_id = ol_core::globals::new_ulid();
        let job = json!({
            "id": job_id,
            "action": args.0.get("action").cloned().unwrap_or(Value::Null),
            "channel": py_str(&channel).to_lowercase().trim(),
            "trigger_time": iso(&trigger),
            "recurring": recurring,
            "recurs_in": if truthy(&recurring) { recur } else { Value::Null },
        });
        self.with(|s| {
            s.data.push(job);
            s.save();
        })?;
        self.signal_reschedule();
        Ok(self.base.result(format!("job added. ID: {}", job_id), true))
    }

    async fn edit_job(&self, args: &ToolArgs) -> anyhow::Result<Value> {
        let id = args.0.get("id").cloned().unwrap_or(Value::Null);
        let index = self.get_index(&id);
        let Some(existing) = (if index == -1 { None } else { self.job_at(index) }) else {
            return Ok(self.base.result("id does not exist", false));
        };
        let existing_recur = existing.get("recurs_in").filter(|v| truthy(v)).cloned().unwrap_or(json!({}));
        let final_recurring = match args.raw("recurring") {
            Some(r) => r.clone(),
            None => existing.get("recurring").cloned().unwrap_or(json!(false)),
        };
        if truthy(&final_recurring) && !self.config_allows_recurring() {
            return Ok(self.base.result(RECURRING_DISABLED, false));
        }

        let relative_duration = args.str("relative_duration");
        let target_time = args.str("target_time");
        let target_weekday = args.i64("target_weekday");
        let weekdays_only = args.raw("weekdays_only").cloned();

        let dur: Option<[i64; 5]> = relative_duration.as_ref().map(|rd| parse_duration(rd).unwrap_or([0; 5]));
        let mut target: Option<(i64, i64, Option<i64>)> = None;
        if let Some(tt) = &target_time {
            match parse_target_time(tt) {
                Ok(t) => target = Some(t),
                Err(e) => return Ok(self.base.result(e, false)),
            }
        }
        if let Some(wd) = target_weekday {
            if !(0..=6).contains(&wd) {
                return Ok(self.base.result("error: target_weekday must be between 0 (Monday) and 6 (Sunday)", false));
            }
        }
        let final_weekdays_only = weekdays_only.clone().unwrap_or_else(|| existing_recur.get("weekdays_only").cloned().unwrap_or(json!(false)));
        let keys = ["weeks", "days", "hours", "minutes", "seconds"];
        let mut recur = Map::new();
        for (i, k) in keys.iter().enumerate() {
            let v = match &dur {
                Some(d) => json!(d[i]),
                None => existing_recur.get(*k).cloned().unwrap_or(json!(0)),
            };
            recur.insert(k.to_string(), v);
        }
        recur.insert(
            "target_weekday".into(),
            match target_weekday {
                Some(w) => json!(w),
                None => existing_recur.get("target_weekday").cloned().unwrap_or(Value::Null),
            },
        );
        recur.insert("weekdays_only".into(), final_weekdays_only);
        if let Some((h, m, s)) = target {
            recur.insert("target_hour".into(), json!(h));
            recur.insert("target_minute".into(), json!(m));
            if let Some(s) = s {
                recur.insert("target_second".into(), json!(s));
            }
        } else if let Some(h) = existing_recur.get("target_hour") {
            recur.insert("target_hour".into(), h.clone());
            recur.insert("target_minute".into(), existing_recur.get("target_minute").cloned().unwrap_or(json!(0)));
            if let Some(s) = existing_recur.get("target_second") {
                recur.insert("target_second".into(), s.clone());
            }
        }
        let recur = Value::Object(recur);

        let time_params_changed = relative_duration.is_some() || target_time.is_some() || target_weekday.is_some() || weekdays_only.is_some();
        let trigger_time = if time_params_changed {
            match calculate_next_from(&now(), &recur) {
                Ok(Some(t)) => json!(iso(&t)),
                Ok(None) => return Ok(self.base.result("error: invalid schedule parameters (zero interval)", false)),
                Err(e) => return Ok(self.base.result(format!("error: {}", e), false)),
            }
        } else {
            existing.get("trigger_time").cloned().unwrap_or(Value::Null)
        };
        let updated = json!({
            "id": id,
            "action": args.raw("action").cloned().unwrap_or_else(|| existing.get("action").cloned().unwrap_or(Value::Null)),
            "channel": args.raw("channel").cloned().unwrap_or_else(|| existing.get("channel").cloned().unwrap_or(Value::Null)),
            "trigger_time": trigger_time,
            "recurring": final_recurring,
            "recurs_in": if truthy(&final_recurring) { recur } else { Value::Null },
        });
        self.with(|s| {
            if let Some(slot) = s.data.get_mut(index as usize) {
                *slot = updated;
            }
            s.save();
        })?;
        self.signal_reschedule();
        Ok(self.base.result("job edited", true))
    }

    async fn remove_job(&self, id: &Value) -> anyhow::Result<Value> {
        let removed = self.with(|s| {
            let idx = Self::index_in(&s.data, id);
            if idx == -1 {
                return false;
            }
            s.data.remove(idx as usize);
            s.save();
            true
        })?;
        if !removed {
            return Ok(self.base.result("id does not exist", false));
        }
        self.signal_reschedule();
        Ok(self.base.result("job deleted", true))
    }

    // ------------- webui routes -------------

    fn route_list(&self) -> anyhow::Result<Value> {
        self.with(|s| s.load())?;
        let mut items: Vec<Value> = self
            .jobs()
            .iter()
            .map(|job| {
                json!({
                    "id": job.get("id").cloned().unwrap_or(Value::Null),
                    "action": job.get("action").cloned().unwrap_or(json!("")),
                    "channel": job.get("channel").cloned().unwrap_or(json!("")),
                    "trigger_time": job.get("trigger_time").cloned().unwrap_or(json!("")),
                    "recurring": job.get("recurring").map(truthy).unwrap_or(false),
                    "recurs_in": job.get("recurs_in").cloned().unwrap_or(Value::Null),
                    "display": format_job_time(job),
                })
            })
            .collect();
        items.sort_by_key(|j| {
            let t = &j["trigger_time"];
            if truthy(t) {
                py_str(t)
            } else {
                "9999".to_string()
            }
        });
        Ok(Value::Array(items))
    }

    fn form_recur(&self, body: &Option<Value>) -> Result<Value, &'static str> {
        let mode = body_get(body, "mode").map(|v| py_str(&v)).unwrap_or_else(|| "relative".into());
        let duration = body_get(body, "duration").filter(truthy).map(|v| py_str(&v)).unwrap_or_default();
        let time = body_get(body, "time").filter(truthy).map(|v| py_str(&v)).unwrap_or_default();
        let weekday = body_get(body, "weekday");
        ui_build_recur(&mode, &duration, &time, weekday.as_ref())
    }

    async fn route_create_or_update(&self, body: &Option<Value>, update: bool) -> anyhow::Result<Value> {
        let mut index = -1;
        let job_id = body_get(body, "id").unwrap_or(Value::Null);
        if update {
            if !truthy(&job_id) {
                return Ok(self.base.result("no job id given", false));
            }
            self.with(|s| s.load())?;
            index = self.get_index(&job_id);
            if index == -1 {
                return Ok(self.base.result("job does not exist", false));
            }
        }
        let action = body_str(body, "action", "").trim().to_string();
        if action.is_empty() {
            return Ok(self.base.result("job action cannot be empty", false));
        }
        let recur = match self.form_recur(body) {
            Ok(r) => r,
            Err(e) => return Ok(self.base.result(e, false)),
        };
        let recurring = body_get(body, "recurring").map(|v| truthy(&v)).unwrap_or(false);
        if recurring && !self.config_allows_recurring() {
            return Ok(self.base.result("recurring jobs are disabled in the scheduler settings", false));
        }
        let trigger = match calculate_next_from(&now(), &recur) {
            Ok(Some(t)) => t,
            Ok(None) => return Ok(self.base.result("invalid schedule parameters (zero interval)", false)),
            Err(e) => return Err(anyhow!(e)),
        };
        let resolved = if update {
            let existing = self.job_at(index).unwrap_or(json!({}));
            body_get(body, "channel")
                .filter(truthy)
                .or_else(|| existing.get("channel").filter(|v| truthy(v)).cloned())
                .or_else(|| self.base.config.get1("notification_channel").filter(truthy))
        } else {
            self.resolve_channel(body_get(body, "channel").as_ref())
        };
        let Some(channel) = resolved else {
            return Ok(self.base.result("no channel available", false));
        };
        let new_id = if update { job_id.clone() } else { json!(ol_core::globals::new_ulid()) };
        let job = json!({
            "id": new_id,
            "action": action,
            "channel": py_str(&channel).to_lowercase().trim(),
            "trigger_time": iso(&trigger),
            "recurring": recurring,
            "recurs_in": if recurring { recur } else { Value::Null },
        });
        self.with(|s| {
            if update {
                if let Some(slot) = s.data.get_mut(index as usize) {
                    *slot = job;
                }
            } else {
                s.data.push(job);
            }
            s.save();
        })?;
        self.signal_reschedule();
        if update {
            Ok(self.base.result("job edited", true))
        } else {
            Ok(self.base.result(format!("job added. ID: {}", py_str(&new_id)), true))
        }
    }
}

#[async_trait]
impl Module for Scheduler {
    fn base(&self) -> &ModuleBase {
        &self.inner.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "add_job" => self.inner.add_job(&args).await,
            "edit_job" => self.inner.edit_job(&args).await,
            "remove_job" => self.inner.remove_job(&args.0.get("id").cloned().unwrap_or(Value::Null)).await,
            _ => anyhow::bail!("'Scheduler' object has no attribute '{}'", method),
        }
    }

    async fn on_ready(&self) -> anyhow::Result<()> {
        *self.inner.schedule.lock() = Some(StorageList::new("schedule", StorageType::Json, None)?);
        let task = tokio::spawn(self.inner.clone().dispatcher_loop());
        if let Some(old) = self.inner.task.lock().replace(task) {
            old.abort();
        }
        Ok(())
    }

    async fn on_shutdown(&self) -> anyhow::Result<()> {
        let task = self.inner.task.lock().take();
        if let Some(t) = task {
            t.abort();
            let _ = t.await;
        }
        Ok(())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        let insert = self.inner.base.config.get1("insert_system_prompt").map(|v| truthy(&v)).unwrap_or(true);
        if !insert {
            return Ok(None);
        }
        let jobs = self.inner.jobs();
        if jobs.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!("Your scheduler system will trigger these events at the specified times:\n{}", schedule_to_string(&jobs))))
    }

    fn ui_routes(&self) -> Vec<UiRoute> {
        meta::ui_routes().into_iter().map(|(r, _)| r).collect()
    }

    async fn ui_route(&self, path: &str, method: &str, body: Option<Value>, _query: Map<String, Value>) -> anyhow::Result<Value> {
        match (path, method) {
            ("list", "GET") => self.inner.route_list(),
            ("channels", "GET") => Ok(json!(available_channels())),
            ("create", "POST") => self.inner.route_create_or_update(&body, false).await,
            ("update", "POST") => self.inner.route_create_or_update(&body, true).await,
            ("delete", "POST") => {
                let Some(id) = body_get(&body, "id").filter(truthy) else {
                    return Ok(self.inner.base.result("no job id given", false));
                };
                self.inner.with(|s| s.load())?;
                self.inner.remove_job(&id).await
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
        crate::util::fromisoformat(s).unwrap()
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("2d 4h 30m"), Some([0, 2, 4, 30, 0]));
        assert_eq!(parse_duration("1w5s"), Some([1, 0, 0, 0, 5]));
        assert_eq!(parse_duration("soon"), None);
    }

    #[test]
    fn target_times() {
        assert_eq!(parse_target_time("14:30"), Ok((14, 30, None)));
        assert_eq!(parse_target_time("14:30:05"), Ok((14, 30, Some(5))));
        assert_eq!(parse_target_time("24:00"), Err("error: hour must be between 0 and 23"));
        assert_eq!(parse_target_time("1430"), Err("error: invalid time format, use HH:MM or HH:MM:SS"));
        assert_eq!(parse_target_time("ab:cd"), Err("error: invalid time format, use HH:MM or HH:MM:SS"));
    }

    #[test]
    fn next_relative() {
        let base = dt("2026-10-14T10:00:00");
        let r = json!({"weeks": 0, "days": 1, "hours": 2, "minutes": 0, "seconds": 0});
        assert_eq!(calculate_next_from(&base, &r).unwrap(), Some(dt("2026-10-15T12:00:00")));
        let zero = json!({"weeks": 0, "days": 0, "hours": 0, "minutes": 0, "seconds": 0});
        assert_eq!(calculate_next_from(&base, &zero).unwrap(), None);
        assert_eq!(calculate_next_from(&base, &Value::Null).unwrap(), None);
    }

    #[test]
    fn next_clock_time() {
        // 2026-10-14 is a Wednesday
        let base = dt("2026-10-14T10:00:00");
        let later_today = json!({"days": 0, "target_hour": 14, "target_minute": 30, "target_weekday": null, "weekdays_only": false});
        assert_eq!(calculate_next_from(&base, &later_today).unwrap(), Some(dt("2026-10-14T14:30:00")));
        let earlier = json!({"days": 0, "target_hour": 9, "target_minute": 0});
        assert_eq!(calculate_next_from(&base, &earlier).unwrap(), Some(dt("2026-10-15T09:00:00")));
        let every_3_days = json!({"days": 3, "target_hour": 9, "target_minute": 0});
        assert_eq!(calculate_next_from(&base, &every_3_days).unwrap(), Some(dt("2026-10-17T09:00:00")));
        // monday
        let monday = json!({"target_hour": 9, "target_minute": 0, "target_weekday": 0});
        assert_eq!(calculate_next_from(&base, &monday).unwrap(), Some(dt("2026-10-19T09:00:00")));
        // wednesday earlier time -> next week
        let wed = json!({"target_hour": 9, "target_minute": 0, "target_weekday": 2});
        assert_eq!(calculate_next_from(&base, &wed).unwrap(), Some(dt("2026-10-21T09:00:00")));
        // weekdays only from friday evening -> monday
        let fri = dt("2026-10-16T20:00:00");
        let wk = json!({"target_hour": 9, "target_minute": 0, "weekdays_only": true});
        assert_eq!(calculate_next_from(&fri, &wk).unwrap(), Some(dt("2026-10-19T09:00:00")));
    }

    #[test]
    fn formatting() {
        assert_eq!(format_recurring_time(&json!({"target_hour": 0, "target_minute": 5})), "every day at 12:05 AM");
        assert_eq!(format_recurring_time(&json!({"target_hour": 13, "target_minute": 0, "target_weekday": 4})), "every Friday at 1:00 PM");
        assert_eq!(format_recurring_time(&json!({"target_hour": 12, "target_minute": 0, "weekdays_only": true})), "every weekday at 12:00 PM");
        assert_eq!(format_recurring_time(&json!({"weeks": 0, "days": 2, "hours": 3})), "every 2 days, 3 hours");
        assert_eq!(format_recurring_time(&json!({})), "invalid schedule");
        let job = json!({"id": "X", "action": "do it", "channel": "webui", "trigger_time": "2026-10-14T10:00:00.123", "recurring": false});
        assert_eq!(schedule_to_string(&[job]), "X: one-time, due 2026-10-14 10:00 on webui: do it");
    }

    #[test]
    fn ui_recur() {
        let r = ui_build_recur("relative", "1h", "", None).unwrap();
        assert_eq!(r["hours"], 1);
        assert_eq!(ui_build_recur("relative", "", "", None).unwrap_err(), "invalid duration. use something like '2d 4h 30m'");
        let r = ui_build_recur("clock", "", "08:15", Some(&json!("weekdays"))).unwrap();
        assert_eq!(r, json!({"weeks": 0, "days": 0, "hours": 0, "minutes": 0, "seconds": 0, "target_weekday": null, "weekdays_only": true, "target_hour": 8, "target_minute": 15}));
        let r = ui_build_recur("clock", "", "08:15", Some(&json!("3"))).unwrap();
        assert_eq!(r["target_weekday"], 3);
        assert_eq!(ui_build_recur("clock", "", "08:15", Some(&json!("9"))).unwrap_err(), "weekday must be between 0 (Monday) and 6 (Sunday)");
        assert_eq!(ui_build_recur("clock", "", "25:00", None).unwrap_err(), "time out of range");
        assert_eq!(ui_build_recur("clock", "", "x", None).unwrap_err(), "invalid time format, use HH:MM");
    }
}
