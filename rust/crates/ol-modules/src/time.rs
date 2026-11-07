//! Port of modules/time/module.py: makes the AI aware of the current time and date, either as
//! an end-of-prompt line or injected into each user message.

#[path = "../../../generated/modules/time.rs"]
pub mod meta;

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{Datelike, Offset, Timelike, Utc};
use chrono_tz::{OffsetComponents, OffsetName};
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use serde_json::{json, Map, Value};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Arc;

const METHOD_END_PROMPT: &str = "end prompt";
const METHOD_MESSAGE_INJECTION: &str = "message injection";

/// the class-level settings dict. Not a pure literal in python (the timezone options are computed),
/// so it is written out here rather than read from the generated meta.
pub fn settings() -> Value {
    json!({
        "method": {
            "type": "select",
            "default": "message injection",
            "description": "What method to use to make your AI aware of time",
            "options": {
                "message injection": "Injects timestamps into the messages you send. This will make your AI able to see when any message was sent, and give it a sense of how much time has passed between each message!",
                "end prompt": "Injects the current time/date at the end of message history, which is a more basic way of making your AI aware of time. It will make your AI only know the current time and have no sense of the passage of time"
            }
        },
        "add_timezone": {
            "default": true,
            "description": "Puts your timezone in the timestamps that are sent to the AI. Makes the AI timezone-aware!"
        },
        "timezone": {
            "default": "local",
            "description": "Your timezone",
            "type": "select",
            "options": timezone_options(),
            "depends": "add_timezone"
        },
        "date_format": {
            "default": "%c",
            "description": "A string that describes exactly how to display the date/time to your AI. Uses strftime format (https://github.com/Vishxnu/Python-strftime-cheatsheet)",
            "depends": "add_timezone"
        }
    })
}

/// python `TIMEZONES`: "local" first, then every available zone name in sorted order
fn timezone_options() -> Value {
    let mut options = Map::new();
    options.insert("local".into(), json!("Use your device's local timezone"));
    let mut names: Vec<&str> = chrono_tz::TZ_VARIANTS.iter().map(|tz| tz.name()).collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        options.insert(name.to_string(), json!(format!("Set your timezone to {}", name)));
    }
    Value::Object(options)
}

/// one instant in the configured zone: python's timezone-aware datetime, reduced to what the
/// module reads (wall-clock fields, microseconds, utcoffset() and tzname())
#[derive(Debug, Clone, PartialEq)]
pub struct Stamp {
    pub naive: chrono::NaiveDateTime,
    pub micros: u32,
    pub offset_secs: i32,
    pub tzname: String,
    /// the timetuple `tm_isdst` flag: -1 for the local zone, else 1 or 0 from dst()
    pub isdst: i32,
}

/// python `datetime.datetime.now().astimezone()`: the device's local zone. The zone abbreviation is
/// the libc `tm_zone`, which is what python's `tzname()` reports for an `astimezone()` result.
/// python's `astimezone()` result has a fixed-offset tzinfo, so its `dst()` is None and the
/// timetuple flag is -1 (let mktime decide).
fn local_stamp() -> Stamp {
    let now = chrono::Local::now();
    let secs = now.timestamp() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    let tzname = if tm.tm_zone.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned()
    };
    Stamp {
        naive: now.naive_local(),
        micros: now.timestamp_subsec_micros(),
        offset_secs: now.offset().local_minus_utc(),
        tzname,
        isdst: -1,
    }
}

/// python `zoneinfo.ZoneInfo(key)` + `datetime.now(tz=...)`
fn zone_stamp(key: &str) -> anyhow::Result<Stamp> {
    validate_zone_key(key)?;
    let tz: chrono_tz::Tz = key
        .parse()
        .map_err(|_| anyhow!("'No time zone found with key {}'", key))?;
    let now = Utc::now().with_timezone(&tz);
    let tzname = now.offset().abbreviation().unwrap_or("").to_string();
    // python's timetuple() flag: 1 when dst() is non-zero, else 0
    let isdst = if now.offset().dst_offset().num_seconds() != 0 { 1 } else { 0 };
    Ok(Stamp {
        naive: now.naive_local(),
        micros: now.timestamp_subsec_micros(),
        offset_secs: now.offset().fix().local_minus_utc(),
        tzname,
        isdst,
    })
}

