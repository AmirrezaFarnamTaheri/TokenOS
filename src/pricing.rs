//! Dynamic Shadow Pricing: provider selection as a live constraint
//! optimization rather than a static priority queue.
//!
//!   U = confidence / (alpha * tokenCost + beta * historicalLatency)
//!
//! Higher utility wins. Quota depletion and recent failures depress utility,
//! so the scheduler automatically drains toward healthy, cheap, fast routes.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One provider option under evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub provider: String,
    pub model: String,
    pub cost_per_mtok_in: f64,
    pub cost_per_mtok_out: f64,
    pub max_context: usize,
    pub priority: i32, // static tiebreaker (lower preferred)
}

/// Scored result for a candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceQuote {
    #[serde(flatten)]
    pub candidate: Candidate,
    pub utility: f64,
    pub est_cost_usd: f64,
    pub avg_latency_ms: f64,
    pub recent_fail_pct: f64,
    pub quota_pressure: f64,
}

/// Tunes the utility function.
#[derive(Debug, Clone, Copy)]
pub struct Weights {
    pub alpha: f64, // weight on token cost
    pub beta: f64,  // weight on latency (per ms)
}

impl Default for Weights {
    fn default() -> Self {
        Weights {
            alpha: 1.0,
            beta: 0.002,
        }
    }
}

// ---------------------------------------------------------------------------
// Rolling health tracker (EWMA latency + failure rate + quota window).
// ---------------------------------------------------------------------------

const EWMA_ALPHA: f64 = 0.3;

#[derive(Debug, Default)]
struct Health {
    ewma_latency_ms: f64,
    fail_ewma: f64, // 0..1
    calls: Vec<Instant>,
}

/// Accumulates live per-provider health metrics. Interior mutability via a
/// fine-grained mutex (held only for microsecond map updates — never across
/// network I/O).
#[derive(Debug)]
pub struct Tracker {
    state: Mutex<HashMap<String, Health>>,
    window: Duration,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    /// Tracker with a 1-minute quota window.
    pub fn new() -> Self {
        Tracker {
            state: Mutex::new(HashMap::new()),
            window: Duration::from_secs(60),
        }
    }

    /// Registers an execution outcome for a provider.
    pub fn record(&self, provider: &str, latency_ms: f64, success: bool) {
        let mut state = self.state.lock().unwrap();
        let h = state.entry(provider.to_string()).or_insert_with(|| Health {
            ewma_latency_ms: latency_ms,
            ..Default::default()
        });
        h.ewma_latency_ms = EWMA_ALPHA * latency_ms + (1.0 - EWMA_ALPHA) * h.ewma_latency_ms;
        let f = if success { 0.0 } else { 1.0 };
        h.fail_ewma = EWMA_ALPHA * f + (1.0 - EWMA_ALPHA) * h.fail_ewma;
        let now = Instant::now();
        h.calls.push(now);
        let cutoff = now - self.window;
        h.calls.retain(|t| *t >= cutoff);
    }

    /// Returns (avg_latency_ms, fail_rate, calls_in_window) for a provider.
    pub fn snapshot(&self, provider: &str) -> (f64, f64, usize) {
        let state = self.state.lock().unwrap();
        match state.get(provider) {
            Some(h) => (h.ewma_latency_ms, h.fail_ewma, h.calls.len()),
            None => (0.0, 0.0, 0),
        }
    }
}

// ---------------------------------------------------------------------------
// Shadow pricing
// ---------------------------------------------------------------------------

