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

/// Memory-efficient two-row dynamic program.
fn levenshtein(a: &[char], b: &[char]) -> usize {
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
    fn reset_clears() {
        let mut d = Detector::new();
        d.observe("abc");
        d.reset();
        assert!(d.history().is_empty());
    }
}
