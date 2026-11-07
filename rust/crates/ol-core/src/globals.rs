//! Port of the module-level globals in core/__init__.py and the logging helpers in core/functions.py

use std::sync::atomic::{AtomicBool, Ordering};

pub const VERSION: f64 = 1.0;

static FIRSTRUN: AtomicBool = AtomicBool::new(false);
static QUIET: AtomicBool = AtomicBool::new(false);
static DEBUG: AtomicBool = AtomicBool::new(false);
static DEBUG_STREAM: AtomicBool = AtomicBool::new(false);
static INSECURE_TLS: AtomicBool = AtomicBool::new(false);

pub fn firstrun() -> bool {
    FIRSTRUN.load(Ordering::Relaxed)
}
pub fn set_firstrun(v: bool) {
    FIRSTRUN.store(v, Ordering::Relaxed)
}
pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}
pub fn set_quiet(v: bool) {
    QUIET.store(v, Ordering::Relaxed)
}
pub fn debug() -> bool {
    DEBUG.load(Ordering::Relaxed)
}
pub fn set_debug(v: bool) {
    DEBUG.store(v, Ordering::Relaxed)
}
pub fn debug_stream() -> bool {
    DEBUG_STREAM.load(Ordering::Relaxed)
}
pub fn set_debug_stream(v: bool) {
    DEBUG_STREAM.store(v, Ordering::Relaxed)
}
pub fn insecure_tls() -> bool {
    INSECURE_TLS.load(Ordering::Relaxed)
}
pub fn set_insecure_tls(v: bool) {
    INSECURE_TLS.store(v, Ordering::Relaxed)
}

/// simple console log (core.log). prints `[CATEGORY] msg`.
pub fn log(category: &str, msg: &str) {
    println!("[{}] {}", category.to_uppercase(), msg);
}

/// provides more detail about an error, but in a compact format (core.detail_error)
pub fn detail_error<E: std::fmt::Debug + std::fmt::Display + ?Sized>(e: &E) -> String {
    if !debug() {
        return e.to_string();
    }
    format!("{} | {:?}", e, e)
}

/// python `datetime.datetime.utcnow().isoformat()`
pub fn utcnow_isoformat() -> String {
    naive_isoformat(&chrono::Utc::now().naive_utc())
}

/// python `datetime.datetime.isoformat()` of a naive datetime (microseconds omitted when 0)
pub fn naive_isoformat(dt: &chrono::NaiveDateTime) -> String {
    use chrono::Timelike;
    if dt.nanosecond() / 1000 == 0 {
        dt.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        dt.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
    }
}

/// python `datetime.fromisoformat()` for the formats openlumara writes (naive or with offset).
/// An offset is not converted: python keeps the string's wall-clock fields, so
/// `2026-01-02T03:04:05+02:00` yields 03:04:05 (strftime prints those fields).
pub fn parse_isoformat(s: &str) -> Option<chrono::NaiveDateTime> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.naive_local());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt);
        }
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0))
}

/// `str(ulid.ULID())` (26 chars, Crockford base32)
pub fn new_ulid() -> String {
    ulid::Ulid::new().to_string()
}

/// unix timestamp in seconds (int(time.time()))
pub fn unix_time() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_isoformat_keeps_wall_clock_of_offset_strings() {
        // python: datetime.fromisoformat("2026-01-02T03:04:05+02:00").strftime("%H:%M:%S") == "03:04:05"
        let dt = parse_isoformat("2026-01-02T03:04:05+02:00").unwrap();
        assert_eq!(dt.format("%m/%d/%y %H:%M:%S").to_string(), "01/02/26 03:04:05");
        assert_eq!(dt, chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap().and_hms_opt(3, 4, 5).unwrap());
        // a negative offset keeps its wall clock too
        assert_eq!(parse_isoformat("2026-01-02T03:04:05-05:30").unwrap().format("%H:%M:%S").to_string(), "03:04:05");
        // Z is UTC, so its wall clock is unchanged as well
        assert_eq!(parse_isoformat("2026-01-02T03:04:05Z").unwrap().format("%H:%M:%S").to_string(), "03:04:05");
    }

    #[test]
    fn parse_isoformat_naive_forms() {
        assert_eq!(parse_isoformat("2026-01-02T03:04:05").unwrap().format("%H:%M:%S").to_string(), "03:04:05");
        assert!(parse_isoformat("nope").is_none());
    }
}