/// the base python's `zoneinfo._tzpath._validate_tzfile_path` checks keys against (`_TEST_PATH`)
const ZONE_KEY_BASE: &str = "_/";

/// python `zoneinfo._tzpath._validate_tzfile_path`: the ValueError for a malformed key, raised
/// before any lookup. A well-formed key that names no zone is handled by `zone_stamp`.
fn validate_zone_key(key: &str) -> anyhow::Result<()> {
    if key.starts_with('/') {
        return Err(anyhow!("ZoneInfo keys may not be absolute paths, got: {}", key));
    }
    if posix_normpath(key).len() != key.len() {
        return Err(anyhow!("ZoneInfo keys must be normalized relative paths, got: {}", key));
    }
    let resolved = posix_normpath(&format!("{}{}", ZONE_KEY_BASE, key));
    if !resolved.starts_with(ZONE_KEY_BASE) {
        return Err(anyhow!("ZoneInfo keys must refer to subdirectories of TZPATH, got: {}", key));
    }
    Ok(())
}

/// python `os.path.normpath` for posix paths: drops empty and "." parts and resolves ".."
fn posix_normpath(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => match parts.last() {
                Some(last) if *last != ".." => {
                    parts.pop();
                }
                _ if !absolute => parts.push(".."),
                _ => {}
            },
            other => parts.push(other),
        }
    }
    let body = parts.join("/");
    if absolute {
        format!("/{}", body)
    } else if body.is_empty() {
        ".".to_string()
    } else {
        body
    }
}

/// python `_get_current_time()`: `timezone` == "local" is the device zone, anything else is an IANA key
fn current_stamp(timezone: Option<&str>) -> anyhow::Result<Stamp> {
    match timezone {
        Some("local") => Ok(local_stamp()),
        Some(key) => zone_stamp(key),
        None => Err(anyhow!("expected str, bytes or os.PathLike object, not NoneType")),
    }
}

/// `%z` as python formats it: sign, HHMM, plus SS when the offset has seconds
fn format_utcoffset(offset_secs: i32) -> String {
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let a = offset_secs.abs();
    let (hh, mm, ss) = (a / 3600, (a % 3600) / 60, a % 60);
    if ss != 0 {
        format!("{sign}{hh:02}{mm:02}{ss:02}")
    } else {
        format!("{sign}{hh:02}{mm:02}")
    }
}

