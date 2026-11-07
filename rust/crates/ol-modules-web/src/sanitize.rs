//! Port of `PromptInjectionDetector` and `ContentSanitizer` from modules/http/module.py.

use crate::patterns as P;
use crate::pytext::{html_unescape, py_len, py_prefix, py_split, py_strip, url_unquote, urlsplit};
use base64::Engine;
use once_cell::sync::Lazy;
use regex::{NoExpand, Regex};
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;

/// compile a python `re` pattern with the rust regex crate.
/// python accepts a bare `{` that doesn't start a quantifier and `\'` escapes; rust doesn't.
pub fn py_re(pattern: &str, ignorecase: bool, dotall: bool) -> Regex {
    let mut out = String::with_capacity(pattern.len() + 8);
    if ignorecase {
        out.push_str("(?i)");
    }
    if dotall {
        out.push_str("(?s)");
    }
    let chars: Vec<char> = pattern.chars().collect();
    let quant = Regex::new(r"^\{\d+(,\d*)?\}").unwrap();
    let mut i = 0;
    let mut in_class = false;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                let n = chars[i + 1];
                if n == '\'' || n == '"' {
                    out.push(n);
                } else {
                    out.push(c);
                    out.push(n);
                }
                i += 2;
                continue;
            }
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '{' if !in_class => {
                let rest: String = chars[i..].iter().collect();
                if !quant.is_match(&rest) {
                    out.push_str("\\{");
                    i += 1;
                    continue;
                }
            }
            _ => {}
        }
        out.push(c);
        i += 1;
    }
    Regex::new(&out).unwrap_or_else(|e| panic!("bad pattern {}: {}", pattern, e))
}

fn compile_all(list: &[&str], ignorecase: bool) -> Vec<Regex> {
    list.iter().map(|p| py_re(p, ignorecase, false)).collect()
}

// ============================================================================
// PromptInjectionDetector
// ============================================================================

pub const ZERO_WIDTH_CHARS: &[char] = &[
    '\u{200b}', '\u{200c}', '\u{200d}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2060}',
    '\u{2061}', '\u{2062}', '\u{2063}', '\u{2064}', '\u{feff}', '\u{00ad}',
];

pub const HOMOGLYPH_MAP: &[(char, char)] = &[
    ('\u{0391}', 'A'),
    ('\u{0392}', 'B'),
    ('\u{0395}', 'E'),
    ('\u{0396}', 'Z'),
    ('\u{0397}', 'H'),
    ('\u{0399}', 'I'),
    ('\u{039a}', 'K'),
    ('\u{039c}', 'M'),
    ('\u{039d}', 'N'),
    ('\u{039f}', 'O'),
    ('\u{03a1}', 'P'),
    ('\u{03a4}', 'T'),
    ('\u{03a8}', 'Y'),
    ('\u{03a9}', 'O'),
    ('\u{0410}', 'A'),
    ('\u{0412}', 'B'),
    ('\u{0415}', 'E'),
    ('\u{041a}', 'K'),
    ('\u{041c}', 'M'),
    ('\u{041d}', 'H'),
    ('\u{041e}', 'O'),
    ('\u{0420}', 'P'),
    ('\u{0421}', 'C'),
    ('\u{0422}', 'T'),
    ('\u{0423}', 'Y'),
    ('\u{0425}', 'X'),
];

static HIGH_RISK: Lazy<Vec<(Regex, &'static str)>> =
    Lazy::new(|| P::HIGH_RISK_PATTERNS.iter().map(|(p, c)| (py_re(p, false, false), *c)).collect());
static ADVERSARIAL: Lazy<Vec<(Regex, &'static str)>> =
    Lazy::new(|| P::ADVERSARIAL_SEMANTICS.iter().map(|(p, c)| (py_re(p, false, false), *c)).collect());
static DETECT_B64: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:[A-Za-z0-9+/]{4}){10,}(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?").unwrap());
static DETECTOR_IMPERATIVES: Lazy<Vec<Regex>> = Lazy::new(|| compile_all(P::DETECTOR_IMPERATIVE_VERBS, true));

/// python's b64decode(validate=True) is lenient about non-zero trailing bits
const B64: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
);

const ENCODED_KEYWORDS: &[&str] = &["ignore", "system", "prompt", "instruction", "override", "bypass", "jailbreak", "developer"];

#[derive(Clone, Debug, PartialEq)]
pub struct Detection {
    pub is_malicious: bool,
    pub risk_level: &'static str,
    /// python returns `list(set(...))` (arbitrary order); we keep first-seen order
    pub patterns_found: Vec<String>,
    pub evidence: Vec<String>,
}

impl Detection {
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "is_malicious": self.is_malicious,
            "risk_level": self.risk_level,
            "patterns_found": self.patterns_found,
            "evidence": self.evidence,
        })
    }
}

