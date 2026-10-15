//! Port of modules/replacer/module.py

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, UserMessageHook};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{Map, Value};

#[path = "../../../generated/modules/replacer.rs"]
pub(crate) mod meta;

static WORD_RX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b[^\W_]+\b").unwrap());
static NON_WORD_RX: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\w]").unwrap());

pub struct Replacer {
    base: ModuleBase,
}

impl Replacer {
    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }
}

/// regex.sub(r'\b[^\W_]+\b', _replace_words, message)
pub fn replace_words(message: &str, replacements: &Map<String, Value>) -> String {
    WORD_RX
        .replace_all(message, |caps: &regex::Captures| {
            let word = &caps[0];
            let stripped = NON_WORD_RX.replace_all(word, "");
            match replacements.get(stripped.as_ref()) {
                Some(v) => ol_core::pyjson::py_str(v),
                None => word.to_string(),
            }
        })
        .into_owned()
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
        Ok(UserMessageHook::Replace(replace_words(content, &replacements)))
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
        assert_eq!(replace_words("my secret, Bob! secrets bob", r), "my [redacted], Alice! secrets bob");
        assert_eq!(replace_words("secret_x x_secret über-cool", r), "secret_x x_secret over-cool");
        assert_eq!(replace_words("", r), "");
    }
}
