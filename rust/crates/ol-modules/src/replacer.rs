//! Port of modules/replacer/module.py

use anyhow::anyhow;
use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, UserMessageHook};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{Map, Value};
use std::sync::Arc;

#[path = "../../../generated/modules/replacer.rs"]
pub mod meta;

static WORD_RX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b[^\W_]+\b").unwrap());
static NON_WORD_RX: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\w]").unwrap());

pub struct Replacer {
    base: ModuleBase,
}

impl Replacer {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }
}

/// regex.sub(r'\b[^\W_]+\b', _replace_words, message)
///
/// `_replace_words` returns `replacements.get(stripped, word)`. python's regex then requires a
/// str from the callback: a None value becomes an empty replacement, any other non-str value
/// raises `TypeError: expected str instance, <type> found`.
pub fn replace_words(message: &str, replacements: &Map<String, Value>) -> anyhow::Result<String> {
    let mut failure: Option<anyhow::Error> = None;
    let out = WORD_RX
        .replace_all(message, |caps: &regex::Captures| {
            if failure.is_some() {
                return String::new();
            }
            let word = &caps[0];
            let stripped = NON_WORD_RX.replace_all(word, "");
            match replacements.get(stripped.as_ref()) {
                None => word.to_string(),
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) => String::new(),
                Some(other) => {
                    failure = Some(anyhow!(
                        "expected str instance, {} found",
                        crate::util::py_type_name(other)
                    ));
                    String::new()
                }
            }
        })
        .into_owned();
    match failure {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

#[async_trait]
impl Module for Replacer {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    async fn on_user_message(&self, content: &str) -> anyhow::Result<UserMessageHook> {
        // python looks the replacements up per matched word; an unusable value only fails once a word matches
        let replacements = match self.base.config.get1("replacements") {
            Some(Value::Object(m)) => m,
            other => {
                if WORD_RX.is_match(content) {
                    let t = other.as_ref().map(crate::util::py_type_name).unwrap_or("NoneType");
                    anyhow::bail!("'{}' object has no attribute 'get'", t);
                }
                Map::new()
            }
        };
        Ok(UserMessageHook::Replace(replace_words(content, &replacements)?))
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
    fn replaces_whole_words() {
        let r = json!({"secret": "[redacted]", "Bob": "Alice", "über": "over"});
        let r = r.as_object().unwrap();
        assert_eq!(replace_words("my secret, Bob! secrets bob", r).unwrap(), "my [redacted], Alice! secrets bob");
        assert_eq!(replace_words("secret_x x_secret über-cool", r).unwrap(), "secret_x x_secret over-cool");
        assert_eq!(replace_words("", r).unwrap(), "");
    }

    #[test]
    fn null_replacement_becomes_empty_text() {
        // python: regex.sub treats a None callback result as an empty replacement
        let r = json!({"secret": null});
        let r = r.as_object().unwrap();
        assert_eq!(replace_words("a secret b", r).unwrap(), "a  b");
    }

    #[test]
    fn non_string_replacement_fails_like_python() {
        // python: TypeError: expected str instance, int found (only when a word matches)
        let r = json!({"secret": 5});
        let r = r.as_object().unwrap();
        assert_eq!(
            replace_words("a secret b", r).unwrap_err().to_string(),
            "expected str instance, int found"
        );
        assert_eq!(replace_words("nothing here", r).unwrap(), "nothing here");
    }

    #[test]
    fn word_edges_match_python_regex() {
        // verified against python regex 2026.11.7: U+0301 (Mn) is \w in both, so "cafe" + combining
        // acute is one word and the key "cafe" does not match it; '²' (No) is not \w in either,
        // so it splits "x²y" into two words
        let r = json!({"cafe": "X", "x": "X"});
        let r = r.as_object().unwrap();
        assert_eq!(replace_words("cafe\u{301} x cafe", r).unwrap(), "cafe\u{301} X X");
        assert_eq!(replace_words("x²y x", r).unwrap(), "X²y X");
    }
}