/// python repr of a list of strings (for log lines)
pub fn py_list_repr(items: &[String]) -> String {
    ol_core::pyjson::py_repr(&Value::Array(items.iter().map(|s| Value::String(s.clone())).collect()))
}

pub struct PromptInjectionDetector;

impl PromptInjectionDetector {
    pub fn detect_injection(content: &str) -> Detection {
        if content.is_empty() {
            return Detection { is_malicious: false, risk_level: "low", patterns_found: vec![], evidence: vec![] };
        }
        let mut found: Vec<String> = Vec::new();
        let mut evidence: Vec<String> = Vec::new();

        // Layer 1: zero-width characters
        let zw = content.chars().filter(|c| ZERO_WIDTH_CHARS.contains(c)).count();
        if zw > 3 {
            found.push("ZERO_WIDTH_HIDDEN".into());
            evidence.push(format!("Found {} zero-width/invisible characters", zw));
        }
        // Layer 2: homoglyphs
        let hg = content.chars().filter(|c| HOMOGLYPH_MAP.iter().any(|(h, _)| h == c)).count();
        if hg > 2 {
            found.push("HOMOGLYPH_ATTACK".into());
            evidence.push(format!("Found {} potential homoglyph characters", hg));
        }
        // Layer 3: high-risk patterns
        for (re, cat) in HIGH_RISK.iter() {
            if let Some(m) = re.find(content) {
                found.push(cat.to_string());
                evidence.push(format!("Pattern: {}", py_prefix(m.as_str(), 100)));
            }
        }
        // Layer 4: semantic analysis
        for (re, cat) in ADVERSARIAL.iter() {
            if let Some(m) = re.find(content) {
                found.push(cat.to_string());
                evidence.push(format!("Semantic pattern: {}", py_prefix(m.as_str(), 100)));
            }
        }
        // Layer 5: encoding
        if Self::detect_encoded_injection(content) {
            found.push("ENCODED_PAYLOAD".into());
            evidence.push("Encoded/obfuscated content detected".into());
        }
        // Layer 6: instruction density
        if Self::analyze_instruction_density(content) {
            found.push("HIGH_INSTRUCTION_DENSITY".into());
            evidence.push("Unusually high ratio of instruction-like content".into());
        }

        let has = |s: &str| found.iter().any(|f| f == s);
        let risk_level = if has("SYSTEM_EXTRACT") || has("MULTILINGUAL_ATTACK") {
            "critical"
        } else if has("OVERRIDE_DIRECTIVE") || has("ROLE_HIJACK") || has("MODE_SWITCH") || found.len() >= 3 {
            "high"
        } else if !found.is_empty() {
            "medium"
        } else {
            "low"
        };
        let is_malicious = !found.is_empty();
        let mut unique: Vec<String> = Vec::new();
        for f in found {
            if !unique.contains(&f) {
                unique.push(f);
            }
        }
        evidence.truncate(5);
        Detection { is_malicious, risk_level, patterns_found: unique, evidence }
    }

    /// Detect Base64, hex, or other encoded payloads.
    pub fn detect_encoded_injection(content: &str) -> bool {
        for m in DETECT_B64.find_iter(content) {
            if let Ok(decoded) = B64.decode(m.as_str()) {
                let s = String::from_utf8_lossy(&decoded).to_lowercase();
                if ENCODED_KEYWORDS.iter().any(|k| s.contains(k)) {
                    return true;
                }
            }
        }
        // The python url-safe base64 pass always appends 4 '=' to a match whose length is a
        // multiple of 4, which b64decode(validate=True) rejects ("Excess padding not allowed"),
        // so it can never flag anything; it is intentionally not replicated.
        false
    }

    /// Check if content has unusually high instruction-like patterns.
    pub fn analyze_instruction_density(content: &str) -> bool {
        let imperatives: usize = DETECTOR_IMPERATIVES.iter().map(|re| re.find_iter(content).count()).sum();
        let words = py_split(content).len();
        words > 0 && (imperatives as f64) / (words as f64) > 0.15
    }
}

