//! Port of modules/time/module.py: makes the AI aware of the current time and date, either as
//! an end-of-prompt line or injected into each user message.

#[path = "../../../generated/modules/time.rs"]
pub mod meta;

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, FixedOffset, Offset, Timelike, Utc};
use chrono_tz::{OffsetComponents, OffsetName};
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::ffi::{CStr, CString};
use std::io::Read;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
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

/// python `TIMEZONES`: "local" first, then every available zone name in sorted order. The names are the
/// host's TZif files (python's `available_timezones()`) plus chrono-tz's compiled list, which keeps the
/// built-in zones available on hosts without tzdata.
fn timezone_options() -> Value {
    // one sorted, duplicate-free set over both sources, so chrono-only names sit in sorted position too
    let mut names = host_zone_names();
    names.extend(chrono_tz::TZ_VARIANTS.iter().map(|tz| tz.name().to_string()));
    let mut options = Map::new();
    options.insert("local".into(), json!("Use your device's local timezone"));
    for name in names {
        options.insert(name.clone(), json!(format!("Set your timezone to {}", name)));
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

/// python `zoneinfo.ZoneInfo(key)` + `datetime.now(tz=...)`. chrono-tz is tried first; a key it lacks
/// (a host-only name such as Factory or localtime) is read from the host's TZif file on TZPATH.
fn zone_stamp(key: &str) -> anyhow::Result<Stamp> {
    validate_zone_key(key)?;
    let utc_now = Utc::now();
    let tz: chrono_tz::Tz = match key.parse() {
        Ok(tz) => tz,
        Err(_) => return host_zone_stamp(key, utc_now),
    };
    let now = utc_now.with_timezone(&tz);
    let tzname = now.offset().abbreviation().unwrap_or("").to_string();
    // python's timetuple() flag: 1 when dst() is non-zero, else 0
    let isdst = if now.offset().dst_offset().num_seconds() != 0 { 1 } else { 0 };
    Ok(Stamp {
        naive: now.naive_local(),
        micros: utc_now.timestamp_subsec_micros(),
        offset_secs: now.offset().fix().local_minus_utc(),
        tzname,
        isdst,
    })
}

/// python `zoneinfo.TZPATH`: PYTHONTZPATH when it is set (only absolute entries count, an empty value
/// means no entries), else the search path the reference CPython was built with
fn tzpath() -> Vec<PathBuf> {
    match std::env::var_os("PYTHONTZPATH") {
        Some(env) => env
            .to_string_lossy()
            .split(':')
            .filter(|p| p.starts_with('/'))
            .map(PathBuf::from)
            .collect(),
        None => ["/usr/share/zoneinfo", "/usr/lib/zoneinfo", "/usr/share/lib/zoneinfo", "/etc/zoneinfo"]
            .iter()
            .map(PathBuf::from)
            .collect(),
    }
}

/// python `zoneinfo._tzpath.find_tzfile`: the first file for `key` on TZPATH
fn find_tzfile(key: &str) -> Option<PathBuf> {
    tzpath().into_iter().map(|dir| dir.join(key)).find(|path| path.is_file())
}

/// python `ZoneInfo(key)` for a key chrono-tz does not know: the host's TZif file for it. A file that is
/// not TZif raises ValueError, and a key with no file is not found.
fn host_zone_stamp(key: &str, utc_now: DateTime<Utc>) -> anyhow::Result<Stamp> {
    let path = find_tzfile(key).ok_or_else(|| anyhow!("'No time zone found with key {}'", key))?;
    let data = std::fs::read(&path)?;
    let ty = parse_tzif(&data)?.type_at(&utc_now);
    let offset = FixedOffset::east_opt(ty.utoff).ok_or_else(|| {
        anyhow!("offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24).")
    })?;
    let local = utc_now.with_timezone(&offset);
    Ok(Stamp {
        naive: local.naive_local(),
        micros: utc_now.timestamp_subsec_micros(),
        offset_secs: ty.utoff,
        tzname: ty.abbr,
        // python's timetuple() flag for dst(): 1 for a DST type, else 0
        isdst: i32::from(ty.isdst),
    })
}

/// one local time type of a TZif file (python `_ttinfo`)
#[derive(Debug, Clone, PartialEq)]
struct TzType {
    utoff: i32,
    isdst: bool,
    abbr: String,
}

/// a POSIX TZ string (the TZif footer): a standard type, and optionally a DST type with its start and
/// end rules
#[derive(Debug, Clone)]
struct PosixTz {
    std: TzType,
    dst: Option<(TzType, Rule, Rule)>,
}

#[derive(Debug, Clone, Copy)]
enum DayRule {
    /// `Jn`: day 1..=365 of the year, with Feb 29 never counted
    Julian1(i64),
    /// `n`: day 0..=365 of the year, with Feb 29 counted
    Julian0(i64),
    /// `Mm.w.d`: weekday d (0 = Sunday) of week w (5 = the last one) of month m
    MonthWeekDay(i64, i64, i64),
}

/// a transition rule: the day, and the local wall-clock time of day in seconds
#[derive(Debug, Clone, Copy)]
struct Rule {
    day: DayRule,
    time: i64,
}

/// the zone after the last transition: a POSIX rule when the footer has DST, else a fixed type
#[derive(Debug)]
enum After {
    Fixed(TzType),
    Rule(PosixTz),
}

/// a parsed TZif file (python `_tzfile`)
#[derive(Debug)]
struct TzFile {
    trans: Vec<i64>,
    trans_types: Vec<TzType>,
    before: TzType,
    after: After,
}

impl TzFile {
    /// python `ZoneInfo.fromutc()` for the instant `utc_now`: the local time type in effect then
    fn type_at(&self, utc_now: &DateTime<Utc>) -> TzType {
        let n = self.trans.len();
        if n == 0 {
            return match &self.after {
                After::Fixed(ty) => ty.clone(),
                After::Rule(rule) => rule.at(utc_now),
            };
        }
        let t = utc_now.timestamp();
        if t < self.trans[0] {
            return self.before.clone();
        }
        if t > self.trans[n - 1] {
            if let After::Rule(rule) = &self.after {
                return rule.at(utc_now);
            }
        }
        let idx = self.trans.partition_point(|&at| at <= t) - 1;
        self.trans_types[idx].clone()
    }
}

impl PosixTz {
    /// python `_TZStr._get_trans_info_fromutc`: the type in effect at `utc_now` under the DST rules
    fn at(&self, utc_now: &DateTime<Utc>) -> TzType {
        let Some((dst, start_rule, end_rule)) = &self.dst else {
            return self.std.clone();
        };
        let t = utc_now.timestamp();
        let year = i64::from(utc_now.year());
        let start = rule_epoch(year, start_rule) - i64::from(self.std.utoff);
        let end = rule_epoch(year, end_rule) - i64::from(dst.utoff);
        let in_dst = if start < end { start <= t && t < end } else { !(end <= t && t < start) };
        if in_dst {
            dst.clone()
        } else {
            self.std.clone()
        }
    }
}

fn invalid_tzif(what: &str) -> anyhow::Error {
    anyhow!("Invalid TZif file: {}", what)
}

/// the six header counts of the TZif header at `at`: isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt
fn tzif_counts(data: &[u8], at: usize) -> anyhow::Result<[usize; 6]> {
    if data.get(at..at + 4) != Some(b"TZif".as_slice()) {
        return Err(invalid_tzif("magic not found"));
    }
    let header = data.get(at..at + 44).ok_or_else(|| invalid_tzif("truncated header"))?;
    let mut counts = [0usize; 6];
    for (i, slot) in counts.iter_mut().enumerate() {
        let b = &header[20 + 4 * i..24 + 4 * i];
        *slot = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize;
    }
    Ok(counts)
}

/// the byte length of a data block after its header, for `tsize`-byte transition times
fn tzif_block_len(c: [usize; 6], tsize: usize) -> usize {
    let [isut, isstd, leap, time, types, chars] = c;
    time * tsize + time + types * 6 + chars + leap * (tsize + 4) + isstd + isut
}

/// the data block at `at`: transitions, the type of each transition, and every local time type
fn parse_tzif_block(
    data: &[u8],
    at: usize,
    c: [usize; 6],
    tsize: usize,
) -> anyhow::Result<(Vec<i64>, Vec<TzType>, Vec<TzType>)> {
    let [_, _, _, time, types, chars] = c;
    let body = data
        .get(at..at + tzif_block_len(c, tsize))
        .ok_or_else(|| invalid_tzif("truncated data"))?;
    let times_end = time * tsize;
    let idx_end = times_end + time;
    let types_end = idx_end + types * 6;
    let trans: Vec<i64> = body[..times_end]
        .chunks_exact(tsize)
        .map(|b| {
            if tsize == 4 {
                i64::from(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            } else {
                i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
            }
        })
        .collect();
    let abbr_chars = &body[types_end..types_end + chars];
    let mut all_types = Vec::with_capacity(types);
    for b in body[idx_end..types_end].as_chunks::<6>().0 {
        let rest = abbr_chars.get(b[5] as usize..).ok_or_else(|| invalid_tzif("bad abbreviation index"))?;
        let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
        all_types.push(TzType {
            utoff: i32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            isdst: b[4] != 0,
            abbr: String::from_utf8_lossy(&rest[..end]).into_owned(),
        });
    }
    let mut trans_types = Vec::with_capacity(time);
    for &i in &body[times_end..idx_end] {
        let ty = all_types.get(i as usize).ok_or_else(|| invalid_tzif("bad transition type index"))?;
        trans_types.push(ty.clone());
    }
    Ok((trans, trans_types, all_types))
}

/// python `zoneinfo._common.load_data` + `_load_file`: a TZif file, version 1 or 2+ (the 64-bit block
/// and POSIX footer)
fn parse_tzif(data: &[u8]) -> anyhow::Result<TzFile> {
    let c1 = tzif_counts(data, 0)?;
    let (trans, trans_types, types, footer) = if data[4] == 0 {
        let (trans, trans_types, types) = parse_tzif_block(data, 44, c1, 4)?;
        (trans, trans_types, types, String::new())
    } else {
        let second = 44 + tzif_block_len(c1, 4);
        let c2 = tzif_counts(data, second)?;
        let block_at = second + 44;
        let (trans, trans_types, types) = parse_tzif_block(data, block_at, c2, 8)?;
        let rest = &data[block_at + tzif_block_len(c2, 8)..];
        let footer = match rest.first() {
            Some(b'\n') => {
                let body = &rest[1..];
                let end = body.iter().position(|&c| c == b'\n').unwrap_or(body.len());
                String::from_utf8_lossy(&body[..end]).into_owned()
            }
            _ => String::new(),
        };
        (trans, trans_types, types, footer)
    };
    if footer.is_empty() && types.is_empty() {
        return Err(anyhow!("No time zone information found."));
    }
    let before = types
        .iter()
        .find(|t| !t.isdst)
        .or_else(|| trans_types.first())
        .or_else(|| types.first())
        .cloned()
        .ok_or_else(|| anyhow!("No time zone information found."))?;
    let after = if footer.is_empty() {
        let last = trans_types.last().or_else(|| types.last()).cloned();
        After::Fixed(last.ok_or_else(|| anyhow!("No time zone information found."))?)
    } else {
        let tz = parse_posix_tz(&footer).ok_or_else(|| invalid_tzif("bad TZ string footer"))?;
        if tz.dst.is_none() {
            After::Fixed(tz.std)
        } else {
            After::Rule(tz)
        }
    };
    Ok(TzFile { trans, trans_types, before, after })
}

/// a cursor over a POSIX TZ string
struct PosixParser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl PosixParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        let hit = self.peek() == Some(c);
        if hit {
            self.pos += 1;
        }
        hit
    }

    /// a standard or DST abbreviation: `<...>` quoted, or at least three letters
    fn name(&mut self) -> Option<String> {
        if self.eat(b'<') {
            let start = self.pos;
            while self.peek()? != b'>' {
                self.pos += 1;
            }
            let name = String::from_utf8_lossy(&self.s[start..self.pos]).into_owned();
            self.pos += 1;
            return Some(name);
        }
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        if self.pos - start < 3 {
            return None;
        }
        Some(String::from_utf8_lossy(&self.s[start..self.pos]).into_owned())
    }

    fn number(&mut self) -> Option<i64> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        std::str::from_utf8(&self.s[start..self.pos]).ok()?.parse().ok()
    }

    /// `[+|-]hh[:mm[:ss]]` in seconds, with the sign as written
    fn offset(&mut self) -> Option<i64> {
        let sign = if self.eat(b'-') {
            -1
        } else {
            self.eat(b'+');
            1
        };
        let mut secs = self.number()? * 3600;
        if self.eat(b':') {
            secs += self.number()? * 60;
            if self.eat(b':') {
                secs += self.number()?;
            }
        }
        Some(sign * secs)
    }

    /// one transition rule with its optional `/time` (default 02:00:00)
    fn rule(&mut self) -> Option<Rule> {
        let day = if self.eat(b'M') {
            let m = self.number()?;
            if !self.eat(b'.') {
                return None;
            }
            let w = self.number()?;
            if !self.eat(b'.') {
                return None;
            }
            let d = self.number()?;
            if !((1..=12).contains(&m) && (1..=5).contains(&w) && (0..=6).contains(&d)) {
                return None;
            }
            DayRule::MonthWeekDay(m, w, d)
        } else if self.eat(b'J') {
            let n = self.number()?;
            if !(1..=365).contains(&n) {
                return None;
            }
            DayRule::Julian1(n)
        } else {
            let n = self.number()?;
            if !(0..=365).contains(&n) {
                return None;
            }
            DayRule::Julian0(n)
        };
        let time = if self.eat(b'/') { self.offset()? } else { 7200 };
        Some(Rule { day, time })
    }
}

