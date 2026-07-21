//! The assistant layer: page context in, answer out.
//!
//! # Honesty about what this is
//! [`LocalAssistant`] is **not a language model**. It is a classic extractive
//! summarizer (word-frequency sentence ranking) plus document statistics. It ships
//! as the default because it needs no network, no API key, and no user consent —
//! which makes it the only provider that satisfies the privacy invariant in
//! docs/04-SECURITY-PRIVACY.md §5.5 unconditionally.
//!
//! A real model plugs in behind [`Assistant`]: implement the trait, and route the
//! same [`PageContext`]. Anything that leaves the device must be gated on explicit,
//! per-action consent — that gate belongs here, not in the engine.

use std::collections::HashMap;

/// The sanitized view of a page an assistant is allowed to see.
/// Deliberately text-only: no markup, no scripts, no cookies, no form values.
pub struct PageContext {
    pub url: String,
    pub text: String,
    pub headings: Vec<(u8, String)>,
    pub blocked_trackers: usize,
    pub secure: bool,
}

pub trait Assistant {
    /// Human-readable analysis of the current page.
    fn respond(&self, ctx: &PageContext) -> String;
    /// Shown in the panel so the user always knows where their data went.
    fn provenance(&self) -> &'static str;
}

/// Runs entirely on-device. No network, ever.
pub struct LocalAssistant;

impl Assistant for LocalAssistant {
    fn provenance(&self) -> &'static str {
        "On-device - nothing left your machine"
    }

    fn respond(&self, ctx: &PageContext) -> String {
        if ctx.text.split_whitespace().count() < 12 {
            return "Not enough readable text on this page to summarise.".to_string();
        }
        let words = ctx.text.split_whitespace().count();
        // ~200 wpm is the usual adult silent-reading estimate.
        let minutes = (words as f32 / 200.0).ceil().max(1.0) as usize;

        let mut report = String::new();
        report.push_str("SUMMARY\n");
        report.push_str(&summarize(&ctx.text, 3));
        report.push_str("\n\nPAGE\n");
        if !ctx.url.is_empty() {
            report.push_str(&format!("{}\n", ctx.url));
        }
        report.push_str(&format!("{words} words - about {minutes} min read\n"));
        report.push_str(if ctx.secure { "Connection: encrypted\n" } else { "Connection: NOT secure\n" });
        if ctx.blocked_trackers > 0 {
            report.push_str(&format!("Blocked {} tracker requests\n", ctx.blocked_trackers));
        }
        if !ctx.headings.is_empty() {
            report.push_str("\nOUTLINE\n");
            for (level, text) in ctx.headings.iter().take(8) {
                let indent = "  ".repeat((*level).saturating_sub(1) as usize);
                report.push_str(&format!("{indent}{text}\n"));
            }
        }
        report
    }
}

/// Words that carry no topical signal, so they shouldn't drive sentence ranking.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "if", "of", "to", "in", "on", "for", "with", "as", "is",
    "are", "was", "were", "be", "been", "it", "its", "this", "that", "these", "those", "at", "by",
    "from", "you", "your", "we", "our", "they", "their", "he", "she", "his", "her", "not", "can",
    "will", "would", "should", "there", "here", "have", "has", "had", "do", "does", "did", "so",
];

fn normalize(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

/// Split on sentence-ending punctuation followed by whitespace.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '!' | '?')
            && bytes.get(i + 1).map(|b| b.is_ascii_whitespace()).unwrap_or(true)
        {
            let end = i + c.len_utf8();
            let s = text[start..end].trim();
            if !s.is_empty() {
                sentences.push(s);
            }
            start = end;
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        sentences.push(tail);
    }
    sentences
}

/// Extractive summary: rank sentences by mean frequency of their content words,
/// then emit the top `max` in original document order so it still reads naturally.
fn summarize(text: &str, max: usize) -> String {
    let sentences = split_sentences(text);
    if sentences.len() <= max {
        return sentences.join(" ");
    }

    let mut freq: HashMap<String, usize> = HashMap::new();
    for word in text.split_whitespace() {
        let w = normalize(word);
        if w.len() > 2 && !STOPWORDS.contains(&w.as_str()) {
            *freq.entry(w).or_insert(0) += 1;
        }
    }

    // Short fragments (headings, citation titles, nav labels) are keyword-dense but
    // say nothing. Very long "sentences" are unpunctuated boilerplate blobs (link
    // farms, footers) rather than prose. Prefer the band in between.
    const MIN_WORDS: usize = 8;
    const MAX_WORDS: usize = 60;
    let substantial: Vec<usize> = sentences
        .iter()
        .enumerate()
        .filter(|(_, s)| (MIN_WORDS..=MAX_WORDS).contains(&s.split_whitespace().count()))
        .map(|(i, _)| i)
        .collect();
    let pool = if substantial.len() >= max { substantial } else { (0..sentences.len()).collect() };

    let mut scored: Vec<(usize, f32)> = pool
        .into_iter()
        .map(|i| {
            let words: Vec<String> = sentences[i].split_whitespace().map(normalize).collect();
            let total: usize = words.iter().filter_map(|w| freq.get(w)).sum();
            // Dampened length normalisation: full mean over-rewards terse fragments,
            // raw sum over-rewards rambling ones.
            (i, total as f32 / (words.len().max(1) as f32).sqrt())
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut picked: Vec<usize> = scored.iter().take(max).map(|(i, _)| *i).collect();
    picked.sort_unstable();
    picked.iter().map(|&i| sentences[i]).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_topical_sentences_in_document_order() {
        let text = "Zero is a browser. The weather is unrelated today. \
                    Zero renders pages with its own engine. Zero is written in Rust.";
        let summary = summarize(text, 2);
        // The off-topic sentence should lose to the Zero-heavy ones.
        assert!(!summary.contains("weather"), "got {summary}");
        // Whichever sentences win, they must appear in document order.
        let positions: Vec<usize> =
            split_sentences(&summary).iter().map(|s| text.find(s).expect("from source")).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "out of order: {summary}");
    }

    #[test]
    fn short_text_is_returned_whole() {
        assert_eq!(summarize("One. Two.", 3), "One. Two.");
    }

    #[test]
    fn refuses_to_summarise_a_thin_page() {
        let ctx = PageContext {
            url: "x".into(),
            text: "too short".into(),
            headings: vec![],
            blocked_trackers: 0,
            secure: true,
        };
        assert!(LocalAssistant.respond(&ctx).contains("Not enough"));
    }
}