/// Scores all candidates for a task and returns them sorted by utility (best
/// first). est_in/est_out are token estimates; quota_per_min maps provider →
/// per-minute call quota (0 = unlimited).
pub fn quote_all(
    cands: &[Candidate],
    confidence: f64,
    est_in: usize,
    est_out: usize,
    w: Weights,
    tracker: Option<&Tracker>,
    quota_per_min: &HashMap<String, u32>,
) -> Vec<PriceQuote> {
    let mut quotes: Vec<PriceQuote> = Vec::with_capacity(cands.len());
    for c in cands {
        // Hard constraint: context must fit.
        if c.max_context > 0 && est_in > c.max_context {
            continue;
        }
        let cost = (est_in as f64 * c.cost_per_mtok_in + est_out as f64 * c.cost_per_mtok_out)
            / 1e6;

        let (lat, fail, calls) = tracker
            .map(|t| t.snapshot(&c.provider))
            .unwrap_or((0.0, 0.0, 0));

        // Quota pressure: 0 (idle) .. 1 (saturated). Saturated providers are
        // shadow-priced toward zero utility instead of hard-dropped, so a
        // fully exhausted fleet still produces a deterministic ordering.
        let mut pressure = 0.0;
        if let Some(&q) = quota_per_min.get(&c.provider) {
            if q > 0 {
                pressure = (calls as f64 / q as f64).min(1.0);
            }
        }

        let denom = w.alpha * cost * 1000.0 + w.beta * lat + 1e-9;
        let mut u = confidence / denom;
        u *= 1.0 - fail; // failure-prone providers decay
        u *= 1.0 - 0.9 * pressure; // quota saturation decays utility by up to 90%
        if u < 0.0 {
            u = 0.0;
        }
        quotes.push(PriceQuote {
            candidate: c.clone(),
            utility: u,
            est_cost_usd: cost,
            avg_latency_ms: lat,
            recent_fail_pct: fail,
            quota_pressure: pressure,
        });
    }
    quotes.sort_by(|a, b| {
        b.utility
            .partial_cmp(&a.utility)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.candidate.priority.cmp(&b.candidate.priority))
            .then(a.candidate.provider.cmp(&b.candidate.provider))
    });
    quotes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, cost_in: f64, prio: i32) -> Candidate {
        Candidate {
            provider: name.into(),
            model: "m".into(),
            cost_per_mtok_in: cost_in,
            cost_per_mtok_out: cost_in * 4.0,
            max_context: 100_000,
            priority: prio,
        }
    }

    #[test]
    fn cheaper_wins() {
        let cands = vec![cand("expensive", 10.0, 1), cand("cheap", 0.1, 2)];
        let q = quote_all(&cands, 0.9, 1000, 500, Weights::default(), None, &HashMap::new());
        assert_eq!(q[0].candidate.provider, "cheap");
    }

    #[test]
    fn context_overflow_filtered() {
        let mut c = cand("small", 0.1, 1);
        c.max_context = 100;
        let q = quote_all(&[c], 0.9, 1000, 100, Weights::default(), None, &HashMap::new());
        assert!(q.is_empty());
    }

    #[test]
    fn failures_decay_utility() {
        let t = Tracker::new();
        for _ in 0..10 {
            t.record("flaky", 100.0, false);
        }
        t.record("healthy", 100.0, true);
        let cands = vec![cand("flaky", 1.0, 1), cand("healthy", 1.0, 2)];
        let q = quote_all(&cands, 0.9, 1000, 500, Weights::default(), Some(&t), &HashMap::new());
        assert_eq!(q[0].candidate.provider, "healthy");
    }

    #[test]
    fn quota_pressure_decays() {
        let t = Tracker::new();
        for _ in 0..10 {
            t.record("saturated", 100.0, true);
        }
        let mut quota = HashMap::new();
        quota.insert("saturated".to_string(), 10u32);
        let cands = vec![cand("saturated", 1.0, 1), cand("idle", 1.0, 2)];
        let q = quote_all(&cands, 0.9, 1000, 500, Weights::default(), Some(&t), &quota);
        assert_eq!(q[0].candidate.provider, "idle");
        let sat = q.iter().find(|x| x.candidate.provider == "saturated").unwrap();
        assert!((sat.quota_pressure - 1.0).abs() < 1e-9);
    }

    #[test]
    fn deterministic_tiebreak() {
        let cands = vec![cand("bbb", 1.0, 2), cand("aaa", 1.0, 2)];
        let q = quote_all(&cands, 0.9, 1000, 500, Weights::default(), None, &HashMap::new());
        assert_eq!(q[0].candidate.provider, "aaa");
    }
}

