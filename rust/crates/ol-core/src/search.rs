//! Port of core/search.py: BM25 ranking with prefix matching and phrase boost, plus snippets.

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

// words are runs of letters OR runs of digits (decimals kept: "3.8"), nothing else.
static NORMALIZE_RX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d+(?:\.\d+)?|[^\W\d_]+").unwrap());

/// splits raw text into lowercase word tokens, so 'Vitamins!' and 'vitamins' match
pub fn normalize_words(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    NORMALIZE_RX.find_iter(&lower).map(|m| m.as_str().to_string()).collect()
}

fn find_chars(hay: &[char], needle: &[char], start: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(start.min(hay.len()));
    }
    if needle.len() > hay.len() {
        return None;
    }
    (start..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()] == *needle)
}

/// returns up to max_snippets excerpts of text surrounding matches of the query
pub fn make_snippets(text: &str, query: &str, max_snippets: usize, radius: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let low: Vec<char> = text.to_lowercase().chars().collect();
    // lowercasing can change length for some scripts; fall back to per-char lowercase
    let low: Vec<char> = if low.len() == chars.len() {
        low
    } else {
        chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect()
    };
    let mut candidates = vec![query.trim().to_lowercase()];
    candidates.extend(normalize_words(query));
    let low_str: String = low.iter().collect();
    let term = candidates.into_iter().find(|c| !c.is_empty() && low_str.contains(c.as_str())).unwrap_or_default();
    if term.is_empty() {
        return vec![];
    }
    let term_chars: Vec<char> = term.chars().collect();
    let mut snippets = Vec::new();
    let mut start = 0;
    while snippets.len() < max_snippets {
        let Some(pos) = find_chars(&low, &term_chars, start) else { break };
        let end = chars.len().min(pos + term_chars.len() + radius);
        let from = pos.saturating_sub(radius);
        let chunk: String = chars[from..end].iter().collect::<String>().replace('\n', " ");
        let chunk = chunk.trim();
        let mut s = String::new();
        if pos > radius {
            s.push_str("...");
        }
        s.push_str(chunk);
        if end < chars.len() {
            s.push_str("...");
        }
        snippets.push(s);
        start = end;
    }
    snippets
}

/// normalizes any entry (string, dict, or whatever) into a {field: text} dict
fn extract(entry: &Value, field_weights: Option<&IndexMap<String, f64>>) -> IndexMap<String, String> {
    let mut fields = IndexMap::new();
    let Value::Object(obj) = entry else {
        fields.insert("text".to_string(), crate::pyjson::py_str(entry));
        return fields;
    };
    for (key, value) in obj {
        if let Some(w) = field_weights {
            if !w.contains_key(key) {
                continue;
            }
        }
        match value {
            Value::String(s) => {
                fields.insert(key.clone(), s.clone());
            }
            Value::Array(a) => {
                let parts: Vec<String> = a
                    .iter()
                    .filter(|v| v.is_string() || v.is_number())
                    .map(crate::pyjson::py_str)
                    .collect();
                if !parts.is_empty() {
                    fields.insert(key.clone(), parts.join(" "));
                }
            }
            _ => {}
        }
    }
    fields
}

type Tally = Arc<IndexMap<String, usize>>;

struct WordCache {
    map: HashMap<String, Tally>,
    words: usize,
}

static WORD_CACHE: Lazy<Mutex<WordCache>> = Lazy::new(|| Mutex::new(WordCache { map: HashMap::new(), words: 0 }));
const WORD_CACHE_BUDGET: usize = 300_000;

/// returns a {word: count} tally for the text, reusing a cached one if possible
fn count_words(text: &str) -> Tally {
    let mut cache = WORD_CACHE.lock();
    if let Some(t) = cache.map.get(text) {
        return t.clone();
    }
    let mut counts: IndexMap<String, usize> = IndexMap::new();
    for w in normalize_words(text) {
        *counts.entry(w).or_insert(0) += 1;
    }
    if cache.words + counts.len() > WORD_CACHE_BUDGET {
        cache.map.clear();
        cache.words = 0;
    }
    let t = Arc::new(counts);
    cache.words += t.len();
    cache.map.insert(text.to_string(), t.clone());
    t
}

