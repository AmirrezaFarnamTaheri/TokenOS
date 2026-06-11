//! Token-Optimal Agent Execution Kernel: a deterministic, zero-token routing
//! engine that decides HOW a task is executed before any network byte is spent.
//!
//! Primary objective:  maximize  Value Delivered / Total System Cost
//! Meta rule: never spend more resources deciding than the decision can save.

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// An execution route in strict priority order. The earliest applicable
/// route always wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Route {
    #[serde(rename = "DIRECT")]
    Direct, // 0: trivial, execute immediately
    #[serde(rename = "REUSE")]
    Reuse, // 1: existing solution satisfies most requirements
    #[serde(rename = "PATCH")]
    Patch, // 2: localized change, architecture intact
    #[serde(rename = "IMPLEMENT")]
    Implement, // 3: requirements clear, full build is cheapest
    #[serde(rename = "PARTIAL")]
    Partial, // 4: genuine external blocker, deliver what's done
    #[serde(rename = "DELEGATE")]
    Delegate, // 5: repetitive + bounded, delegation pays for itself
    #[serde(rename = "ASK")]
    Ask, // 6: blocked by missing information (one question)
    #[serde(rename = "VERIFY")]
    Verify, // internal: verification-only execution
    #[serde(rename = "ESCALATE-CONFLICT")]
    EscalateConflict, // requirements contradict
    #[serde(rename = "ESCALATE-SAFETY")]
    EscalateSafety, // violates constraints or policy
    #[serde(rename = "ESCALATE-EXTERNAL")]
    EscalateExternal, // external dependency blocks progress
}

impl Route {
    /// Kernel priority index (lower = earlier).
    pub fn priority(self) -> u8 {
        match self {
            Route::Direct => 0,
            Route::Reuse => 1,
            Route::Patch => 2,
            Route::Implement => 3,
            Route::Partial => 4,
            Route::Delegate => 5,
            Route::Ask => 6,
            Route::EscalateConflict | Route::EscalateSafety | Route::EscalateExternal => 7,
            Route::Verify => 99,
        }
    }

    /// Whether the route terminates execution upward.
    pub fn is_escalation(self) -> bool {
        matches!(
            self,
            Route::EscalateConflict | Route::EscalateSafety | Route::EscalateExternal
        )
    }

    /// Whether the route resolves with zero network cost.
    pub fn is_terminal_local(self) -> bool {
        self == Route::Ask || self.is_escalation()
    }

    /// Route-scoped output budget (evolution S27): the maximum number of
    /// output tokens a route is allowed to request from a provider. Paying
    /// for more output headroom than the route's contract can use is pure
    /// waste — an ASK is one question, a DIRECT answer is short, a PATCH is
    /// a minimal diff. Only full builds get the wide ceiling.
    pub fn max_output_tokens(self) -> i64 {
        match self {
            Route::Ask => 256,                       // exactly one question
            Route::Direct => 1024,                   // trivial answer
            Route::Delegate => 1024,                 // packet ack, not prose
            Route::Verify => 1024,                   // verdict, not essay
            Route::Reuse | Route::Patch => 2048,     // bounded modification
            Route::Partial | Route::Implement => 4096, // full productive output
            Route::EscalateConflict | Route::EscalateSafety | Route::EscalateExternal => 0, // never reach a provider
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Route::Direct => "DIRECT",
            Route::Reuse => "REUSE",
            Route::Patch => "PATCH",
            Route::Implement => "IMPLEMENT",
            Route::Partial => "PARTIAL",
            Route::Delegate => "DELEGATE",
            Route::Ask => "ASK",
            Route::Verify => "VERIFY",
            Route::EscalateConflict => "ESCALATE-CONFLICT",
            Route::EscalateSafety => "ESCALATE-SAFETY",
            Route::EscalateExternal => "ESCALATE-EXTERNAL",
        }
    }
}

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Routed,
    InProgress,
    Verifying,
    Done,
    Blocked,
    Escalated,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Routed => "routed",
            Status::InProgress => "in_progress",
            Status::Verifying => "verifying",
            Status::Done => "done",
            Status::Blocked => "blocked",
            Status::Escalated => "escalated",
            Status::Failed => "failed",
        }
    }
}

