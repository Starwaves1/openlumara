//! Port of modules/token_threshold/module.py: warns the AI when the current chat has used
//! more than `warning_threshold` (a percentage setting, default 0.8) of `api.max_context`.

use anyhow::anyhow;
use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase};
use serde_json::Value;
use std::sync::Arc;
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

#[path = "../../../generated/modules/token_threshold.rs"]
pub mod meta;

pub struct TokenThreshold {
    base: ModuleBase,
}

impl TokenThreshold {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }
}

/// python `int` value of `api.max_context`: exact when it fits an i64, otherwise the nearest f64
/// (python keeps arbitrary precision; only the sign and the magnitude matter for the warning)
#[derive(Clone, Copy, Debug, PartialEq)]
enum PyInt {
    Exact(i64),
    Huge(f64),
}

impl PyInt {
    fn as_f64(self) -> f64 {
        match self {
            PyInt::Exact(i) => i as f64,
            PyInt::Huge(f) => f,
        }
    }
}

/// python `int(float)`: truncates toward zero, keeping the exact magnitude beyond the i64 range.
/// NaN and infinity fail the way CPython does (ValueError / OverflowError).
fn float_to_py_int(f: f64) -> anyhow::Result<PyInt> {
    if f.is_nan() {
        return Err(anyhow!("cannot convert float NaN to integer"));
    }
    if f.is_infinite() {
        return Err(anyhow!("cannot convert float infinity to integer"));
    }
    let t = f.trunc();
    if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&t) {
        Ok(PyInt::Exact(t as i64))
    } else {
        Ok(PyInt::Huge(t))
    }
}

/// the decimal value of a Unicode decimal digit (general category Nd), as python's int(str) reads
/// it. Unicode encodes every Nd run as ascending groups of ten starting at zero, so the value is
/// the count of Nd characters just before it, mod 10.
fn unicode_decimal_digit(c: char) -> Option<u8> {
    if c.is_ascii() || c.general_category() != GeneralCategory::DecimalNumber {
        return None;
    }
    let mut before = 0u32;
    let mut cp = c as u32;
    while cp > 0 {
        cp -= 1;
        match char::from_u32(cp) {
            Some(p) if p.general_category() == GeneralCategory::DecimalNumber => before += 1,
            _ => break,
        }
    }
    Some((before % 10) as u8)
}

/// CPython's default int/str conversion digit limit (sys.int_info.default_max_str_digits)
const PY_MAX_STR_DIGITS: usize = 4300;

/// python `int(str)` in base 10: surrounding whitespace, an optional sign, then digits in which a
/// single `_` may sit between two digits (`int("4_096") == 4096`). Any Unicode decimal digit
/// counts as a digit (`int("٤٠96") == 4096`). `Err` carries python's ValueError text.
fn parse_py_int_literal(s: &str) -> Result<PyInt, String> {
    let invalid = || format!("invalid literal for int() with base 10: {}", crate::util::repr_str(s));
    // python maps every decimal digit to its ASCII form before parsing; any other non-ASCII
    // character makes the literal invalid
    let mapped: String = s
        .trim()
        .chars()
        .map(|c| match unicode_decimal_digit(c) {
            Some(d) => Some(char::from(b'0' + d)),
            None if c.is_ascii() => Some(c),
            None => None,
        })
        .collect::<Option<String>>()
        .ok_or_else(invalid)?;
    let t = mapped.as_str();
    let (sign, digits) = match t.as_bytes().first() {
        Some(b'-') => ("-", &t[1..]),
        Some(b'+') => ("", &t[1..]),
        _ => ("", t),
    };
    let well_formed = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__");
    if !well_formed {
        return Err(invalid());
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    if clean.len() > PY_MAX_STR_DIGITS {
        return Err(format!(
            "Exceeds the limit ({} digits) for integer string conversion: value has {} digits; use sys.set_int_max_str_digits() to increase the limit",
            PY_MAX_STR_DIGITS,
            clean.len()
        ));
    }
    let signed = format!("{}{}", sign, clean);
    match signed.parse::<i64>() {
        Ok(i) => Ok(PyInt::Exact(i)),
        Err(_) => signed.parse::<f64>().map(PyInt::Huge).map_err(|_| invalid()),
    }
}

/// python `int(value)` for the `api.max_context` config value
fn py_int(v: &Value) -> anyhow::Result<PyInt> {
    match v {
        Value::Bool(b) => Ok(PyInt::Exact(i64::from(*b))),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Ok(PyInt::Exact(i)),
            None => float_to_py_int(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => parse_py_int_literal(s).map_err(|msg| anyhow!("{}", msg)),
        other => Err(anyhow!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            crate::util::py_type_name(other)
        )),
    }
}

/// python `value / max_tokens` for the token usage (only int/float/bool are numbers)
fn py_number_for_division(v: &Value) -> anyhow::Result<f64> {
    match v {
        Value::Bool(b) => Ok(f64::from(u8::from(*b))),
        Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0)),
        other => Err(anyhow!(
            "unsupported operand type(s) for /: '{}' and 'int'",
            crate::util::py_type_name(other)
        )),
    }
}

