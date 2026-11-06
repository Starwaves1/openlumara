//! Port of modules/token_threshold/module.py: warns the AI when the current chat has used
//! more than `warning_threshold` (a percentage setting, default 0.8) of `api.max_context`.

use anyhow::anyhow;
use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase};
use serde_json::Value;
use std::sync::Arc;

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

/// python `int(value)` for the `api.max_context` config value
fn py_int(v: &Value) -> anyhow::Result<i64> {
    match v {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Ok(i),
            None => Ok(n.as_f64().unwrap_or(0.0).trunc() as i64),
        },
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| anyhow!("invalid literal for int() with base 10: '{}'", s)),
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
pub fn warning(current: f64, max_tokens: i64, threshold: f64) -> Option<String> {
    if max_tokens <= 0 {
        return None;
    }
    let used_percentage = (current / max_tokens as f64) * 100.0;
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
        let current = ch.chat.get("token_usage").unwrap_or(Value::from(0));
        let max_tokens = match ol_core::config::get(&["api", "max_context"]) {
            Some(v) => py_int(&v)?,
            None => py_int(&Value::Null)?,
        };
        // avoid division by zero
        if max_tokens <= 0 {
            return Ok(None);
        }
        let current = py_number_for_division(&current)?;
        let threshold = py_threshold(self.base.config.get1("warning_threshold"))?;
        Ok(warning(current, max_tokens, threshold))
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
        assert!(warning(100.0, 1000, 0.8).is_none());
        assert!(warning(850.0, 0, 0.8).is_none());
        let w = warning(850.0, 1000, 0.8).unwrap();
        assert_eq!(
            w,
            "WARNING: Approaching token limit! You have used 85.0% of the allowed tokens. 15.0% remaining. Warn the user!! The user might want to use `/compress` to summarize the chat so far (the user won't lose chat history, but the AI won't see the messages anymore). Or the user can use `/new` to start a new chat."
        );
    }

    #[test]
    fn python_int_conversion() {
        assert_eq!(py_int(&json!(8192)).unwrap(), 8192);
        assert_eq!(py_int(&json!(8192.9)).unwrap(), 8192);
        assert_eq!(py_int(&json!(" 4096 ")).unwrap(), 4096);
        assert_eq!(py_int(&json!("abc")).unwrap_err().to_string(), "invalid literal for int() with base 10: 'abc'");
        assert_eq!(
            py_int(&Value::Null).unwrap_err().to_string(),
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
        );
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
        let w = warning(current, 4000, threshold).unwrap();
        assert!(w.starts_with("WARNING: Approaching token limit! You have used 25.0% of the allowed tokens. 75.0% remaining."));
    }
}