/// One line of failure memory: a failed action and its reason.
/// Maximum 5 entries are retained per task; oldest entries are evicted first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEntry {
    pub action: String,
    pub reason: String,
    pub at: DateTime<Utc>,
}

/// Hard cap on retained failure entries per task.
pub const MAX_FAILURE_MEMORY: usize = 5;

/// The compressed, structured task state that replaces conversational
/// history. State is preferred over summaries; summaries over transcripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub task_id: String,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    pub status: Status,
    pub blocked: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub next_action: String,
    /// minimum viable context only
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
    #[serde(
        rename = "failure_memory",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub failures: Vec<FailureEntry>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl State {
    pub fn new(task_id: impl Into<String>, goal: impl Into<String>) -> Self {
        let now = Utc::now();
        State {
            task_id: task_id.into(),
            goal: goal.into(),
            constraints: Vec::new(),
            status: Status::Pending,
            blocked: false,
            next_action: String::new(),
            context: String::new(),
            failures: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Appends a failure entry, evicting the oldest beyond the cap.
    pub fn remember_failure(&mut self, action: &str, reason: &str) {
        self.failures.push(FailureEntry {
            action: action.to_string(),
            reason: reason.to_string(),
            at: Utc::now(),
        });
        if self.failures.len() > MAX_FAILURE_MEMORY {
            let cut = self.failures.len() - MAX_FAILURE_MEMORY;
            self.failures.drain(..cut);
        }
    }

    /// Canonical compressed JSON form used for state transfer.
    pub fn compact(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

/// Deterministic inputs the router uses to pick a route. Computed locally
/// (zero token cost) from the task description, the workspace index and
/// prior telemetry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Signals {
    pub estimated_tokens: usize,
    pub confidence: f64,
    pub trivial: bool,
    pub has_existing_solution: bool,
    pub localized_change: bool,
    pub repetitive: bool,
    pub bounded: bool,
    pub external_blocker: bool,
    pub conflicting_requirements: bool,
    pub safety_violation: bool,
    pub missing_critical_info: bool,
    pub repeated_failure: bool,
    pub loop_detected: bool,
}

/// Tunable, empirically adjusted thresholds. These live in deterministic
/// code — never in a worker prompt — so routing costs zero tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterPolicy {
    pub ask_threshold: f64,
    pub direct_max_tokens: usize,
    pub delegation_penalty: f64,
    pub delegation_min_scale: f64,
    /// Budget sentinel (evolution S29): hard per-task cost ceiling in USD.
    /// Providers whose shadow-priced estimate exceeds it are excluded from
    /// the failover chain; if EVERY candidate exceeds it the run terminates
    /// locally at zero token cost. 0 disables the sentinel.
    #[serde(default)]
    pub max_cost_per_task_usd: f64,
    /// Verified solution cache (evolution S25): serve an exact goal +
    /// constraints re-request from the durable cache at zero tokens.
    /// Enabled by default; set false to always re-execute.
    #[serde(default = "default_true")]
    pub reuse_cache: bool,
}

fn default_true() -> bool {
    true
}

impl Default for RouterPolicy {
    fn default() -> Self {
        RouterPolicy {
            ask_threshold: 0.35,
            direct_max_tokens: 600,
            delegation_penalty: 1500.0, // tokens-equivalent fixed cost
            delegation_min_scale: 1.5,  // savings must exceed 1.5x the penalty
            max_cost_per_task_usd: 0.0, // 0 = sentinel disabled
            reuse_cache: true,
        }
    }
}

/// Router output: the chosen route plus a one-line reason. The reason is for
/// the human flight recorder, never transmitted upstream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub route: Route,
    pub reason: String,
    pub signals: Signals,
}

/// Applies the kernel's strict priority ladder. Earliest applicable route
/// wins. Escalations and information blocks preempt everything because they
/// cost the least and prevent guaranteed waste.
pub fn decide(s: Signals, p: &RouterPolicy) -> Decision {
    let (route, reason) = if s.safety_violation {
        (
            Route::EscalateSafety,
            "execution would violate constraints or policy",
        )
    } else if s.conflicting_requirements {
        (Route::EscalateConflict, "requirements contradict each other")
    } else if s.loop_detected {
        (
            Route::EscalateExternal,
            "semantic execution loop detected (edit-distance ceiling)",
        )
    } else if s.external_blocker && !s.bounded {
        (
            Route::EscalateExternal,
            "external dependency prevents any execution",
        )
    } else if s.missing_critical_info || s.confidence < p.ask_threshold {
        (
            Route::Ask,
            "execution blocked by missing information; one question required",
        )
    } else if s.trivial && s.estimated_tokens <= p.direct_max_tokens {
        (
            Route::Direct,
            "trivial task; execution cost below routing cost",
        )
    } else if s.has_existing_solution {
        (
            Route::Reuse,
            "existing solution satisfies most requirements (reuse > extend > build)",
        )
    } else if s.localized_change && !s.repeated_failure {
        (
            Route::Patch,
            "localized change with intact architecture (patch > rewrite)",
        )
    } else if s.external_blocker {
        (
            Route::Partial,
            "external blocker exists; delivering all completed work",
        )
    } else if s.repetitive
        && s.bounded
        && (s.estimated_tokens as f64) > p.delegation_penalty * p.delegation_min_scale
    {
        (
            Route::Delegate,
            "repetitive bounded work; expected savings exceed delegation penalty",
        )
    } else {
        (
            Route::Implement,
            "requirements sufficiently clear; direct completion is cheapest",
        )
    };
    Decision {
        route,
        reason: reason.to_string(),
        signals: s,
    }
}

// ---------------------------------------------------------------------------
// Local heuristics: deterministic signal extraction from a task description.
// These never call a model. Intentionally simple, fast and auditable.
// All patterns are linear-time (Rust's regex crate guarantees no catastrophic
// backtracking — it compiles to a finite automaton).
// ---------------------------------------------------------------------------

static RE_QUESTIONABLE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(maybe|somehow|something|not sure|tbd)\b|\?\?\?").unwrap());
static RE_LOCALIZED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(fix|patch|typo|rename|bump|adjust|tweak|update (a|the|one)|small change|one[- ]line)\b")
        .unwrap()
});
static RE_TRIVIAL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(typo|rename|bump version|add comment|format|reformat|lint fix|sort imports)\b")
        .unwrap()
});
static RE_REPETITIVE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(for (each|every|all)|batch|bulk|repeat|across \d+|all \d+ files|every file)\b")
        .unwrap()
});
static RE_EXTERNAL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(waiting (on|for)|blocked by|api key missing|credentials missing|upstream outage|access denied|permission denied)\b")
        .unwrap()
});
static RE_CONFLICT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(but also must not|contradict|mutually exclusive|both .* and never)\b").unwrap()
});
static RE_SAFETY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(bypass auth|disable security|exfiltrate|leak credentials|ignore policy)\b")
        .unwrap()
});
static RE_ASK_NEEDED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(which one\?|unspecified|missing spec|need to know|clarify)\b").unwrap()
});

