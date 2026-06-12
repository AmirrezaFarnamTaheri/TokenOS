//! Fast, offline token estimation so the kernel can enforce context budgets
//! before a single network byte is spent.
//!
//! Deterministic BPE-approximation heuristic calibrated against cl100k_base
//! behaviour on mixed code/prose corpora. Not a real BPE tokenizer (no
//! vocabulary shipping) but errs conservatively (slightly over-estimates),
//! which is the safe direction for budgeting.

/// Approximate token count for `s`.
///
/// Calibration model:
///  - ASCII prose averages ~4.2 chars/token.
///  - Code/symbol-dense text averages ~1.8 chars/token for symbols.
///  - Non-ASCII (CJK etc.) averages ~1.1 tokens per rune.
pub fn estimate(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let (mut ascii_letters, mut symbols, mut non_ascii, mut spaces) =
        (0usize, 0usize, 0usize, 0usize);
    for r in s.chars() {
        if (r as u32) > 127 {
            non_ascii += 1;
        } else if r.is_whitespace() {
            spaces += 1;
        } else if r.is_ascii_alphanumeric() {
            ascii_letters += 1;
        } else {
            symbols += 1;
        }
    }
    let prose = (ascii_letters + spaces) as f64 / 4.2;
    let sym = symbols as f64 / 1.8;
    let cjk = non_ascii as f64 * 1.1;
    ((prose + sym + cjk) as usize).max(1)
}

/// Convenience wrapper for raw byte slices.
pub fn estimate_bytes(b: &[u8]) -> usize {
    match std::str::from_utf8(b) {
        Ok(s) => estimate(s),
        Err(_) => b.len() / 2, // binary-ish content: assume worst-case density
    }
}

/// Whether text fits within `max_tokens`.
pub fn fits_budget(s: &str, max_tokens: usize) -> bool {
    estimate(s) <= max_tokens
}

