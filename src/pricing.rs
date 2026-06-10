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
