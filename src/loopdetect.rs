//! Semantic loop detection via edit-distance ceilings (Levenshtein). Agents
//! oscillating between near-identical failed attempts are caught
//! deterministically and escalated, bypassing model confidence entirely.
//!
//! The detector window is persisted in SQLite by the store layer (audit
//! finding 12.2), so loops are detected across cold CLI process invocations.

/// Normalized edit-distance below which two failed attempts are considered
/// "the same attempt" (3%).
pub const DEFAULT_CEILING: f64 = 0.03;

/// Hard cap on input size for the quadratic Levenshtein pass. Inputs larger
/// than this are pre-hashed/truncated to keep CPU bounded (scalability
/// guard against O(M*N) blowups on huge generations).
pub const MAX_COMPARE_CHARS: usize = 20_000;

/// In-memory detector over a bounded window of prior failed attempts.
/// For cross-process durability, feed it history loaded from the store.
#[derive(Debug, Clone)]
pub struct Detector {
    pub ceiling: f64,
    pub window: usize,
    history: Vec<String>,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector {
    pub fn new() -> Self {
        Detector {
            ceiling: DEFAULT_CEILING,
            window: 5,
            history: Vec::new(),
        }
    }

    /// Seeds the window from persisted history (oldest first).
    pub fn with_history(mut self, history: Vec<String>) -> Self {
        self.history = history;
        self.trim();
        self
    }

    /// Records a failed attempt and reports whether it forms a semantic loop
    /// with any previously observed attempt.
    pub fn observe(&mut self, attempt: &str) -> bool {
        let looped = self
            .history
            .iter()
            .any(|prev| normalized_distance(prev, attempt) < self.ceiling);
        self.history.push(attempt.to_string());
        self.trim();
        looped
    }

    /// Checks for a loop without recording (read-only probe).
    pub fn would_loop(&self, attempt: &str) -> bool {
        self.history
            .iter()
            .any(|prev| normalized_distance(prev, attempt) < self.ceiling)
    }

    /// Clears the attempt history (e.g., after a successful verification).
    pub fn reset(&mut self) {
        self.history.clear();
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    fn trim(&mut self) {
        if self.window > 0 && self.history.len() > self.window {
            let cut = self.history.len() - self.window;
            self.history.drain(..cut);
        }
    }
}

/// Levenshtein(a,b) / max(len(a),len(b)), with a size guard: oversized
/// inputs compare only their leading MAX_COMPARE_CHARS chars.
///
/// Evolution section 22: the inner distance uses Myers' 1999 bit-parallel
/// algorithm — 64 DP cells advance per machine word per pattern character,
/// turning O(M*N) scalar work into O(ceil(M/64)*N) word operations. The
/// pattern is the shorter string so the word count is minimal.
pub fn normalized_distance(a: &str, b: &str) -> f64 {
    if a == b {
        return 0.0;
    }
    let ra: Vec<char> = a.chars().take(MAX_COMPARE_CHARS).collect();
    let rb: Vec<char> = b.chars().take(MAX_COMPARE_CHARS).collect();
    let (la, lb) = (ra.len(), rb.len());
    if la == 0 || lb == 0 {
        return 1.0;
    }
    let max_len = la.max(lb);
    levenshtein(&ra, &rb) as f64 / max_len as f64
}

/// Edit distance dispatcher: Myers bit-parallel for the common case, plain
/// two-row DP as the small-input fallback (avoids setup overhead for tiny
/// strings where it dominates).
fn levenshtein(a: &[char], b: &[char]) -> usize {
    let (la, lb) = (a.len(), b.len());
    if la == 0 {
        return lb;
    }
    if lb == 0 {
        return la;
    }
    // Pattern = shorter string (fewer 64-bit blocks).
    let (pat, txt) = if la <= lb { (a, b) } else { (b, a) };
    if pat.len() <= 16 {
        return levenshtein_two_row(pat, txt);
    }
    myers_levenshtein(pat, txt)
}

/// Myers bit-parallel Levenshtein, multiword variant per Hyyrö (2003),
/// "A bit-vector algorithm for computing Levenshtein and Damerau edit
/// distances". The pattern (vertical DP dimension) is split into 64-row
/// blocks; horizontal deltas carry between vertically adjacent blocks. Each
/// text character advances ceil(M/64) words instead of M scalar cells.
fn myers_levenshtein(pat: &[char], txt: &[char]) -> usize {
    use std::collections::HashMap;
    let m = pat.len();
    let blocks = m.div_ceil(64);

    // Per-character match bitmasks, one u64 per block.
    let mut peq: HashMap<char, Vec<u64>> = HashMap::new();
    for (i, &c) in pat.iter().enumerate() {
        peq.entry(c)
            .or_insert_with(|| vec![0u64; blocks])[i / 64] |= 1u64 << (i % 64);
    }
    let zeros = vec![0u64; blocks];

    let mut vp = vec![u64::MAX; blocks]; // vertical +1 deltas
    let mut vn = vec![0u64; blocks];     // vertical -1 deltas
    let mut score = m;
    let last = blocks - 1;
    let test_bit = 1u64 << ((m - 1) % 64);

    for &tc in txt {
        let eq_all = peq.get(&tc).unwrap_or(&zeros);
        // Boundary row 0 has horizontal delta +1.
        let mut ph_in = 1u64;
        let mut mh_in = 0u64;

        for blk in 0..blocks {
            let pv = vp[blk];
            let nv = vn[blk];
            let eq = eq_all[blk];

            let xv = eq | nv;
            let eq_h = eq | mh_in;
            let xh = ((eq_h & pv).wrapping_add(pv) ^ pv) | eq_h;

            let ph = nv | !(xh | pv);
            let mh = pv & xh;

            if blk == last {
                if ph & test_bit != 0 {
                    score += 1;
                } else if mh & test_bit != 0 {
                    score -= 1;
                }
            }

            let ph_out = ph >> 63;
            let mh_out = mh >> 63;
            let ph_sh = (ph << 1) | ph_in;
            let mh_sh = (mh << 1) | mh_in;

            vp[blk] = mh_sh | !(xv | ph_sh);
            vn[blk] = ph_sh & xv;

            ph_in = ph_out;
            mh_in = mh_out;
        }
    }
    score
}

/// Memory-efficient two-row dynamic program (small-input fallback and the
/// reference oracle for the property test below).
fn levenshtein_two_row(a: &[char], b: &[char]) -> usize {
    let (la, lb) = (a.len(), b.len());
    if la == 0 {
        return lb;
    }
    if lb == 0 {
        return la;
    }
    let mut prev: Vec<usize> = (0..=lb).collect();
    let mut curr: Vec<usize> = vec![0; lb + 1];
    for i in 1..=la {
        curr[0] = i;
        for j in 1..=lb {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[lb]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_is_zero() {
        assert_eq!(normalized_distance("abc", "abc"), 0.0);
    }

    #[test]
    fn disjoint_is_one() {
        assert!(normalized_distance("aaaa", "bbbb") >= 0.99);
    }

    #[test]
    fn detects_near_identical_loop() {
        let mut d = Detector::new();
        let attempt = "x".repeat(1000);
        assert!(!d.observe(&attempt));
        let mut near = attempt.clone();
        near.replace_range(0..5, "yyyyy"); // 0.5% change < 3% ceiling
        assert!(d.observe(&near));
    }

    #[test]
    fn distinct_attempts_pass() {
        let mut d = Detector::new();
        assert!(!d.observe("first completely different approach to the problem"));
        assert!(!d.observe("second wholly unrelated strategy with new structure"));
    }

    #[test]
    fn window_bounds_history() {
        let mut d = Detector::new();
        for i in 0..10 {
            d.observe(&format!("attempt number {i} with unique content padding {i}{i}{i}"));
        }
        assert!(d.history().len() <= 5);
    }

    #[test]
    fn seeded_history_detects_cross_process_loop() {
        // Simulates persistence: history reloaded from store, loop caught.
        let prior = vec!["the exact same failing output body".to_string()];
        let d = Detector::new().with_history(prior);
        assert!(d.would_loop("the exact same failing output body"));
    }

    #[test]
    fn myers_matches_reference_dp() {
        // Property check: bit-parallel result == classic DP on assorted
        // pairs spanning block boundaries (63/64/65/100+ chars).
        let cases: Vec<(String, String)> = vec![
            ("kitten".into(), "sitting".into()),
            ("a".repeat(63), format!("{}b", "a".repeat(63))),
            ("x".repeat(64), "x".repeat(64)),
            ("x".repeat(64), "y".repeat(64)),
            ("ab".repeat(50), "ba".repeat(50)),
            ("the quick brown fox jumps over the lazy dog".repeat(3),
             "the quick brown cat jumps over the lazy dog".repeat(3)),
            ("z".repeat(130), format!("{}q{}", "z".repeat(65), "z".repeat(64))),
            ("hello".into(), "world-of-completely-different-content".into()),
        ];
        for (a, b) in cases {
            let ca: Vec<char> = a.chars().collect();
            let cb: Vec<char> = b.chars().collect();
            let reference = levenshtein_two_row(&ca, &cb);
            let (pat, txt) = if ca.len() <= cb.len() { (&ca, &cb) } else { (&cb, &ca) };
            let fast = myers_levenshtein(pat, txt);
            assert_eq!(fast, reference, "mismatch for {:?} vs {:?}", a, b);
        }
    }

    #[test]
    fn myers_multiblock_unicode() {
        let a: String = "日本語テキスト".repeat(20); // 140 chars, 3 blocks
        let mut b = a.clone();
        b.push('変');
        let ca: Vec<char> = a.chars().collect();
        let cb: Vec<char> = b.chars().collect();
        assert_eq!(myers_levenshtein(&ca, &cb), 1);
    }

    #[test]
    fn large_inputs_stay_fast_and_correct() {
        // 20k-char near-identical inputs: the 3% ceiling must trip, and the
        // bit-parallel path must agree with the normalized expectation.
        let a = "lorem ipsum dolor sit amet ".repeat(800); // ~21.6k chars
        let mut b = a.clone();
        b.replace_range(0..10, "XXXXXXXXXX");
        let d = normalized_distance(&a, &b);
        assert!(d < 0.03, "near-identical large inputs must loop: d={d}");
    }

    #[test]
    fn reset_clears() {
        let mut d = Detector::new();
        d.observe("abc");
        d.reset();
        assert!(d.history().is_empty());
    }
}