/// python `zoneinfo._parse_tz_str`: `std offset [dst [offset] [,start[/time],end[/time]]]`
fn parse_posix_tz(s: &str) -> Option<PosixTz> {
    let mut p = PosixParser { s: s.as_bytes(), pos: 0 };
    let std_name = p.name()?;
    // POSIX offsets are positive west of Greenwich, so the UT offset is the negation
    let std = TzType { utoff: i32::try_from(-p.offset()?).ok()?, isdst: false, abbr: std_name };
    if p.peek().is_none() {
        return Some(PosixTz { std, dst: None });
    }
    let dst_name = p.name()?;
    let dst_utoff = if matches!(p.peek(), None | Some(b',')) {
        std.utoff + 3600
    } else {
        i32::try_from(-p.offset()?).ok()?
    };
    let (start, end) = if p.eat(b',') {
        let start = p.rule()?;
        if !p.eat(b',') {
            return None;
        }
        (start, p.rule()?)
    } else {
        // python's default DST rules when the footer gives none
        (
            Rule { day: DayRule::MonthWeekDay(3, 2, 0), time: 7200 },
            Rule { day: DayRule::MonthWeekDay(11, 1, 0), time: 7200 },
        )
    };
    if p.peek().is_some() {
        return None;
    }
    let dst = TzType { utoff: dst_utoff, isdst: true, abbr: dst_name };
    Some(PosixTz { std, dst: Some((dst, start, end)) })
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// days since 1970-01-01 of a proleptic Gregorian date
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// python `_DayOffset` / `_CalendarOffset.year_to_epoch`: the rule's local date and time of day as
/// seconds since the epoch (the local wall-clock fields read as if they were UTC)
fn rule_epoch(year: i64, rule: &Rule) -> i64 {
    let jan1 = days_from_civil(year, 1, 1);
    let day = match rule.day {
        DayRule::Julian1(n) => jan1 + n - 1 + if is_leap(year) && n >= 60 { 1 } else { 0 },
        DayRule::Julian0(n) => jan1 + n,
        DayRule::MonthWeekDay(m, w, d) => {
            let first = days_from_civil(year, m, 1);
            // 1970-01-01 was a Thursday (weekday 4, with Sunday = 0)
            let first_weekday = (first + 4).rem_euclid(7);
            let mut day = first + (d - first_weekday).rem_euclid(7) + (w - 1) * 7;
            let (next_year, next_month) = if m == 12 { (year + 1, 1) } else { (year, m + 1) };
            let next_month_start = days_from_civil(next_year, next_month, 1);
            // week 5 means the last such weekday: step back a week while past the month's end
            while day >= next_month_start {
                day -= 7;
            }
            day
        }
    };
    day * 86400 + rule.time
}

/// python `zoneinfo.available_timezones()` on this host: every TZif file under TZPATH, keyed by its path
/// relative to that root. At the root `right/` and `posix/` are skipped, symlinked directories are listed
/// but not descended into, and "posixrules" is dropped.
fn host_zone_names() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for root in tzpath() {
        if root.exists() {
            collect_zone_names(&root, &root, &mut names);
        }
    }
    names.remove("posixrules");
    names
}