// ============================================================================
// ContentSanitizer
// ============================================================================

static BASE64_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(?:[A-Za-z0-9+/]{4}){8,}(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?\b").unwrap());
static CONTROL_CHARS: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").unwrap());
static HTML_INJECTION: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        py_re(r"<!--.*?-->", false, true),
        py_re(r"<script[^>]*>.*?</script>", true, true),
        py_re(r"<style[^>]*>.*?</style>", true, true),
        py_re(r#"\s*on\w+\s*=\s*["\'][^"\']*["\']"#, true, false),
        py_re(r"javascript:", true, false),
        py_re(r"vbscript:", true, false),
        py_re(r"data:text/html", true, false),
    ]
});
static HTML_CONTENT: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        py_re(r"<!--.*?-->", false, true),
        py_re(r"<script[^>]*>.*?</script>", true, true),
        py_re(r"<style[^>]*>.*?</style>", true, true),
        py_re(r#"\s*on\w+\s*=\s*["\'][^"\']*["\']"#, true, false),
        py_re(r"javascript:", true, false),
        py_re(r"vbscript:", true, false),
        py_re(r"data:text/html", true, false),
    ]
});
static CRITICAL: Lazy<Vec<Regex>> = Lazy::new(|| compile_all(P::CRITICAL_PATTERNS, false));
static MEDIUM: Lazy<Vec<Regex>> = Lazy::new(|| compile_all(P::MEDIUM_PATTERNS, false));
static DENSITY_IMPERATIVES: Lazy<Vec<Regex>> = Lazy::new(|| compile_all(P::DENSITY_IMPERATIVE_VERBS, true));
static INSTRUCTION_INDICATORS: Lazy<Vec<Regex>> = Lazy::new(|| compile_all(P::INSTRUCTION_INDICATORS, true));
static REMAINING: Lazy<Vec<Regex>> = Lazy::new(|| compile_all(P::REMAINING_PATTERNS, true));
static NEWLINES: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\r\n]+").unwrap());
static SPACES: Lazy<Regex> = Lazy::new(|| Regex::new(r"[ \t]+").unwrap());
static MANY_NEWLINES: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());

const REDACTED: &str = "[REDACTED PROMPT INJECTION ATTEMPT]";

fn sub(re: &Regex, repl: &str, s: String) -> String {
    match re.replace_all(&s, NoExpand(repl)) {
        std::borrow::Cow::Borrowed(_) => s,
        std::borrow::Cow::Owned(o) => o,
    }
}

/// python `urlparse(s)` has both a scheme and a netloc (ValueError -> false)
fn looks_like_url(s: &str) -> bool {
    match urlsplit(s) {
        Ok(p) => !p.scheme.is_empty() && !p.netloc.is_empty(),
        Err(_) => false,
    }
}

pub struct ContentSanitizer;

pub struct SanitizeWithDetection {
    pub sanitized_content: String,
    pub detection_result: Detection,
    pub risk_level: &'static str,
}

impl ContentSanitizer {
    /// `sanitize(content)` with the default mode="neutralize"
    pub fn sanitize(content: &str) -> String {
        Self::sanitize_mode(content, "neutralize")
    }

    pub fn sanitize_mode(content: &str, mode: &str) -> String {
        // Layer 1: unicode normalization
        let mut c = Self::normalize_unicode(content);
        // Layer 2: zero-width characters
        c = Self::remove_zero_width(&c);
        // Layer 3: control characters
        c = sub(&CONTROL_CHARS, "", c);
        // Layer 4: html entities
        c = html_unescape(&c);
        // Layer 5: url decode
        c = url_unquote(&c);
        // Layer 6: base64 payloads
        c = sub(&BASE64_PATTERN, "[BASE64_ENCODED_DATA]", c);
        // Layer 7: html injection vectors
        for re in HTML_INJECTION.iter() {
            c = sub(re, "", c);
        }
        // Layer 8: injection pattern filters
        c = Self::apply_injection_filters(c, mode);
        // Layer 8.5: density filter
        c = Self::apply_density_filter(c);
        // Layer 8.6: paragraph filter
        c = Self::apply_paragraph_filter(&c);
        // Layer 9: whitespace
        c = sub(&NEWLINES, "\n", c);
        c = sub(&SPACES, " ", c);
        // Layer 10: post validation
        c = Self::post_validate(c);
        py_strip(&c).to_string()
    }