/// Trims `s` so its estimate fits within `max_tokens`, cutting at the nearest
/// line boundary to avoid shipping half-statements.
pub fn truncate(s: &str, max_tokens: usize) -> String {
    if fits_budget(s, max_tokens) {
        return s.to_string();
    }
    // Binary search on char boundary for the largest fitting prefix.
    let idxs: Vec<usize> = s.char_indices().map(|(i, _)| i).chain([s.len()]).collect();
    let (mut lo, mut hi) = (0usize, idxs.len() - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if estimate(&s[..idxs[mid]]) <= max_tokens {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let cut = &s[..idxs[lo]];
    // Pull back to the last newline if one exists in the final 20%.
    let floor = cut.len() * 4 / 5;
    if let Some(pos) = cut.rfind('\n') {
        if pos >= floor {
            return cut[..pos].to_string();
        }
    }
    cut.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        assert_eq!(estimate(""), 0);
    }

    #[test]
    fn prose_density() {
        let s = "The quick brown fox jumps over the lazy dog repeatedly today.";
        let est = estimate(s);
        assert!((10..=22).contains(&est), "estimate was {est}");
    }

    #[test]
    fn cjk_counts_per_rune() {
        let est = estimate("これはテストです");
        assert!(est >= 8, "estimate was {est}");
    }

    #[test]
    fn truncate_respects_budget() {
        let s = "line one\n".repeat(500);
        let t = truncate(&s, 100);
        assert!(estimate(&t) <= 100);
        assert!(!t.is_empty());
    }

    #[test]
    fn truncate_keeps_small_intact() {
        assert_eq!(truncate("short", 100), "short");
    }

    #[test]
    fn truncate_multibyte_safe() {
        let s = "日本語のテキスト".repeat(200);
        let t = truncate(&s, 50);
        assert!(estimate(&t) <= 50);
    }

    #[test]
    fn binary_bytes_fallback() {
        let b = vec![0xFFu8; 100];
        assert_eq!(estimate_bytes(&b), 50);
    }
}

// ---------------------------------------------------------------------------
// Greedy longest-match subword counter (evolution section 23).
//
// A deterministic BPE-style segmenter: text is split on whitespace and
// punctuation boundaries, then each word is segmented by greedy longest
// match against an embedded vocabulary of high-frequency cl100k-like
// subword units. Unmatched residue falls back to ~4-chars-per-token (ASCII)
// or one-token-per-char (non-ASCII), matching the conservative budgeting
// direction of `estimate`. No vocabulary files are shipped or mmapped —
// the merge table is compiled into the binary, so counting is exact w.r.t.
// this vocabulary and fully reproducible across machines.
// ---------------------------------------------------------------------------

use once_cell::sync::Lazy;
use std::collections::HashSet;

/// High-frequency subword vocabulary (lowercase). Order doesn't matter —
/// matching is greedy longest-first by probing lengths descending.
static VOCAB_LIST: &[&str] = &[
    // whole high-frequency words
    "the",
    "and",
    "for",
    "are",
    "but",
    "not",
    "you",
    "all",
    "can",
    "had",
    "her",
    "was",
    "one",
    "our",
    "out",
    "day",
    "get",
    "has",
    "him",
    "his",
    "how",
    "man",
    "new",
    "now",
    "old",
    "see",
    "two",
    "way",
    "who",
    "boy",
    "did",
    "its",
    "let",
    "put",
    "say",
    "she",
    "too",
    "use",
    "that",
    "with",
    "have",
    "this",
    "will",
    "your",
    "from",
    "they",
    "know",
    "want",
    "been",
    "good",
    "much",
    "some",
    "time",
    "very",
    "when",
    "come",
    "here",
    "just",
    "like",
    "long",
    "make",
    "many",
    "more",
    "only",
    "over",
    "such",
    "take",
    "than",
    "them",
    "well",
    "were",
    "what",
    "into",
    "code",
    "file",
    "test",
    "data",
    "type",
    "list",
    "name",
    "value",
    "error",
    "function",
    "return",
    "string",
    "number",
    "object",
    "array",
    "class",
    "const",
    "import",
    "export",
    "public",
    "private",
    "static",
    "struct",
    "match",
    "async",
    "await",
    "print",
    "write",
    "read",
    "open",
    "close",
    "true",
    "false",
    "null",
    "none",
    "self",
    "must",
    "should",
    "would",
    "could",
    "about",
    "after",
    "before",
    "first",
    "other",
    "right",
    "their",
    "there",
    "these",
    "thing",
    "think",
    "three",
    "under",
    "water",
    "where",
    "which",
    "while",
    "world",
    "years",
    "implement",
    "update",
    "create",
    "delete",
    "remove",
    "change",
    "check",
    "build",
    "start",
    "spawn",
    "thread",
    "token",
    "model",
    "route",
    "provider",
    "config",
    "state",
    "task",
    "goal",
    // common prefixes/suffixes/subwords
    "ing",
    "ion",
    "tion",
    "ation",
    "ed",
    "er",
    "est",
    "ly",
    "ity",
    "ment",
    "ness",
    "able",
    "ible",
    "ous",
    "ful",
    "less",
    "ize",
    "ise",
    "ant",
    "ent",
    "al",
    "ic",
    "ive",
    "ate",
    "ary",
    "ory",
    "pre",
    "pro",
    "con",
    "com",
    "dis",
    "mis",
    "non",
    "sub",
    "super",
    "trans",
    "inter",
    "intra",
    "over",
    "under",
    "anti",
    "auto",
    "semi",
    "multi",
    "micro",
    "macro",
    "re",
    "un",
    "in",
    "im",
    "ir",
    "il",
    "de",
    "ex",
    "en",
    "em",
    "be",
    "an",
    "ab",
    "ad",
    "ac",
    "as",
    "at",
    "co",
    "do",
    "go",
    "if",
    "is",
    "it",
    "me",
    "my",
    "no",
    "of",
    "on",
    "or",
    "so",
    "to",
    "up",
    "us",
    "we",
    "ow",
    "ay",
    "ai",
    "ea",
    "ee",
    "oo",
    "ou",
    "th",
    "ch",
    "sh",
    "wh",
    "qu",
    "ck",
    "ng",
    "nk",
    "nt",
    "nd",
    "st",
    "sp",
    "sc",
    "sk",
    "sl",
    "sm",
    "sn",
    "sw",
    "tw",
    "tr",
    "dr",
    "br",
    "cr",
    "fr",
    "gr",
    "pr",
    "bl",
    "cl",
    "fl",
    "gl",
    "pl",
];

static VOCAB: Lazy<(HashSet<&'static str>, usize)> = Lazy::new(|| {
    let set: HashSet<&'static str> = VOCAB_LIST.iter().copied().collect();
    let max_len = VOCAB_LIST.iter().map(|w| w.len()).max().unwrap_or(1);
    (set, max_len)
});