/// python `self.config.get("warning_threshold") * 100` followed by `used >= ...`.
/// a number (or bool) is fine; the other types fail at `*` (None, dict) or at `>=` (str, list).
fn py_threshold(v: Option<Value>) -> anyhow::Result<f64> {
    match v {
        Some(Value::Bool(b)) => Ok(f64::from(u8::from(b))),
        Some(Value::Number(n)) => Ok(n.as_f64().unwrap_or(0.0)),
        None | Some(Value::Null) => Err(anyhow!("unsupported operand type(s) for *: 'NoneType' and 'int'")),
        Some(other @ Value::Object(_)) => Err(anyhow!(
            "unsupported operand type(s) for *: '{}' and 'int'",
            crate::util::py_type_name(&other)
        )),
        Some(other) => Err(anyhow!(
            "'>=' not supported between instances of 'float' and '{}'",
            crate::util::py_type_name(&other)
        )),
    }
}

/// the warning text for a given usage (None when below the threshold)
pub fn warning(current: f64, max_tokens: f64, threshold: f64) -> Option<String> {
    if max_tokens <= 0.0 {
        return None;
    }
    let used_percentage = (current / max_tokens) * 100.0;
    let warning_threshold_percent = threshold * 100.0;
    if used_percentage >= warning_threshold_percent {
        let remaining_percentage = 100.0 - used_percentage;
        Some(format!(
            "WARNING: Approaching token limit! You have used {:.1}% of the allowed tokens. {:.1}% remaining. Warn the user!! The user might want to use `/compress` to summarize the chat so far (the user won't lose chat history, but the AI won't see the messages anymore). Or the user can use `/new` to start a new chat.",
            used_percentage, remaining_percentage
        ))
    } else {
        None
    }
}

#[async_trait]
impl Module for TokenThreshold {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    async fn on_end_prompt(&self) -> anyhow::Result<Option<String>> {
        // python: no channel (or no context) -> None
        let Some(ch) = self.base.channel() else { return Ok(None) };
        // python chat.get() raises when no chat is loaded; the hook fails with that error
        if ch.chat.current().is_none() {
            return Err(anyhow!("No chat is currently loaded!"));
        }
        let current = ch.chat.get("token_usage").unwrap_or(Value::from(0));
        let max_tokens = match ol_core::config::get(&["api", "max_context"]) {
            Some(v) => py_int(&v)?,
            None => py_int(&Value::Null)?,
        };
        // avoid division by zero
        if max_tokens.as_f64() <= 0.0 {
            return Ok(None);
        }
        let current = py_number_for_division(&current)?;
        let threshold = py_threshold(self.base.config.get1("warning_threshold"))?;
        Ok(warning(current, max_tokens.as_f64(), threshold))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn warning_text() {
        assert!(warning(100.0, 1000.0, 0.8).is_none());
        assert!(warning(850.0, 0.0, 0.8).is_none());
        let w = warning(850.0, 1000.0, 0.8).unwrap();
        assert_eq!(
            w,
            "WARNING: Approaching token limit! You have used 85.0% of the allowed tokens. 15.0% remaining. Warn the user!! The user might want to use `/compress` to summarize the chat so far (the user won't lose chat history, but the AI won't see the messages anymore). Or the user can use `/new` to start a new chat."
        );
    }