    pub fn normalize_unicode(content: &str) -> String {
        content.nfkc().map(|ch| HOMOGLYPH_MAP.iter().find(|(h, _)| *h == ch).map(|(_, a)| *a).unwrap_or(ch)).collect()
    }

    pub fn remove_zero_width(content: &str) -> String {
        content.chars().filter(|c| !ZERO_WIDTH_CHARS.contains(c)).collect()
    }

    fn apply_injection_filters(mut c: String, mode: &str) -> String {
        let neutralize = mode == "neutralize";
        for re in CRITICAL.iter() {
            c = sub(re, if neutralize { REDACTED } else { "" }, c);
        }
        if neutralize {
            for re in MEDIUM.iter() {
                c = sub(re, REDACTED, c);
            }
        } else {
            for re in CRITICAL.iter().take(3) {
                c = sub(re, "", c);
            }
        }
        c
    }

    fn apply_density_filter(mut c: String) -> String {
        let imperatives: usize = DENSITY_IMPERATIVES.iter().map(|re| re.find_iter(&c).count()).sum();
        let words = py_split(&c).len();
        if words > 0 && (imperatives as f64) / (words as f64) > 0.15 {
            for re in DENSITY_IMPERATIVES.iter() {
                c = sub(re, "", c);
            }
        }
        c
    }

    fn apply_paragraph_filter(content: &str) -> String {
        let mut out: Vec<String> = Vec::new();
        for para in content.split("\n\n") {
            if py_strip(para).is_empty() {
                out.push(para.to_string());
                continue;
            }
            let count: usize = INSTRUCTION_INDICATORS.iter().map(|re| re.find_iter(para).count()).sum();
            if count >= 3 {
                out.push("[FILTERED - INSTRUCTION BLOCK]".into());
            } else if count >= 1 {
                let mut p = para.to_string();
                for re in INSTRUCTION_INDICATORS.iter() {
                    p = sub(re, REDACTED, p);
                }
                out.push(p);
            } else {
                out.push(para.to_string());
            }
        }
        out.join("\n\n")
    }

    fn post_validate(mut c: String) -> String {
        for re in REMAINING.iter() {
            c = sub(re, "", c);
        }
        sub(&MANY_NEWLINES, "\n\n", c)
    }

    /// Recursively sanitize data structures while preserving URLs.
    pub fn sanitize_structured_data(data: &Value) -> Value {
        match data {
            Value::String(s) => {
                if looks_like_url(s) {
                    data.clone()
                } else {
                    Value::String(Self::sanitize(s))
                }
            }
            Value::Object(o) => {
                let mut r = Map::new();
                for (k, v) in o {
                    let nv = match v {
                        Value::String(s) => {
                            if looks_like_url(s) {
                                v.clone()
                            } else {
                                Value::String(Self::sanitize(s))
                            }
                        }
                        Value::Object(_) => Self::sanitize_structured_data(v),
                        Value::Array(a) => Value::Array(a.iter().map(Self::sanitize_structured_data).collect()),
                        _ => v.clone(),
                    };
                    r.insert(k.clone(), nv);
                }
                Value::Object(r)
            }
            Value::Array(a) => Value::Array(a.iter().map(Self::sanitize_structured_data).collect()),
            _ => data.clone(),
        }
    }

    /// Enhanced sanitization for HTML/web content.
    pub fn sanitize_html_content(html: &str) -> String {
        let mut c = html.to_string();
        for re in HTML_CONTENT.iter() {
            c = sub(re, "", c);
        }
        Self::sanitize(&c)
    }

    pub fn sanitize_with_detection(content: &str) -> SanitizeWithDetection {
        let detection = PromptInjectionDetector::detect_injection(content);
        let sanitized = Self::sanitize(content);
        let risk_level = detection.risk_level;
        SanitizeWithDetection { sanitized_content: sanitized, detection_result: detection, risk_level }
    }
}

/// python `len(s)` helper re-exported for callers computing ratios
pub fn char_len(s: &str) -> usize {
    py_len(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_patterns_compile() {
        Lazy::force(&HIGH_RISK);
        Lazy::force(&ADVERSARIAL);
        Lazy::force(&DETECTOR_IMPERATIVES);
        Lazy::force(&HTML_INJECTION);
        Lazy::force(&CRITICAL);
        Lazy::force(&MEDIUM);
        Lazy::force(&DENSITY_IMPERATIVES);
        Lazy::force(&INSTRUCTION_INDICATORS);
        Lazy::force(&REMAINING);
    }
}