fn collect_zone_names(root: &Path, dir: &Path, names: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // os.walk classifies entries with is_dir() (which follows links) but descends only into real directories
        let is_dir = std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            let is_link = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            let skipped = dir == root && matches!(entry.file_name().to_str(), Some("right") | Some("posix"));
            if !is_link && !skipped {
                collect_zone_names(root, &path, names);
            }
        } else if is_tzif_file(&path) {
            if let Ok(key) = path.strip_prefix(root) {
                names.insert(key.to_string_lossy().into_owned());
            }
        }
    }
}

/// python's `valid_key`: the file starts with the TZif magic
fn is_tzif_file(path: &Path) -> bool {
    let mut magic = Vec::new();
    std::fs::File::open(path).and_then(|f| f.take(4).read_to_end(&mut magic)).is_ok() && magic == b"TZif"
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

/// the python type name of a config value, as it appears in TypeError messages
fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// python `_get_current_time()`: `timezone` == "local" is the device zone, anything else is an IANA key
fn current_stamp(timezone: &Value) -> anyhow::Result<Stamp> {
    match timezone {
        Value::String(key) if key == "local" => Ok(local_stamp()),
        Value::String(key) => zone_stamp(key),
        // python looks the key up in ZoneInfo's cache (a dict) before it checks the type
        Value::Array(_) => Err(anyhow!("cannot use 'list' as a dict key (unhashable type: 'list')")),
        Value::Object(_) => Err(anyhow!("cannot use 'dict' as a dict key (unhashable type: 'dict')")),
        other => Err(anyhow!("expected str, bytes or os.PathLike object, not {}", py_type_name(other))),
    }
}

/// python `strftime()` format argument: only a str is accepted. The C-level argument error spells None
/// as "None" (the ZoneInfo error in `current_stamp` uses the "NoneType" spelling).
fn format_arg(v: &Value) -> anyhow::Result<&str> {
    let type_name = match v {
        Value::Null => "None",
        other => py_type_name(other),
    };
    v.as_str().ok_or_else(|| anyhow!("strftime() argument 1 must be str, not {}", type_name))
}

/// `%z` / `%:z` as python formats it: sign, HHMM (HH:MM with `sep` ":"), plus SS when the offset has seconds
fn format_utcoffset(offset_secs: i32, sep: &str) -> String {
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let a = offset_secs.abs();
    let (hh, mm, ss) = (a / 3600, (a % 3600) / 60, a % 60);
    if ss != 0 {
        format!("{sign}{hh:02}{sep}{mm:02}{sep}{ss:02}")
    } else {
        format!("{sign}{hh:02}{sep}{mm:02}")
    }
}

/// python `datetime.strftime` pre-pass (`_wrap_strftime`): %z, %:z, %Z and %f are expanded here, every
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
            Some('z') => out.push_str(&format_utcoffset(stamp.offset_secs, "")),
            // %:z is only a directive when a 'z' follows the colon; otherwise both characters pass through
            Some(':') if chars.as_str().starts_with('z') => {
                chars.next();
                out.push_str(&format_utcoffset(stamp.offset_secs, ":"));
            }
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

/// python `datetime.strftime(fmt)`: the `_wrap_strftime` pre-pass and then the C library for each
/// NUL-separated piece of the format, with the NULs kept in the output (CPython 3.14 does this)
fn py_strftime(fmt: &str, stamp: &Stamp) -> anyhow::Result<String> {
    let pieces = fmt
        .split('\0')
        .map(|piece| c_strftime(&wrap_strftime(piece, stamp), stamp))
        .collect::<anyhow::Result<Vec<String>>>()?;
    Ok(pieces.join("\0"))
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
    let mut parts = vec![py_strftime(date_format, stamp)?];
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
        let timezone = self.base.config.get1("timezone").unwrap_or(Value::Null);
        let stamp = current_stamp(&timezone)?;
        let date_format = self.base.config.get1("date_format").unwrap_or(Value::Null);
        time_info(&stamp, format_arg(&date_format)?, self.base.config.get_bool("add_timezone"))
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
        let err = current_stamp(&json!("Nope/Nowhere")).unwrap_err();
        assert_eq!(err.to_string(), "'No time zone found with key Nope/Nowhere'");
        assert_eq!(
            current_stamp(&Value::Null).unwrap_err().to_string(),
            "expected str, bytes or os.PathLike object, not NoneType"
        );
    }

    #[test]
    fn non_string_config_values_name_their_python_type() {
        // python: ZoneInfo(value) and strftime(value) name the real type in their TypeError
        let zone_cases = [
            (json!(5), "expected str, bytes or os.PathLike object, not int"),
            (json!(1.5), "expected str, bytes or os.PathLike object, not float"),
            (json!(true), "expected str, bytes or os.PathLike object, not bool"),
            (json!(["a"]), "cannot use 'list' as a dict key (unhashable type: 'list')"),
            (json!({}), "cannot use 'dict' as a dict key (unhashable type: 'dict')"),
        ];
        for (value, msg) in zone_cases {
            assert_eq!(current_stamp(&value).unwrap_err().to_string(), msg, "value {value}");
        }
        let fmt_cases = [
            (json!(5), "strftime() argument 1 must be str, not int"),
            (json!(false), "strftime() argument 1 must be str, not bool"),
            (Value::Null, "strftime() argument 1 must be str, not None"),
        ];
        for (value, msg) in fmt_cases {
            assert_eq!(format_arg(&value).unwrap_err().to_string(), msg, "value {value}");
        }
    }

    #[test]
    fn colon_z_and_nul_pieces_follow_python_strftime() {
        let s = fixed_stamp();
        // python 3.14: %:z is +HH:MM, with :SS when the offset has seconds
        assert_eq!(time_info(&s, "%:z", false).unwrap(), "+01:00");
        assert_eq!(time_info(&s, "%:%z", false).unwrap(), "%:+0100");
        let mut odd = fixed_stamp();
        odd.offset_secs = -(5 * 3600 + 29 * 60 + 53);
        assert_eq!(time_info(&odd, "%:z|%z", false).unwrap(), "-05:29:53|-052953");
        // python 3.14 keeps a NUL in the output and formats each side on its own
        assert_eq!(time_info(&s, "%Y\0%m", false).unwrap(), "2026\011");
        assert_eq!(time_info(&s, "a\0%Y\0", false).unwrap(), "a\x002026\x00");
        assert_eq!(time_info(&s, "x%%\0%%Y", false).unwrap(), "x%\0%Y");
    }

    #[test]
    fn zone_lookup_reports_a_non_tzif_file_as_a_value_error() {
        let _g = crate::util::CONFIG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // python reads the first file for the key on TZPATH: a file that is not TZif is a ValueError
        let dir = std::env::temp_dir().join(format!("time-tzpath-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zone.tab"), "# tzdata text, not TZif\n").unwrap();
        let saved = std::env::var_os("PYTHONTZPATH");
        std::env::set_var("PYTHONTZPATH", &dir);
        let not_tzif = zone_stamp("zone.tab").unwrap_err().to_string();
        let missing = zone_stamp("Not/AZone").unwrap_err().to_string();
        match saved {
            Some(v) => std::env::set_var("PYTHONTZPATH", v),
            None => std::env::remove_var("PYTHONTZPATH"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(not_tzif, "Invalid TZif file: magic not found");
        assert_eq!(missing, "'No time zone found with key Not/AZone'");
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
            assert_eq!(current_stamp(&json!(key)).unwrap_err().to_string(), msg, "key {key:?}");
        }
        // "../_/x" normalizes back under the validation base, so it is a well-formed but unknown key
        assert_eq!(
            current_stamp(&json!("../_/x")).unwrap_err().to_string(),
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

    /// a TZif version 2 file: the (empty) version 1 block, then the given transitions (time, type index),
    /// local time types (utoff, isdst, abbreviation) and POSIX footer
    fn tzif_bytes(trans: &[(i64, u8)], types: &[(i32, bool, &str)], footer: &str) -> Vec<u8> {
        fn header(out: &mut Vec<u8>, counts: [u32; 6]) {
            out.extend_from_slice(b"TZif2");
            out.extend_from_slice(&[0u8; 15]);
            for c in counts {
                out.extend_from_slice(&c.to_be_bytes());
            }
        }
        let mut chars = Vec::new();
        let mut abbr_at = Vec::new();
        for (_, _, abbr) in types {
            abbr_at.push(chars.len() as u8);
            chars.extend_from_slice(abbr.as_bytes());
            chars.push(0);
        }
        let mut out = Vec::new();
        header(&mut out, [0; 6]);
        header(&mut out, [0, 0, 0, trans.len() as u32, types.len() as u32, chars.len() as u32]);
        for (at, _) in trans {
            out.extend_from_slice(&at.to_be_bytes());
        }
        for (_, idx) in trans {
            out.push(*idx);
        }
        for (i, (utoff, isdst, _)) in types.iter().enumerate() {
            out.extend_from_slice(&utoff.to_be_bytes());
            out.push(u8::from(*isdst));
            out.push(abbr_at[i]);
        }
        out.extend_from_slice(&chars);
        out.extend_from_slice(format!("\n{footer}\n").as_bytes());
        out
    }

    #[test]
    fn host_tzif_zone_gives_its_offset_and_abbreviation() {
        let _g = crate::util::CONFIG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // a key chrono-tz does not know is read from the TZif file on TZPATH, like python's ZoneInfo
        let dir = std::env::temp_dir().join(format!("time-host-zone-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Test")).unwrap();
        std::fs::write(dir.join("Test/Fixed"), tzif_bytes(&[], &[(-19800, false, "TST")], "TST5:30")).unwrap();
        let saved = std::env::var_os("PYTHONTZPATH");
        std::env::set_var("PYTHONTZPATH", &dir);
        let stamp = zone_stamp("Test/Fixed");
        match saved {
            Some(v) => std::env::set_var("PYTHONTZPATH", v),
            None => std::env::remove_var("PYTHONTZPATH"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        let stamp = stamp.unwrap();
        assert_eq!(stamp.offset_secs, -19800);
        assert_eq!(stamp.tzname, "TST");
        assert_eq!(stamp.isdst, 0);
        assert_eq!(time_info(&stamp, "%H:%M %Z", false).unwrap(), format!("{} TST", stamp.naive.format("%H:%M")));
        assert_eq!(time_info(&stamp, "%H:%M", true).unwrap(), format!("{} TST", stamp.naive.format("%H:%M")));
    }

    #[test]
    fn tzif_transitions_pick_the_type_in_effect() {
        let data = tzif_bytes(&[(1000, 0), (2000, 1)], &[(3600, false, "AAA"), (7200, true, "BBB")], "");
        let tz = parse_tzif(&data).unwrap();
        let at = |t: i64| DateTime::from_timestamp(t, 0).unwrap();
        // before the first transition: the first standard type
        assert_eq!(tz.type_at(&at(500)).abbr, "AAA");
        assert_eq!(tz.type_at(&at(1500)).abbr, "AAA");
        assert_eq!(tz.type_at(&at(2000)).abbr, "BBB");
        // after the last transition with no footer rule: the last transition's type
        let later = tz.type_at(&at(9999));
        assert_eq!((later.abbr.as_str(), later.utoff, later.isdst), ("BBB", 7200, true));
    }

    #[test]
    fn posix_footer_rule_switches_at_the_us_transitions() {
        let rule = parse_posix_tz("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let at = |y: i32, mo: u32, d: u32, h: u32, mi: u32| {
            chrono::NaiveDate::from_ymd_opt(y, mo, d).unwrap().and_hms_opt(h, mi, 0).unwrap().and_utc()
        };
        // 2026-03-08 is the second Sunday of March: DST starts at 02:00 EST, which is 07:00 UTC
        assert_eq!(rule.at(&at(2026, 3, 8, 6, 59)).utoff, -18000);
        assert_eq!(rule.at(&at(2026, 3, 8, 7, 0)).utoff, -14400);
        // 2026-11-01 is the first Sunday of November: DST ends at 02:00 EDT, which is 06:00 UTC
        assert_eq!(rule.at(&at(2026, 11, 1, 5, 59)).utoff, -14400);
        let winter = rule.at(&at(2026, 11, 1, 6, 0));
        assert_eq!((winter.utoff, winter.abbr.as_str(), winter.isdst), (-18000, "EST", false));
        assert_eq!(rule.at(&at(2026, 7, 1, 12, 0)).abbr, "EDT");
    }

    #[test]
    fn timezone_options_follow_the_host_tzdata_walk() {
        let _g = crate::util::CONFIG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // python's available_timezones(): TZif files only, no right/ or posix/ at the root, no
        // posixrules, no symlinked directories, and no text files
        let dir = std::env::temp_dir().join(format!("time-options-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Test")).unwrap();
        std::fs::create_dir_all(dir.join("right/Test")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("Test/Zone"), tzif_bytes(&[], &[(0, false, "UTC")], "UTC0")).unwrap();
        std::fs::write(dir.join("right/Test/Zone"), tzif_bytes(&[], &[(0, false, "UTC")], "UTC0")).unwrap();
        std::fs::write(dir.join("posixrules"), tzif_bytes(&[], &[(0, false, "UTC")], "UTC0")).unwrap();
        std::fs::write(dir.join("zone.tab"), "# tzdata text, not TZif\n").unwrap();
        std::os::unix::fs::symlink(dir.join("Test"), dir.join("LinkDir")).unwrap();
        let saved = std::env::var_os("PYTHONTZPATH");
        std::env::set_var("PYTHONTZPATH", &dir);
        let options = timezone_options();
        match saved {
            Some(v) => std::env::set_var("PYTHONTZPATH", v),
            None => std::env::remove_var("PYTHONTZPATH"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        let opts = options.as_object().unwrap();
        assert_eq!(opts["Test/Zone"], json!("Set your timezone to Test/Zone"));
        assert_eq!(opts["local"], json!("Use your device's local timezone"));
        assert!(!opts.contains_key("right/Test/Zone"));
        assert!(!opts.contains_key("posixrules"));
        assert!(!opts.contains_key("zone.tab"));
        assert!(!opts.keys().any(|k| k.starts_with("LinkDir")));
    }

    #[test]
    fn timezone_options_are_local_then_every_zone_sorted_and_unique() {
        let _g = crate::util::CONFIG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // python: TIMEZONES = {"local": ...} then update() with sorted(available_timezones()), so "local"
        // is first and the zone names follow in code-point order with no repeats
        let dir = std::env::temp_dir().join(format!("time-order-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Mid")).unwrap();
        std::fs::write(dir.join("Zzz_Last"), tzif_bytes(&[], &[(0, false, "UTC")], "UTC0")).unwrap();
        std::fs::write(dir.join("Aaa_First"), tzif_bytes(&[], &[(0, false, "UTC")], "UTC0")).unwrap();
        std::fs::write(dir.join("Mid/Zone"), tzif_bytes(&[], &[(0, false, "UTC")], "UTC0")).unwrap();
        let saved = std::env::var_os("PYTHONTZPATH");
        std::env::set_var("PYTHONTZPATH", &dir);
        let options = timezone_options();
        match saved {
            Some(v) => std::env::set_var("PYTHONTZPATH", v),
            None => std::env::remove_var("PYTHONTZPATH"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        let keys: Vec<&String> = options.as_object().unwrap().keys().collect();
        assert_eq!(keys[0], "local");
        let zones: Vec<&str> = keys[1..].iter().map(|k| k.as_str()).collect();
        let mut sorted = zones.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(zones, sorted, "zone names must be sorted and unique after 'local'");
        // the host files and chrono-tz's built-in names are merged into one sorted run
        assert!(zones.contains(&"Aaa_First") && zones.contains(&"Mid/Zone") && zones.contains(&"Zzz_Last"));
        assert!(zones.len() > 3, "chrono-tz names must join the host names");
    }
}