/// python `datetime.strftime` pre-pass (`_wrap_strftime`): %z, %Z and %f are expanded here, every
/// other directive (and "%%") is passed through to the C library
fn wrap_strftime(fmt: &str, stamp: &Stamp) -> String {
    let mut out = String::with_capacity(fmt.len());
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            None => out.push('%'),
            Some('z') => out.push_str(&format_utcoffset(stamp.offset_secs)),
            Some('Z') => out.push_str(&stamp.tzname),
            Some('f') => out.push_str(&format!("{:06}", stamp.micros)),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

/// python `time.strftime(fmt, timetuple)`: the C library strftime on the datetime's fields, with
/// the buffer growth loop CPython uses
fn c_strftime(fmt: &str, stamp: &Stamp) -> anyhow::Result<String> {
    let fmt_c = CString::new(fmt).map_err(|_| anyhow!("embedded null character"))?;
    let zone_c = CString::new(stamp.tzname.replace('\0', "")).unwrap_or_default();
    let naive = &stamp.naive;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_sec = naive.second() as _;
    tm.tm_min = naive.minute() as _;
    tm.tm_hour = naive.hour() as _;
    tm.tm_mday = naive.day() as _;
    tm.tm_mon = naive.month0() as _;
    tm.tm_year = (naive.year() - 1900) as _;
    tm.tm_wday = naive.weekday().num_days_from_sunday() as _;
    tm.tm_yday = naive.ordinal0() as _;
    tm.tm_isdst = stamp.isdst as _;
    tm.tm_gmtoff = stamp.offset_secs as _;
    tm.tm_zone = zone_c.as_ptr() as *const c_char;
    let mut size = 1024usize;
    loop {
        let mut buf = vec![0u8; size];
        let n = unsafe { libc::strftime(buf.as_mut_ptr() as *mut c_char, size, fmt_c.as_ptr(), &tm) };
        if n > 0 || size >= 256 * fmt.len() {
            return Ok(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        size *= 2;
    }
}

/// python `now.strftime(date_format)`, then the zone name appended when `add_timezone` is set,
/// joined with a space
fn time_info(stamp: &Stamp, date_format: &str, add_timezone: bool) -> anyhow::Result<String> {
    let mut parts = vec![c_strftime(&wrap_strftime(date_format, stamp), stamp)?];
    if add_timezone {
        parts.push(stamp.tzname.clone());
    }
    Ok(parts.join(" "))
}

pub struct Time {
    base: ModuleBase,
}

impl Time {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }

    /// the formatted "<date> [<tzname>]" text, read from the current config
    fn time_text(&self) -> anyhow::Result<String> {
        let timezone = self.base.config.get_str("timezone");
        let stamp = current_stamp(timezone.as_deref())?;
        let date_format = self
            .base
            .config
            .get_str("date_format")
            .ok_or_else(|| anyhow!("strftime() argument 1 must be str, not None"))?;
        time_info(&stamp, &date_format, self.base.config.get_bool("add_timezone"))
    }
}

#[async_trait]
impl Module for Time {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn call_tool(&self, method: &str, _args: ToolArgs) -> anyhow::Result<Value> {
        anyhow::bail!("'Time' object has no attribute '{}'", method)
    }

    async fn on_end_prompt(&self) -> anyhow::Result<Option<String>> {
        if self.base.config.get_str("method").as_deref() != Some(METHOD_END_PROMPT) {
            return Ok(None);
        }
        Ok(Some(format!("Current time/date is {}", self.time_text()?)))
    }

    async fn on_message_inject(&self) -> anyhow::Result<Option<String>> {
        if self.base.config.get_str("method").as_deref() != Some(METHOD_MESSAGE_INJECTION) {
            return Ok(None);
        }
        Ok(Some(format!("sent on {}", self.time_text()?)))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_stamp() -> Stamp {
        Stamp {
            naive: chrono::NaiveDate::from_ymd_opt(2026, 11, 7).unwrap().and_hms_micro_opt(14, 3, 22, 500_000).unwrap(),
            micros: 500_000,
            offset_secs: 3600,
            tzname: "CET".into(),
            isdst: 0,
        }
    }

    #[test]
    fn c_locale_default_format_and_python_directives() {
        let s = fixed_stamp();
        // python "%c" in the C locale, and the datetime-only directives %Z %z %f
        assert_eq!(time_info(&s, "%c", false).unwrap(), "Sat Nov  7 14:03:22 2026");
        assert_eq!(time_info(&s, "%Y-%m-%d %H:%M:%S.%f %Z %z %% %j", false).unwrap(), "2026-11-07 14:03:22.500000 CET +0100 % 311");
        // %%Z is a literal "%Z", not the zone name
        assert_eq!(time_info(&s, "%%Z", false).unwrap(), "%Z");
    }

    #[test]
    fn timezone_suffix_is_appended_with_a_space() {
        let s = fixed_stamp();
        assert_eq!(time_info(&s, "%c", true).unwrap(), "Sat Nov  7 14:03:22 2026 CET");
        let utc = zone_stamp("UTC").unwrap();
        assert_eq!(utc.offset_secs, 0);
        assert_eq!(utc.tzname, "UTC");
        assert_eq!(time_info(&utc, "%H:%M %z", true).unwrap(), format!("{} +0000 UTC", utc.naive.format("%H:%M")));
    }

    #[test]
    fn unknown_zone_key_is_an_error() {
        let err = current_stamp(Some("Nope/Nowhere")).unwrap_err();
        assert_eq!(err.to_string(), "'No time zone found with key Nope/Nowhere'");
        assert!(current_stamp(None).is_err());
    }

    #[test]
    fn malformed_zone_keys_raise_python_value_errors() {
        // python ZoneInfo raises ValueError (no KeyError quoting) for these keys
        let cases = [
            ("", "ZoneInfo keys must be normalized relative paths, got: "),
            ("/abs", "ZoneInfo keys may not be absolute paths, got: /abs"),
            ("../x", "ZoneInfo keys must refer to subdirectories of TZPATH, got: ../x"),
            ("a/../b", "ZoneInfo keys must be normalized relative paths, got: a/../b"),
            ("America/", "ZoneInfo keys must be normalized relative paths, got: America/"),
        ];
        for (key, msg) in cases {
            assert_eq!(current_stamp(Some(key)).unwrap_err().to_string(), msg, "key {key:?}");
        }
        // "../_/x" normalizes back under the validation base, so it is a well-formed but unknown key
        assert_eq!(
            current_stamp(Some("../_/x")).unwrap_err().to_string(),
            "'No time zone found with key ../_/x'"
        );
    }

    extern "C" {
        fn tzset();
    }

    #[test]
    fn strftime_seconds_honours_the_dst_flag() {
        let _g = crate::util::CONFIG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // %s calls mktime on the tm struct, so the isdst flag picks the zone offset in effect
        let saved = std::env::var_os("TZ");
        std::env::set_var("TZ", "Europe/Berlin");
        unsafe { tzset() };
        let mut summer = fixed_stamp();
        summer.naive = chrono::NaiveDate::from_ymd_opt(2026, 7, 12).unwrap().and_hms_opt(12, 0, 0).unwrap();
        summer.offset_secs = 7200;
        summer.tzname = "CEST".into();
        summer.isdst = 1;
        let with_dst = c_strftime("%s", &summer).unwrap();
        summer.isdst = 0;
        let without_dst = c_strftime("%s", &summer).unwrap();
        match saved {
            Some(v) => std::env::set_var("TZ", v),
            None => std::env::remove_var("TZ"),
        }
        unsafe { tzset() };
        // python: datetime(2026,7,12,12,0,tzinfo=ZoneInfo("Europe/Berlin")).timestamp() == 1783850400
        assert_eq!(with_dst, "1783850400");
        assert_eq!(without_dst, "1783854000");
    }

    #[test]
    fn settings_defaults_and_timezone_options() {
        let s = settings();
        assert_eq!(s["method"]["default"], json!("message injection"));
        assert_eq!(s["date_format"]["default"], json!("%c"));
        assert_eq!(s["timezone"]["depends"], json!("add_timezone"));
        let opts = s["timezone"]["options"].as_object().unwrap();
        // "local" comes first, then the zone names in sorted order
        assert_eq!(opts.keys().next().unwrap(), "local");
        assert_eq!(opts["local"], json!("Use your device's local timezone"));
        assert_eq!(opts["UTC"], json!("Set your timezone to UTC"));
        let names: Vec<&String> = opts.keys().skip(1).collect();
        assert!(names.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn hooks_follow_the_configured_method() {
        let _g = crate::util::CONFIG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let data = json!({
            "modules": {"settings": {"time": {"method": "end prompt", "timezone": "UTC", "add_timezone": true, "date_format": "%Y"}}}
        });
        ol_core::config::set_for_tests(data.clone());
        // set_for_tests() keeps a config file from an earlier test, and every read reloads that file,
        // so the same data is written to it (JSON is valid YAML)
        let path = ol_core::config::config_path().expect("test config path");
        std::fs::write(&path, data.to_string()).expect("write test config");

        let m = Time::new(ModuleBase::detached("time"));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let end = rt.block_on(m.on_end_prompt()).unwrap().unwrap();
        assert_eq!(end, format!("Current time/date is {} UTC", Utc::now().format("%Y")));
        assert_eq!(rt.block_on(m.on_message_inject()).unwrap(), None);
    }
}
