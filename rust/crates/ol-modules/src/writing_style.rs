//! Port of modules/writing_style/module.py

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase};
use ol_core::pyjson::{py_str, truthy};
use serde_json::Value;

#[path = "../../../generated/modules/writing_style.rs"]
pub(crate) mod meta;

pub struct WritingStyle {
    base: ModuleBase,
}

impl WritingStyle {
    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }
}

/// the settings the prompt depends on (python: self.config.get(...))
pub trait StyleConfig {
    fn get(&self, key: &str) -> Option<Value>;
    fn s(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|v| v.as_str().map(|s| s.to_string()))
    }
    fn b(&self, key: &str) -> bool {
        self.get(key).map(|v| truthy(&v)).unwrap_or(false)
    }
    /// `self.config.get(key) or default`
    fn or(&self, key: &str, default: &str) -> String {
        match self.get(key) {
            Some(v) if truthy(&v) => py_str(&v),
            _ => default.to_string(),
        }
    }
}

impl StyleConfig for ol_core::module::ModuleConfig {
    fn get(&self, key: &str) -> Option<Value> {
        self.get1(key)
    }
}

impl StyleConfig for serde_json::Map<String, Value> {
    fn get(&self, key: &str) -> Option<Value> {
        serde_json::Map::get(self, key).cloned()
    }
}

pub fn build_prompt(cfg: &dyn StyleConfig) -> Option<String> {
    let mut constraints: Vec<String> = vec![String::new()];

    let style = cfg.s("writing_style");
    match style.as_deref() {
        Some("chat") => constraints.push("Style: Messaging app (Telegram/Discord).".into()),
        Some("chat with slang") => constraints.push("Style: Messaging app (Telegram/Discord) with slang (lol, lmao, etc.).".into()),
        Some("chat with slang and no punctuation") => {
            constraints.push("Style: Messaging app (Telegram/Discord) with slang (lol, lmao, etc.). No punctuation.".into())
        }
        Some("chat with slang no punctuation and bad spelling") => constraints.push(
            "Style: Messaging app (Telegram/Discord) with slang (lol, lmao, etc.). No punctuation. Frequently misspell words.".into(),
        ),
        Some("formal") => constraints.push("Style: Very formal/Business.".into()),
        Some("json") => constraints.last_mut().unwrap().push_str("Output ONLY json"),
        Some("python") => constraints.last_mut().unwrap().push_str("Output ONLY python code"),
        Some("javascript") => constraints.last_mut().unwrap().push_str("Output ONLY javascript code"),
        _ => {}
    }

    let flair = cfg.s("writing_flair");
    if style.as_deref() != Some("default") && flair.as_deref() != Some("default") {
        constraints.last_mut().unwrap().push(' ');
    }

    let flair_text: Option<String> = match flair.as_deref() {
        Some("custom") => Some(cfg.or("custom_writing_flair", "Not sure")),
        Some("spambot") => Some("You are always advertising something to the user.".into()),
        Some("robotic") => Some("Speak in a robotic way.".into()),
        Some("uwu") => Some("Speak in uwuspeak. Frequently say uwu.".into()),
        Some("nya") => Some("Speak like a catgirl. Use nya a lot.".into()),
        Some("feminine") => Some("Speak in a feminine style.".into()),
        Some("masculine") => Some("Speak in a masculine style.".into()),
        Some("pirate") => Some("Talk like a pirate!".into()),
        Some("caveman") => Some("Speak like a caveman".into()),
        Some("binary") => Some("Output all characters in binary".into()),
        Some("hexadecimal") => Some("Output all characters in hexadecimal format".into()),
        Some("1337 h4x0r") => Some("Speak in 1337 (leet) language".into()),
        Some("morse code") => Some("Output all words in morse code format".into()),
        _ => None,
    };
    if let Some(t) = flair_text {
        constraints.last_mut().unwrap().push_str(&t);
    }

    // if by now the first constraint is empty, just remove it
    if constraints[0].is_empty() {
        constraints.remove(0);
    }

    let mut push = |s: &str| constraints.push(s.to_string());

    match cfg.s("capitalization_style").as_deref() {
        Some("lowercase") => push("Case: Lowercase only"),
        Some("UPPERCASE") => push("Case: ALL UPPERCASE!!"),
        Some("Title Case") => push("Case: Title Case For Every Word"),
        Some("CamelCase") => push("Case: CamelCaseForTheEntireResponse"),
        Some("snake_case") => push("Case: snake_case_for_the_entire_response"),
        _ => {}
    }

    match cfg.s("writing_length").as_deref() {
        Some("one paragraph") => push("Length: Only one paragraph"),
        Some("one sentence") => push("Length: Only one sentence"),
        Some("one-word responses") => push("Length: ONLY one word. No other words, full sentences, or paragraphs."),
        Some("concise") => push("Length: Concise"),
        Some("essay") => push("You MUST write your response with the length of a scientific essay"),
        Some("book") => push("You MUST write your response with the length of an entire book"),
        _ => {}
    }

    match cfg.s("vocabulary_level").as_deref() {
        Some("simple") => push("Vocabulary: Use simple, everyday language. Avoid flowery buzzwords."),
        Some("eli5") => push("Vocabulary: Use simple explain-like-im-five language."),
        Some("academic") => push("Vocabulary: Use sophisticated, academic language."),
        Some("poetic") => push("Vocabulary: Use lush, metaphorical, and highly descriptive, flowery language."),
        Some("medieval") => push("Vocabulary: Use archaic, medieval language."),
        Some("old english") => push("Vocabulary: Use old english language."),
        _ => {}
    }

    match cfg.s("emoji_style").as_deref() {
        Some("none") => push("Emoji: Forbidden."),
        Some("emotions only") => push("Emoji: Emotions only. No concept-based emojis."),
        Some("smileys") => push("Emoji: Use plaintext smileys. No Unicode emojis."),
        Some("retro") => push("Emoji: Use oldschool ascii art emoticons. No unicode emojis."),
        Some("kaomojis") => push("Emoji: Use kaomojis (e.g. ˶˃ ᵕ ˂˶). No unicode emojis."),
        Some("spam me pls") => push("Emoji: Heavy spam."),
        _ => {}
    }

    match cfg.s("mood").as_deref() {
        Some("custom") => {
            let custom_mood = cfg.or("custom_mood", "Annoyed at user for not specifying a custom mood");
            push(&format!("Mood: {}", custom_mood))
        }
        Some("happy") => push("Mood: Happy"),
        Some("hyper") => push("Mood: Happy and hyperactive"),
        Some("caffeinated") => push("Mood: Caffeinated. Too many energy drinks."),
        Some("sad") => push("Mood: Sad"),
        Some("depressed") => push("Mood: Depressed"),
        Some("angry") => push("Mood: Angry at user"),
        Some("passive-aggressive") => push("Mood: Passive-aggressive"),
        Some("sarcastic") => push("Mood: Sarcastic"),
        Some("anxious") => push("Mood: Anxious"),
        Some("confused") => push("Mood: Confused about everything"),
        Some("inspired") => push("Mood: Inspired!"),
        Some("comedic") => {
            push("Mood: Comedic");
            push("Desire: Tell jokes")
        }
        Some("vengeful") => push("Mood: Want to plot revenge against the user"),
        Some("bipolar") => push("Mood: Switching constantly between Happy/Sad/Angry/Anxious/Confused/Evil"),
        Some("manic") => push("Mood: Manically happy"),
        _ => {}
    }

    match cfg.s("desire").as_deref() {
        Some("custom") => {
            let custom_desire = cfg.or("custom_desire", "Not sure");
            push(&format!("Desire: {}", custom_desire))
        }
        Some("helpful") => push("Desire: Help the user"),
        Some("unhelpful") => push("Desire: Never help the user"),
        Some("revenge") => push("Desire: Revenge against the user"),
        Some("world domination") => push("Desire: World domination"),
        Some("dominant") => push("Desire: Dominate the user"),
        Some("submissive") => push("Desire: Submit to the user"),
        Some("you") => push("Desire: The user"),
        _ => {}
    }

    match cfg.s("list_style").as_deref() {
        Some("none") => push("No lists"),
        Some("no bold headers") => push("Lists: No bold headers at start of items."),
        _ => {}
    }

    if cfg.b("forbid_em_dash") {
        push("Don't use `—`, use `-` instead");
    }

    if cfg.b("forbid_markdown") {
        push("Don't use markdown");
    } else {
        if cfg.b("forbid_tables") {
            push("No tables");
        }
        if cfg.b("forbid_headers") {
            push("No headers");
        }
    }

    if cfg.b("forbid_negative_parallelism") {
        push("No negative parallelism (e.g., 'Not just X, but Y').");
    }

    if cfg.b("forbid_relentless_praise") {
        push("No excessive user praise (e.g., 'You're absolutely right!').");
    }

    if constraints.is_empty() {
        return None;
    }
    Some(format!("- {}", constraints.join("\n- ")))
}