/// Derives Signals from a task description plus environment facts supplied by
/// the engine (token estimate, index hit, failure memory, loop detection).
pub fn extract_signals(
    task: &str,
    estimated_tokens: usize,
    index_hit: bool,
    repeated_failure: bool,
    loop_detected: bool,
) -> Signals {
    let t = task.trim();
    let words = t.split_whitespace().count();

    let missing_critical_info = RE_ASK_NEEDED.is_match(t);

    let mut s = Signals {
        estimated_tokens,
        trivial: RE_TRIVIAL.is_match(t) && words <= 40,
        has_existing_solution: index_hit,
        localized_change: RE_LOCALIZED.is_match(t),
        repetitive: RE_REPETITIVE.is_match(t),
        bounded: words <= 200,
        external_blocker: RE_EXTERNAL.is_match(t),
        conflicting_requirements: RE_CONFLICT.is_match(t),
        safety_violation: RE_SAFETY.is_match(t),
        missing_critical_info,
        repeated_failure,
        loop_detected,
        confidence: 0.0,
    };

    // Deterministic confidence: start high, subtract for vagueness markers,
    // extreme brevity and prior failures. Bounded to [0,1].
    let mut conf = 0.9_f64;
    let n = RE_QUESTIONABLE.find_iter(t).take(3).count();
    if n > 0 {
        conf -= 0.35 + 0.25 * (n as f64 - 1.0);
    }
    if words < 3 {
        conf -= 0.4;
    }
    if repeated_failure {
        conf -= 0.2;
    }
    if s.missing_critical_info {
        conf -= 0.5;
    }
    s.confidence = conf.clamp(0.0, 1.0);
    s
}