/// scores each entry against the query using BM25, returning (entry_index, score) sorted best-first
pub fn bm25_rank(
    entries: &[Value],
    query: &str,
    field_weights: Option<&IndexMap<String, f64>>,
    top_n: usize,
) -> Vec<(usize, f64)> {
    let repeat_saturation = 1.5;
    let length_penalty = 0.75;

    let full_query = query.trim().to_lowercase();
    let mut query_terms: Vec<String> = Vec::new();
    for t in normalize_words(query) {
        if !query_terms.contains(&t) {
            query_terms.push(t);
        }
    }
    if query_terms.is_empty() {
        return vec![];
    }

    let mut entries_by_field: IndexMap<String, Vec<(usize, String)>> = IndexMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        for (field, text) in extract(entry, field_weights) {
            entries_by_field.entry(field).or_default().push((idx, text));
        }
    }

    let mut total_scores: IndexMap<usize, f64> = IndexMap::new();
    for (field_name, field_texts) in &entries_by_field {
        let tallies: Vec<Tally> = field_texts.iter().map(|(_, t)| count_words(t)).collect();
        let lengths: Vec<usize> = tallies.iter().map(|t| t.values().sum()).collect();
        let avg_length = lengths.iter().sum::<usize>() as f64 / lengths.len() as f64;
        if avg_length == 0.0 {
            continue;
        }
        let total_entries = field_texts.len() as f64;

        let mut entries_with_term: IndexMap<&str, usize> = query_terms.iter().map(|t| (t.as_str(), 0)).collect();
        let mut matched: Vec<(usize, usize, &String, IndexMap<&str, usize>)> = Vec::new();
        for (position, (entry_index, text)) in field_texts.iter().enumerate() {
            let mut hits: IndexMap<&str, usize> = IndexMap::new();
            for (word, count) in tallies[position].iter() {
                for term in &query_terms {
                    if word.starts_with(term.as_str()) {
                        *hits.entry(term.as_str()).or_insert(0) += count;
                    }
                }
            }
            if !hits.is_empty() {
                for term in hits.keys() {
                    *entries_with_term.get_mut(term).unwrap() += 1;
                }
                matched.push((position, *entry_index, text, hits));
            }
        }

        let field_weight = field_weights.and_then(|w| w.get(field_name)).copied().unwrap_or(1.0);

        for (position, entry_index, text, hits) in matched {
            let mut entry_score = 0.0;
            for (term, term_hits) in hits {
                let n = entries_with_term[term] as f64;
                let rarity = (1.0 + (total_entries - n + 0.5) / (n + 0.5)).ln();
                let handicap = 1.0 - length_penalty + length_penalty * lengths[position] as f64 / avg_length;
                let th = term_hits as f64;
                entry_score += rarity * th * (repeat_saturation + 1.0) / (th + repeat_saturation * handicap);
            }
            if !full_query.is_empty() && text.to_lowercase().contains(&full_query) {
                entry_score *= 1.5;
            }
            *total_scores.entry(entry_index).or_insert(0.0) += entry_score * field_weight;
        }
    }

    let mut ranked: Vec<(usize, f64)> = total_scores.into_iter().collect();
    // stable sort, best first (python sorted(..., reverse=True) is stable too)
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(top_n);
    ranked
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub id: Value,
    pub score: f64,
    pub entry: Value,
}

/// search that uses BM25 to rank entries. returns ranked {"id", "score", "entry"} records.
pub fn search(
    entries: &[Value],
    query: &str,
    id_field: &str,
    field_weights: Option<&IndexMap<String, f64>>,
    top_n: usize,
) -> Vec<SearchResult> {
    if entries.is_empty() || query.trim().is_empty() {
        return vec![];
    }
    bm25_rank(entries, query, field_weights, top_n)
        .into_iter()
        .map(|(idx, score)| {
            let entry = entries[idx].clone();
            let id = entry
                .as_object()
                .and_then(|o| o.get(id_field))
                .filter(|v| crate::pyjson::truthy(v))
                .cloned()
                .unwrap_or(Value::from(idx));
            SearchResult { id, score, entry }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize() {
        assert_eq!(normalize_words("about_lumara 128gb v3.8 file.txt"), vec!["about", "lumara", "128", "gb", "v", "3.8", "file", "txt"]);
    }

    #[test]
    fn ranking() {
        let entries = vec![
            json!({"id": "a", "title": "kitty facts", "text": "cats are great"}),
            json!({"id": "b", "title": "dogs", "text": "vitamins for dogs"}),
            json!({"id": "c", "title": "nothing", "text": "roof"}),
        ];
        let r = search(&entries, "vitamin", "id", None, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].id, json!("b"));
        let r = search(&entries, "of", "id", None, 10);
        assert!(r.is_empty());
    }

    #[test]
    fn snippets() {
        let s = make_snippets("hello world, this is a test of snippets", "test", 1, 5);
        assert_eq!(s, vec!["...is a test of s...".to_string()]);
    }
}
