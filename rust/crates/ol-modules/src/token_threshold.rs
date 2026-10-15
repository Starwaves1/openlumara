//! Port of modules/token_threshold/module.py

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase};

#[path = "../../../generated/modules/token_threshold.rs"]
pub(crate) mod meta;

pub struct TokenThreshold {
    base: ModuleBase,
}

impl TokenThreshold {
    pub fn new(base: ModuleBase) -> Self {
        Self { base }
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
        let Some(ch) = self.base.channel() else { return Ok(None) };
        let current = ch.chat.get("token_usage").and_then(|v| ol_core::pyjson::as_f64(&v)).unwrap_or(0.0);
        let max_tokens = match ol_core::config::get(&["api", "max_context"]) {
            Some(v) => match ol_core::pyjson::as_f64(&v) {
                Some(f) => f.trunc() as i64,
                None => anyhow::bail!(
                    "int() argument must be a string, a bytes-like object or a real number, not '{}'",
                    crate::util::py_type_name(&v)
                ),
            },
            None => anyhow::bail!("int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"),
        };
        let Some(threshold) = self.base.config.get_f64("warning_threshold") else {
            if max_tokens <= 0 {
                return Ok(None);
            }
            anyhow::bail!("unsupported operand type(s) for *: 'NoneType' and 'int'");
        };
        Ok(warning(current, max_tokens, threshold))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_text() {
        assert!(warning(100.0, 1000, 0.8).is_none());
        let w = warning(850.0, 1000, 0.8).unwrap();
        assert!(w.starts_with("WARNING: Approaching token limit! You have used 85.0% of the allowed tokens. 15.0% remaining."));
        assert!(warning(850.0, 0, 0.8).is_none());
    }
}