// ---------------------------------------------------------------------------
// Lock-free UCB1 multi-armed bandit router (evolution section 19).
//
// Each provider arm keeps three atomics: pull count, cumulative reward and
// cumulative squared latency. f64 values are bit-cast into AtomicU64 and
// updated with compare-exchange loops, so the hot scoring path takes no lock
// at all and scales linearly across the multi-threaded runtime.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};

/// f64 stored in an AtomicU64 via bit-casting. Updates use a CAS loop —
/// wait-free for readers, lock-free for writers.
#[derive(Debug, Default)]
pub struct AtomicF64(AtomicU64);

impl AtomicF64 {
    pub fn new(v: f64) -> Self {
        AtomicF64(AtomicU64::new(v.to_bits()))
    }

    #[inline]
    pub fn load(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Acquire))
    }

    #[inline]
    pub fn store(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Release);
    }

    /// Lock-free fetch-add via compare-exchange loop.
    pub fn fetch_add(&self, delta: f64) -> f64 {
        let mut cur = self.0.load(Ordering::Acquire);
        loop {
            let new = (f64::from_bits(cur) + delta).to_bits();
            match self
                .0
                .compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(prev) => return f64::from_bits(prev),
                Err(actual) => cur = actual,
            }
        }
    }
}

/// One bandit arm: a provider's lifetime pull count and reward sum.
#[derive(Debug, Default)]
pub struct Arm {
    pulls: AtomicU64,
    reward_sum: AtomicF64,
    latency_sum_ms: AtomicF64,
}

impl Arm {
    pub fn pulls(&self) -> u64 {
        self.pulls.load(Ordering::Acquire)
    }

    pub fn mean_reward(&self) -> f64 {
        let n = self.pulls();
        if n == 0 {
            return 0.0;
        }
        self.reward_sum.load() / n as f64
    }

    pub fn mean_latency_ms(&self) -> f64 {
        let n = self.pulls();
        if n == 0 {
            return 0.0;
        }
        self.latency_sum_ms.load() / n as f64
    }
}

/// Lock-free UCB1 router over a FIXED arm set. The arm list is immutable
/// after construction (no rebalancing locks); all statistics are atomics.
///
///   UCB1(arm) = mean_reward + c * sqrt(2 ln(total_pulls) / arm_pulls)
///
/// Unpulled arms score +infinity, guaranteeing initial exploration of every
/// provider before exploitation begins.
#[derive(Debug)]
pub struct Ucb1Router {
    arms: Vec<(String, Arm)>,
    total_pulls: AtomicU64,
    /// exploration constant (1.0 = classic UCB1; lower = greedier)
    pub exploration: f64,
}

impl Ucb1Router {
    pub fn new(providers: &[String]) -> Self {
        Ucb1Router {
            arms: providers
                .iter()
                .map(|p| (p.clone(), Arm::default()))
                .collect(),
            total_pulls: AtomicU64::new(0),
            exploration: 1.0,
        }
    }

    fn arm(&self, provider: &str) -> Option<&Arm> {
        self.arms
            .iter()
            .find(|(p, _)| p == provider)
            .map(|(_, a)| a)
    }

    /// Records an outcome. Reward = success(0/1) scaled down by latency so
    /// fast successes dominate slow ones; failures earn 0.
    pub fn record(&self, provider: &str, success: bool, latency_ms: f64) {
        let Some(arm) = self.arm(provider) else { return };
        let reward = if success {
            // 1.0 at 0 ms decaying toward ~0.5 at 10 s.
            1.0 / (1.0 + latency_ms / 10_000.0)
        } else {
            0.0
        };
        arm.pulls.fetch_add(1, Ordering::AcqRel);
        arm.reward_sum.fetch_add(reward);
        arm.latency_sum_ms.fetch_add(latency_ms);
        self.total_pulls.fetch_add(1, Ordering::AcqRel);
    }

    /// UCB1 score for a single provider (infinity when unexplored).
    pub fn score(&self, provider: &str) -> f64 {
        let Some(arm) = self.arm(provider) else {
            return f64::NEG_INFINITY;
        };
        let n = arm.pulls();
        if n == 0 {
            return f64::INFINITY;
        }
        let total = self.total_pulls.load(Ordering::Acquire).max(1) as f64;
        arm.mean_reward() + self.exploration * (2.0 * total.ln() / n as f64).sqrt()
    }

