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
        let mid = (lo + hi + 1) / 2;
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