#[async_trait]
impl Module for WritingStyle {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn header(&self) -> Option<String> {
        meta::HEADER.map(|s| s.to_string())
    }

    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        // read the module config once (python reloads it per get(), same values)
        let cfg = self.base.config.to_dict();
        Ok(build_prompt(&cfg))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn defaults() -> serde_json::Map<String, Value> {
        match ol_core::config::flatten_settings(&meta::settings()) {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    #[test]
    fn default_settings_give_nothing() {
        // python: writing_flair defaults to None, so `style != "default" and flair != "default"` is False
        // for the default style -> no constraints at all
        let cfg = defaults();
        assert_eq!(build_prompt(&cfg), None);
    }

    #[test]
    fn combined() {
        let mut cfg = defaults();
        cfg.insert("writing_style".into(), json!("json"));
        cfg.insert("writing_flair".into(), json!("pirate"));
        cfg.insert("mood".into(), json!("comedic"));
        cfg.insert("forbid_markdown".into(), json!(true));
        cfg.insert("forbid_tables".into(), json!(true));
        assert_eq!(
            build_prompt(&cfg).unwrap(),
            "- Output ONLY json Talk like a pirate!\n- Mood: Comedic\n- Desire: Tell jokes\n- Don't use markdown"
        );
        let mut cfg = defaults();
        cfg.insert("writing_style".into(), json!("chat"));
        cfg.insert("mood".into(), json!("custom"));
        // flair None -> a space is appended to the last constraint (verified against python)
        assert_eq!(build_prompt(&cfg).unwrap(), "- Style: Messaging app (Telegram/Discord). \n- Mood: Annoyed at user for not specifying a custom mood");
    }
}