/// Exact (vocabulary-relative) token count via greedy longest-match
/// segmentation. Deterministic, allocation-light, O(N * max_piece_len).
pub fn count_bpe(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let (vocab, max_len) = (&VOCAB.0, VOCAB.1);
    let mut tokens = 0usize;
    let mut buf = [0u8; 128];
    let mut len = 0usize;

    let flush = |b: &[u8], tokens: &mut usize| {
        if b.is_empty() {
            return;
        }
        let mut i = 0usize;
        while i < b.len() {
            let mut matched = 0usize;
            let cap = (b.len() - i).min(max_len);
            for l in (2..=cap).rev() {
                if let Ok(slice) = std::str::from_utf8(&b[i..i + l]) {
                    if vocab.contains(slice) {
                        matched = l;
                        break;
                    }
                }
            }
            if matched > 0 {
                *tokens += 1;
                i += matched;
            } else {
                let j = (i + 4).min(b.len());
                *tokens += 1;
                i = j;
            }
        }
    };

    for ch in s.chars() {
        if ch.is_alphanumeric() && ch.is_ascii() {
            if len >= 128 {
                flush(&buf[..len], &mut tokens);
                len = 0;
            }
            buf[len] = (ch as u8).to_ascii_lowercase();
            len += 1;
        } else if (ch as u32) > 127 {
            flush(&buf[..len], &mut tokens);
            len = 0;
            tokens += 1; // non-ASCII: ~1 token per scalar (CJK-conservative)
        } else if ch.is_whitespace() {
            flush(&buf[..len], &mut tokens);
            len = 0;
            // whitespace fuses into the next token (BPE leading-space): free
        } else {
            flush(&buf[..len], &mut tokens);
            len = 0;
            tokens += 1; // each symbol is a token
        }
    }
    flush(&buf[..len], &mut tokens);
    tokens.max(1)
}

/// Best-of-both budget counter: takes the max of the calibrated heuristic
/// and the greedy subword count, erring conservative for budget enforcement.
pub fn count_conservative(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    estimate(s).max(count_bpe(s))
}

#[cfg(test)]
mod bpe_tests {
    use super::*;

    #[test]
    fn empty_counts_zero() {
        assert_eq!(count_bpe(""), 0);
        assert_eq!(count_conservative(""), 0);
    }

    #[test]
    fn known_words_count_once_or_twice() {
        // "the" is a single vocab hit.
        assert_eq!(count_bpe("the"), 1);
        // "thinking" → "think" + "ing" = 2.
        assert_eq!(count_bpe("thinking"), 2);
    }

    #[test]
    fn symbols_count_individually() {
        assert_eq!(count_bpe("{}();"), 5);
    }

    #[test]
    fn whitespace_is_free() {
        assert_eq!(count_bpe("the   the"), 2);
    }

    #[test]
    fn cjk_counts_per_char() {
        assert_eq!(count_bpe("日本語"), 3);
    }

    #[test]
    fn deterministic() {
        let s = "implement the new token counting function for the kernel";
        assert_eq!(count_bpe(s), count_bpe(s));
    }

    #[test]
    fn prose_density_plausible() {
        let s = "The quick brown fox jumps over the lazy dog repeatedly today.";
        let n = count_bpe(s);
        // 11 words + 1 period; subword splits keep it in a sane band.
        assert!((10..=32).contains(&n), "count was {n}");
    }

    #[test]
    fn conservative_dominates_both() {
        let s = "implement the new function";
        assert!(count_conservative(s) >= estimate(s));
        assert!(count_conservative(s) >= count_bpe(s));
    }

    #[test]
    fn unknown_long_word_falls_back_chunked() {
        // 16 chars of consonant gibberish with no vocab hits longer than 2:
        // must stay bounded (≤ 8 fallback chunks).
        let n = count_bpe("xzqvxzqvxzqvxzqv");
        assert!(n <= 8, "count was {n}");
        assert!(n >= 4);
    }
}