    #[test]
    fn python_int_conversion() {
        assert_eq!(py_int(&json!(8192)).unwrap(), PyInt::Exact(8192));
        assert_eq!(py_int(&json!(8192.9)).unwrap(), PyInt::Exact(8192));
        assert_eq!(py_int(&json!(" 4096 ")).unwrap(), PyInt::Exact(4096));
        assert_eq!(py_int(&json!("4_096")).unwrap(), PyInt::Exact(4096));
        assert_eq!(py_int(&json!("-1_0")).unwrap(), PyInt::Exact(-10));
        assert_eq!(py_int(&json!("abc")).unwrap_err().to_string(), "invalid literal for int() with base 10: 'abc'");
        assert_eq!(py_int(&json!("4__096")).unwrap_err().to_string(), "invalid literal for int() with base 10: '4__096'");
        assert_eq!(py_int(&json!("_4096")).unwrap_err().to_string(), "invalid literal for int() with base 10: '_4096'");
        assert_eq!(py_int(&json!("it's")).unwrap_err().to_string(), "invalid literal for int() with base 10: \"it's\"");
        assert_eq!(
            py_int(&Value::Null).unwrap_err().to_string(),
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
        );
    }

    #[test]
    fn unicode_decimal_digits_parse_like_python() {
        // python: int('٤٠٩٦') == 4096 (Arabic-Indic digits), int('４０９６') == 4096 (fullwidth)
        assert_eq!(py_int(&json!("٤٠٩٦")).unwrap(), PyInt::Exact(4096));
        assert_eq!(py_int(&json!("４０９６")).unwrap(), PyInt::Exact(4096));
        assert_eq!(py_int(&json!(" -٤_٠٩٦ ")).unwrap(), PyInt::Exact(-4096));
        // a non-digit non-ASCII character is still an invalid literal
        assert_eq!(
            py_int(&json!("٤٠é")).unwrap_err().to_string(),
            "invalid literal for int() with base 10: '٤٠é'"
        );
    }

    #[test]
    fn non_finite_floats_fail_like_python_int() {
        // python: int(float('nan')) -> ValueError, int(float('inf')) -> OverflowError
        assert_eq!(
            float_to_py_int(f64::NAN).unwrap_err().to_string(),
            "cannot convert float NaN to integer"
        );
        assert_eq!(
            float_to_py_int(f64::INFINITY).unwrap_err().to_string(),
            "cannot convert float infinity to integer"
        );
        assert_eq!(
            float_to_py_int(f64::NEG_INFINITY).unwrap_err().to_string(),
            "cannot convert float infinity to integer"
        );
        assert_eq!(float_to_py_int(-7.9).unwrap(), PyInt::Exact(-7));
    }

    #[test]
    fn operand_errors_match_python() {
        assert_eq!(py_threshold(Some(json!(0.8))).unwrap(), 0.8);
        assert_eq!(
            py_threshold(None).unwrap_err().to_string(),
            "unsupported operand type(s) for *: 'NoneType' and 'int'"
        );
        assert_eq!(
            py_threshold(Some(json!("0.8"))).unwrap_err().to_string(),
            "'>=' not supported between instances of 'float' and 'str'"
        );
        assert_eq!(
            py_number_for_division(&json!("12")).unwrap_err().to_string(),
            "unsupported operand type(s) for /: 'str' and 'int'"
        );
    }

    #[test]
    fn warning_from_python_style_inputs() {
        // 1000 of 4000 tokens used with a 0.2 threshold -> 25.0% used, 75.0% remaining
        let current = py_number_for_division(&json!(1000)).unwrap();
        let threshold = py_threshold(Some(json!(0.2))).unwrap();
        let w = warning(current, 4000.0, threshold).unwrap();
        assert!(w.starts_with("WARNING: Approaching token limit! You have used 25.0% of the allowed tokens. 75.0% remaining."));
    }

    #[test]
    fn huge_max_context_keeps_its_magnitude() {
        // python int() keeps the exact value: 1e30 is not saturated to i64::MAX
        assert_eq!(py_int(&json!(1e30)).unwrap(), PyInt::Huge(1e30));
        assert_eq!(py_int(&json!("99999999999999999999")).unwrap(), PyInt::Huge(1e20));
        let max = py_int(&json!(1e30)).unwrap().as_f64();
        assert!(warning(1_000_000.0, max, 0.8).is_none());
    }

    #[test]
    fn oversized_digit_string_hits_python_limit() {
        let long = "1".repeat(4301);
        assert_eq!(
            py_int(&json!(long)).unwrap_err().to_string(),
            "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; use sys.set_int_max_str_digits() to increase the limit"
        );
    }
}