    /// Returns all providers ordered by descending UCB1 score with a
    /// deterministic name tiebreak. Zero locks on this path.
    pub fn ranked(&self) -> Vec<(String, f64)> {
        let mut out: Vec<(String, f64)> = self
            .arms
            .iter()
            .map(|(p, _)| (p.clone(), self.score(p)))
            .collect();
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        out
    }

    /// Best arm right now (None when no arms configured).
    pub fn select(&self) -> Option<String> {
        self.ranked().into_iter().next().map(|(p, _)| p)
    }

    /// Raw per-arm statistics: (pulls, mean_reward, mean_latency_ms).
    /// Unknown providers report zeros. Lock-free.
    pub fn arm_stats(&self, provider: &str) -> (u64, f64, f64) {
        match self.arm(provider) {
            None => (0, 0.0, 0.0),
            Some(a) => (a.pulls(), a.mean_reward(), a.mean_latency_ms()),
        }
    }

    /// Deterministic multiplicative weight applied to a shadow-pricing
    /// utility: unexplored or unknown arms are neutral (1.0, so shadow
    /// pricing alone decides and the arm still gets explored); explored arms
    /// scale utility by `0.5 + mean_reward` ∈ [0.5, 1.5], so live observed
    /// success/latency evidence bends the failover order toward arms that
    /// actually deliver.
    pub fn exploitation_weight(&self, provider: &str) -> f64 {
        match self.arm(provider) {
            None => 1.0,
            Some(a) if a.pulls() == 0 => 1.0,
            Some(a) => 0.5 + a.mean_reward(),
        }
    }
}

#[cfg(test)]
mod ucb1_tests {
    use super::*;
    use std::sync::Arc;

    fn arms(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn atomic_f64_roundtrip_and_add() {
        let a = AtomicF64::new(1.5);
        assert_eq!(a.load(), 1.5);
        a.fetch_add(2.25);
        assert_eq!(a.load(), 3.75);
        a.store(-0.5);
        assert_eq!(a.load(), -0.5);
    }

    #[test]
    fn unexplored_arms_are_pulled_first() {
        let r = Ucb1Router::new(&arms(&["a", "b"]));
        r.record("a", true, 100.0);
        // "b" is unexplored → infinite score → selected.
        assert_eq!(r.select().unwrap(), "b");
    }

    #[test]
    fn better_arm_wins_after_exploration() {
        let r = Ucb1Router::new(&arms(&["good", "bad"]));
        for _ in 0..50 {
            r.record("good", true, 50.0);
            r.record("bad", false, 50.0);
        }
        assert_eq!(r.select().unwrap(), "good");
        assert!(r.score("good") > r.score("bad"));
    }

    #[test]
    fn fast_success_outranks_slow_success() {
        let r = Ucb1Router::new(&arms(&["fast", "slow"]));
        for _ in 0..100 {
            r.record("fast", true, 50.0);
            r.record("slow", true, 20_000.0);
        }
        let ranked = r.ranked();
        assert_eq!(ranked[0].0, "fast");
    }

    #[test]
    fn unknown_provider_is_ignored() {
        let r = Ucb1Router::new(&arms(&["a"]));
        r.record("ghost", true, 1.0); // no-op
        assert_eq!(r.score("ghost"), f64::NEG_INFINITY);
        assert_eq!(r.select().unwrap(), "a");
    }

    #[test]
    fn concurrent_records_are_lock_free_and_consistent() {
        let r = Arc::new(Ucb1Router::new(&arms(&["x", "y"])));
        let mut handles = Vec::new();
        for t in 0..8 {
            let r = r.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..1000 {
                    let p = if (t + i) % 2 == 0 { "x" } else { "y" };
                    r.record(p, i % 3 != 0, 10.0 + i as f64);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let total: u64 = r.arms.iter().map(|(_, a)| a.pulls()).sum();
        assert_eq!(total, 8000);
        assert_eq!(r.total_pulls.load(Ordering::Acquire), 8000);
    }

    #[test]
    fn empty_router_selects_none() {
        let r = Ucb1Router::new(&[]);
        assert!(r.select().is_none());
    }
}