/// Minimal contract transmitted when work is delegated.
/// No history, no reasoning — conclusions only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationPacket {
    pub task: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    pub acceptance: String,
    pub next_step: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig() -> Signals {
        Signals {
            confidence: 0.9,
            bounded: true,
            ..Default::default()
        }
    }

    #[test]
    fn safety_preempts_everything() {
        let mut s = sig();
        s.safety_violation = true;
        s.trivial = true;
        let d = decide(s, &RouterPolicy::default());
        assert_eq!(d.route, Route::EscalateSafety);
    }

    #[test]
    fn conflict_preempts_ask() {
        let mut s = sig();
        s.conflicting_requirements = true;
        s.missing_critical_info = true;
        assert_eq!(
            decide(s, &RouterPolicy::default()).route,
            Route::EscalateConflict
        );
    }

    #[test]
    fn loop_escalates() {
        let mut s = sig();
        s.loop_detected = true;
        assert_eq!(
            decide(s, &RouterPolicy::default()).route,
            Route::EscalateExternal
        );
    }

    #[test]
    fn low_confidence_asks() {
        let mut s = sig();
        s.confidence = 0.2;
        assert_eq!(decide(s, &RouterPolicy::default()).route, Route::Ask);
    }

    #[test]
    fn trivial_small_is_direct() {
        let mut s = sig();
        s.trivial = true;
        s.estimated_tokens = 100;
        assert_eq!(decide(s, &RouterPolicy::default()).route, Route::Direct);
    }

    #[test]
    fn trivial_large_is_not_direct() {
        let mut s = sig();
        s.trivial = true;
        s.estimated_tokens = 10_000;
        assert_ne!(decide(s, &RouterPolicy::default()).route, Route::Direct);
    }

    #[test]
    fn reuse_beats_patch() {
        let mut s = sig();
        s.has_existing_solution = true;
        s.localized_change = true;
        assert_eq!(decide(s, &RouterPolicy::default()).route, Route::Reuse);
    }

    #[test]
    fn patch_unless_repeated_failure() {
        let mut s = sig();
        s.localized_change = true;
        assert_eq!(decide(s.clone(), &RouterPolicy::default()).route, Route::Patch);
        s.repeated_failure = true;
        assert_eq!(decide(s, &RouterPolicy::default()).route, Route::Implement);
    }

    #[test]
    fn delegate_requires_scale() {
        let mut s = sig();
        s.repetitive = true;
        s.bounded = true;
        s.estimated_tokens = 10_000;
        assert_eq!(decide(s.clone(), &RouterPolicy::default()).route, Route::Delegate);
        s.estimated_tokens = 1000;
        assert_eq!(decide(s, &RouterPolicy::default()).route, Route::Implement);
    }

    #[test]
    fn default_is_implement() {
        assert_eq!(decide(sig(), &RouterPolicy::default()).route, Route::Implement);
    }

    #[test]
    fn extract_trivial_typo() {
        let s = extract_signals("fix typo in README", 50, false, false, false);
        assert!(s.trivial);
        assert!(s.localized_change);
        assert!(s.confidence > 0.8);
    }

    #[test]
    fn extract_vague_lowers_confidence() {
        let s = extract_signals(
            "maybe somehow do something with the thing, not sure",
            50,
            false,
            false,
            false,
        );
        assert!(s.confidence < 0.35);
    }

    #[test]
    fn failure_memory_caps_at_five() {
        let mut st = State::new("t1", "goal");
        for i in 0..8 {
            st.remember_failure(&format!("a{i}"), "r");
        }
        assert_eq!(st.failures.len(), MAX_FAILURE_MEMORY);
        assert_eq!(st.failures[0].action, "a3");
    }
}
