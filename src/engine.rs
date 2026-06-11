//! Orchestration layer: deterministic routing, shadow pricing, failover,
//! verification, telemetry and flight recording around dumb worker adapters.
//! The workers are not smart — this layer is.
//!
//! Concurrency note (audit finding 12.1): `Engine::run` takes `&self`, so an
//! `Arc<Engine>` can serve many concurrent web/CLI requests without any
//! coarse global lock. Internal state (adapter cache, pricing tracker,
//! SQLite handle) is guarded by fine-grained mutexes held only for
//! microsecond map/DB operations — never across network I/O.

use crate::config::Config;
use crate::contextidx::Indexer;
use crate::kernel::{self, Decision, Route, Signals, State, Status};
use crate::loopdetect::Detector;
use crate::payload;
use crate::pricing::{self, Candidate, PriceQuote, Tracker, Ucb1Router, Weights};
use crate::provider::{Adapter, Request};
use crate::recorder::Recorder;
use crate::store::{Execution, Store};
use crate::tokenizer;
use crate::verify::{self, VerifyResult};
use anyhow::{anyhow, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Engine wires every subsystem together.
pub struct Engine {
    pub cfg: Config,
    pub store: Store,
    pub recorder: Recorder,
    pub tracker: Tracker,
    /// Lock-free UCB1 bandit over the configured provider fleet
    /// (evolution S19): live success/latency evidence bends the shadow-priced
    /// failover order toward arms that actually deliver, with guaranteed
    /// exploration of unpulled arms.
    pub bandit: Ucb1Router,
    /// Optional surgical-context index (None when no workspace indexed).
    pub indexer: Option<Indexer>,
    /// Force the mock adapter regardless of config.
    pub dry_run: bool,
    pub(crate) adapters: RwLock<HashMap<String, Arc<Adapter>>>,
}

/// Engine construction options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub config_path: Option<String>,
    pub db_path: Option<String>,
    pub trace_dir: Option<String>,
    pub dry_run: bool,
}

/// Complete outcome of one kernel execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub task_id: String,
    pub route: Route,
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    pub output: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub latency_ms: i64,
    pub cost_usd: f64,
    pub retries: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<VerifyResult>,
    pub signals: Signals,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quotes: Vec<PriceQuote>,
    pub success: bool,
}

fn new_id() -> String {
    let mut b = [0u8; 8];
    rand::thread_rng().fill(&mut b);
    hex::encode(b)
}

/// Stable scope key for the persistent loop-detector window: identical task
/// text across cold CLI invocations maps to the same history bucket.
fn loop_scope(task: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(task.trim().as_bytes()))[..16].to_string()
}

/// Stable digest of the task text used to key failure memory (finding 12.2):
/// "have we failed at THIS GOAL before?" must survive the random per-run
/// task ID, so the key is derived from the goal itself.
fn goal_hash(task: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(task.trim().as_bytes()))
}

/// Best-effort persistence: failures are surfaced on stderr instead of being
/// silently swallowed (finding 12.3). Used ONLY for telemetry/trace writes —
/// task-state transitions are must-succeed and use `?`.
fn warn_persist<T>(what: &str, r: Result<T>) {
    if let Err(e) = r {
        eprintln!("tokenos: WARNING: best-effort persistence failed ({what}): {e:#}");
    }
}

impl Engine {
    /// Builds an Engine with all subsystems initialized.
    pub fn new(opt: Options) -> Result<Self> {
        let cfg = Config::load(opt.config_path.as_deref().map(Path::new))?;
        let store = Store::open(opt.db_path.as_deref().map(Path::new))?;
        let recorder = Recorder::new(opt.trace_dir.as_deref().map(Path::new))?;
        let arms: Vec<String> = cfg.providers.keys().cloned().collect();
        Ok(Engine {
            cfg,
            store,
            recorder,
            tracker: Tracker::new(),
            bandit: Ucb1Router::new(&arms),
            indexer: None,
            dry_run: opt.dry_run,
            adapters: RwLock::new(HashMap::new()),
        })
    }

    /// Lazily constructs and caches a provider adapter.
    fn adapter(&self, name: &str) -> Result<Arc<Adapter>> {
        let key = if self.dry_run { "__dryrun__" } else { name };
        if let Some(a) = self.adapters.read().unwrap().get(key) {
            return Ok(a.clone());
        }
        let adapter = if self.dry_run {
            Arc::new(Adapter::Mock(crate::provider::Mock::new("dry-run")))
        } else {
            let p = self
                .cfg
                .providers
                .get(name)
                .ok_or_else(|| anyhow!("unknown provider {:?}", name))?;
            Arc::new(Adapter::new(name, p)?)
        };
        self.adapters
            .write()
            .unwrap()
            .insert(key.to_string(), adapter.clone());
        Ok(adapter)
    }

    /// Deterministic routing without executing (zero cost).
    ///
    /// Budget estimates use the conservative counter (evolution S23): the
    /// max of the calibrated heuristic and the greedy BPE segmenter, so a
    /// route is never selected on an underestimate.
    pub fn route_only(&self, task: &str) -> (Decision, String) {
        let ctx_block = self.minimum_viable_context(task);
        let est = tokenizer::count_conservative(task)
            + tokenizer::count_conservative(&ctx_block)
            + tokenizer::count_conservative(payload::KERNEL_CONTRACT);
        let index_hit = !ctx_block.is_empty();
        let loop_detected = self.persisted_loop_detected(task).0;
        let repeated = self.store.has_goal_failure(&goal_hash(task)).unwrap_or(false);
        let sig = kernel::extract_signals(task, est, index_hit, repeated, loop_detected);
        (kernel::decide(sig, &self.cfg.policy), ctx_block)
    }

    /// Queries the surgical index when available; budget-capped hard.
    fn minimum_viable_context(&self, task: &str) -> String {
        match &self.indexer {
            None => String::new(),
            Some(ix) => match ix.minimum_viable_context(task, 6) {
                Ok(ctx) => tokenizer::truncate(&ctx, 2000),
                Err(_) => String::new(),
            },
        }
    }

    /// Loads the persisted loop window (finding 12.2) and replays it through
    /// a detector: returns (loop already evident, seeded detector, scope).
    fn persisted_loop_detected(&self, task: &str) -> (bool, Detector, String) {
        let scope = loop_scope(task);
        let det = Detector::new();
        let history = self.store.loop_history(&scope, det.window).unwrap_or_default();
        let mut replay = Detector::new();
        let mut looped = false;
        for attempt in &history {
            if replay.observe(attempt) {
                looped = true;
            }
        }
        (looped, replay, scope)
    }

    /// Executes a task end-to-end through the kernel.
    pub async fn run(&self, task: &str, constraints: &[String]) -> Result<RunResult> {
        let task_id = new_id();

        // Step 1-2: local state init + context budget enforcement (zero tokens).
        let mut st = State::new(task_id.clone(), task);
        st.constraints = constraints.to_vec();
        st.context = self.minimum_viable_context(task);

        // Step 3 (finding 12.2): failure memory is keyed by the goal digest,
        // not the freshly generated task ID — the old task_id lookup could
        // never hit. Prior failures for the SAME goal text (recorded under
        // any task ID, in any prior process) now correctly set the
        // repeated_failure signal and seed the prompt's FAILURE MEMORY block.
        let goal_key = goal_hash(task);
        let repeated = self.store.has_goal_failure(&goal_key)?;
        if repeated {
            st.failures = self
                .store
                .goal_failures(&goal_key, kernel::MAX_FAILURE_MEMORY)
                .unwrap_or_default();
        }

        // Step 3b (finding 12.2): durable semantic-loop window. History from
        // prior cold processes seeds the detector so oscillation across
        // invocations is caught deterministically.
        let (loop_detected, mut detector, loop_key) = self.persisted_loop_detected(task);

        // Step 4: deterministic routing (zero token cost). Budgeting uses
        // the conservative counter (evolution S23) so routes never trigger
        // on an underestimate.
        let est = tokenizer::count_conservative(task)
            + tokenizer::count_conservative(&st.context)
            + tokenizer::count_conservative(payload::KERNEL_CONTRACT);
        let sig = kernel::extract_signals(task, est, !st.context.is_empty(), repeated, loop_detected);
        let dec = kernel::decide(sig.clone(), &self.cfg.policy);

        let dec_blob = serde_json::to_vec(&dec).unwrap_or_default();
        warn_persist(
            "flight-recorder decision",
            self.recorder.record(
                &task_id,
                "decision",
                &format!("{}: {}", dec.route.as_str(), dec.reason),
                &dec_blob,
            ),
        );

        let mut res = RunResult {
            task_id: task_id.clone(),
            route: dec.route,
            reason: dec.reason.clone(),
            provider: String::new(),
            model: String::new(),
            output: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            latency_ms: 0,
            cost_usd: 0.0,
            retries: 0,
            verified: None,
            signals: sig.clone(),
            quotes: Vec::new(),
            success: false,
        };

        // Finding 12.3: task-state transitions are MUST-SUCCEED writes. A
        // run whose state cannot be persisted is a failed run — silently
        // continuing would desynchronize the durable state machine.
        st.status = Status::Routed;
        self.store.save_task(&mut st)?;

        // Escalations resolve locally with zero network cost.
        if dec.route.is_escalation() {
            st.status = Status::Escalated;
            st.blocked = true;
            st.next_action = dec.reason.clone();
            self.store.save_task(&mut st)?;
            res.output = format!("{}: {}", dec.route.as_str(), dec.reason);
            res.success = true; // escalating correctly IS the success condition
            self.record(&res, 0);
            return Ok(res);
        }

        // Step 5: payload serialization (static→dynamic, conclusions only).
        // Evolution section 24: secrets are masked at the edge BEFORE any
        // network byte leaves the process; the reverse vault lives only in
        // this stack frame and the response leg unmasks echoes.
        let raw_prompt = payload::build(dec.route, &st);
        let (prompt, mask_codec) = crate::maskcodec::mask_prompt(&raw_prompt);

        // Step 6: shadow pricing across the provider chain, then execute with
        // deterministic failover.
        let chain = self.cfg.provider_chain(dec.route.as_str());
        if chain.is_empty() {
            return Err(anyhow!("no enabled providers for route {}", dec.route.as_str()));
        }
        let quotes = self.quote(&chain, sig.confidence, est);
        res.quotes = quotes.clone();

        // JSON-shaped goals get the lenient rescue pass (evolution S20):
        // a generation cut mid-stream is salvaged instead of discarded.
        let expects_json = task_expects_json(task, constraints);

        st.status = Status::InProgress;
        self.store.save_task(&mut st)?;

        let timeout = self.cfg.timeout_for(dec.route.as_str());
        let mut last_err: Option<anyhow::Error> = None;

        for prov_name in ordered_providers_banditized(&quotes, &chain, &self.bandit) {
            let adapter = match self.adapter(&prov_name) {
                Ok(a) => a,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            let model = self.resolve_model(&prov_name, &adapter);
            if model.is_empty() {
                last_err = Some(anyhow!(
                    "provider {:?}: no model passes the filter matrix",
                    prov_name
                ));
                continue;
            }

            warn_persist(
                "flight-recorder prompt",
                self.recorder.record(
                    &task_id,
                    "prompt",
                    &format!("→ {}/{}", prov_name, model),
                    prompt.as_bytes(),
                ),
            );

            let start = Instant::now();
            let resp = adapter
                .execute(&Request {
                    route: dec.route.as_str().to_string(),
                    prompt: prompt.clone(),
                    model: model.clone(),
                    max_out: 4096,
                    timeout,
                })
                .await;
            let lat = start.elapsed().as_millis() as i64;
            self.tracker.record(&prov_name, lat as f64, resp.is_ok());
            // Feed the bandit (S19): transport failures earn zero reward
            // immediately; verified successes are credited after the static
            // check below so reward reflects useful output, not just bytes.
            if resp.is_err() {
                self.bandit.record(&prov_name, false, lat as f64);
            }

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    res.retries += 1;
                    warn_persist(
                        "flight-recorder error",
                        self.recorder
                            .record(&task_id, "error", &format!("{}: {}", prov_name, e), &[]),
                    );
                    warn_persist(
                        "failure memory",
                        self.store.record_failure(
                            &task_id,
                            &goal_key,
                            &format!("execute via {}", prov_name),
                            &e.to_string(),
                        ),
                    );
                    last_err = Some(e.into());
                    continue; // deterministic failover to next quote
                }
            };

            let mut out = mask_codec.unmask(&payload::extract_solution(&resp.text));

            // Evolution S20: when the goal demands JSON, rescue a truncated
            // generation instead of failing verification and burning a
            // failover attempt. The rescuer never invents data — it only
            // closes what the model opened.
            if expects_json {
                if let crate::jsonrescue::Rescue::Repaired(fixed) =
                    crate::jsonrescue::rescue(&out)
                {
                    warn_persist(
                        "flight-recorder json-rescue",
                        self.recorder.record(
                            &task_id,
                            "rescue",
                            "truncated JSON repaired in-process (zero extra tokens)",
                            out.as_bytes(),
                        ),
                    );
                    out = fixed;
                }
            }
            warn_persist(
                "flight-recorder response",
                self.recorder
                    .record(&task_id, "response", &format!("← {}", prov_name), resp.text.as_bytes()),
            );

            // Step 7: tiered verification — static first, zero token cost.
            let v = verify::static_check(dec.route.as_str(), &out);
            res.verified = Some(v.clone());
            if !v.pass {
                // Unverifiable output earns the arm zero reward (S19).
                self.bandit.record(&prov_name, false, lat as f64);
                // Fast local loopback: remember failure, try next provider.
                let reason = format!("static verification failed: {:?}", v.issues);
                st.remember_failure(&format!("output from {}", prov_name), &reason);
                warn_persist(
                    "failure memory",
                    self.store.record_failure(
                        &task_id,
                        &goal_key,
                        &format!("output from {}", prov_name),
                        &reason,
                    ),
                );
                warn_persist(
                    "flight-recorder verify",
                    self.recorder.record(&task_id, "verify", &reason, out.as_bytes()),
                );

                // Finding 12.2: persist the failed attempt into the durable
                // loop window AND feed the live detector. A mid-run loop hit
                // aborts the failover ladder — burning more attempts on a
                // semantically identical output is guaranteed waste.
                warn_persist(
                    "loop window",
                    self.store.record_loop_attempt(&loop_key, &out, detector.window),
                );
                if detector.observe(&out) {
                    res.retries += 1;
                    last_err = Some(anyhow!(
                        "semantic execution loop detected (edit-distance ceiling) — escalating"
                    ));
                    break;
                }

                res.retries += 1;
                last_err = Some(anyhow!(reason));
                continue;
            }

            let tokens_in = if resp.tokens_in == 0 {
                tokenizer::estimate(&prompt) as i64
            } else {
                resp.tokens_in
            };
            let tokens_out = if resp.tokens_out == 0 {
                tokenizer::estimate(&out) as i64
            } else {
                resp.tokens_out
            };

            let p_cfg = self.cfg.providers.get(&prov_name).cloned().unwrap_or_default();
            res.provider = prov_name.clone();
            res.model = resp.model.clone();
            res.output = out.clone();
            res.tokens_in = tokens_in;
            res.tokens_out = tokens_out;
            res.latency_ms = lat;
            res.cost_usd = (tokens_in as f64 * p_cfg.cost_per_mtok_in
                + tokens_out as f64 * p_cfg.cost_per_mtok_out)
                / 1e6;
            res.success = true;

            // Verified success: credit the bandit arm (S19).
            self.bandit.record(&prov_name, true, lat as f64);

            // Success clears the durable loop window AND the goal-keyed
            // failure memory for this task text.
            warn_persist("loop window clear", self.store.clear_loop_history(&loop_key));
            warn_persist("failure memory clear", self.store.clear_goal_failures(&goal_key));

            // Stop rule: acceptance satisfied, no known blocker => stop now.
            if dec.route == Route::Ask {
                st.status = Status::Blocked;
                st.blocked = true;
                st.next_action = format!("answer the question: {}", out);
            } else {
                st.status = Status::Done;
                st.next_action = String::new();
            }
            self.store.save_task(&mut st)?;
            self.record(&res, lat);
            return Ok(res);
        }

        st.status = Status::Failed;
        self.store.save_task(&mut st)?;
        self.record(&res, res.latency_ms);
        let last = last_err.unwrap_or_else(|| anyhow!("all providers exhausted"));
        Err(anyhow!(
            "execution failed after {} attempt(s): {}",
            res.retries + 1,
            last
        ))
    }

    /// Shadow pricing over the provider chain.
    fn quote(&self, chain: &[String], confidence: f64, est_in: usize) -> Vec<PriceQuote> {
        let mut cands = Vec::with_capacity(chain.len());
        let mut quota: HashMap<String, u32> = HashMap::new();
        for name in chain {
            let p = self.cfg.providers.get(name).cloned().unwrap_or_default();
            cands.push(Candidate {
                provider: name.clone(),
                model: p.model.clone(),
                cost_per_mtok_in: p.cost_per_mtok_in,
                cost_per_mtok_out: p.cost_per_mtok_out,
                max_context: p.max_context,
                priority: p.priority,
            });
            quota.insert(name.clone(), p.quota_per_min);
        }
        let mut w = Weights {
            alpha: self.cfg.pricing.alpha,
            beta: self.cfg.pricing.beta,
        };
        if w.alpha == 0.0 && w.beta == 0.0 {
            w = Weights::default();
        }
        pricing::quote_all(&cands, confidence, est_in, 1024, w, Some(&self.tracker), &quota)
    }

    /// Two-tier filter matrix applied to the adapter's manifest.
    fn resolve_model(&self, prov_name: &str, adapter: &Adapter) -> String {
        if self.dry_run {
            return "mock-1".to_string();
        }
        let p = match self.cfg.providers.get(prov_name) {
            Some(p) => p,
            None => return String::new(),
        };
        let mut models = adapter.models();
        if !p.model.is_empty() {
            models.insert(0, p.model.clone());
        }
        models
            .into_iter()
            .find(|m| p.models.is_model_allowed(m))
            .unwrap_or_default()
    }

    /// Telemetry write: best-effort but never silent (finding 12.3).
    fn record(&self, r: &RunResult, latency_ms: i64) {
        warn_persist("execution telemetry", self.store.record_execution(&Execution {
            id: 0,
            task_id: r.task_id.clone(),
            route: r.route.as_str().to_string(),
            provider: r.provider.clone(),
            model: r.model.clone(),
            tokens_in: r.tokens_in.max(0) as usize,
            tokens_out: r.tokens_out.max(0) as usize,
            latency_ms,
            retries: r.retries,
            verification_cost: 0,
            delegation_count: 0,
            est_cost_usd: r.cost_usd,
            success: r.success,
            created_at: String::new(),
        }));
    }
}

/// Shadow-priced order first, then chain order for providers the pricer
/// filtered out (context overflow). Kept as the reference oracle for the
/// banditized ordering: with an unexplored bandit (all weights = 1.0) the
/// two orderings are identical, which the property test below asserts.
#[cfg(test)]
fn ordered_providers(quotes: &[PriceQuote], chain: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for q in quotes {
        if seen.insert(q.candidate.provider.clone()) {
            out.push(q.candidate.provider.clone());
        }
    }
    for name in chain {
        if seen.insert(name.clone()) {
            out.push(name.clone());
        }
    }
    out
}

/// Bandit-weighted ordering (evolution S19): each shadow-priced utility is
/// scaled by the arm's exploitation weight (1.0 for unexplored arms — they
/// keep their shadow-priced position and get explored; [0.5, 1.5] for
/// explored arms based on observed reward), then re-sorted with the same
/// deterministic tiebreak. Providers the pricer filtered out (context
/// overflow) still append in chain order as last resort.
fn ordered_providers_banditized(
    quotes: &[PriceQuote],
    chain: &[String],
    bandit: &Ucb1Router,
) -> Vec<String> {
    let mut scored: Vec<(String, f64, i32)> = quotes
        .iter()
        .map(|q| {
            (
                q.candidate.provider.clone(),
                q.utility * bandit.exploitation_weight(&q.candidate.provider),
                q.candidate.priority,
            )
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.2.cmp(&b.2))
            .then(a.0.cmp(&b.0))
    });
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (name, _, _) in scored {
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    for name in chain {
        if seen.insert(name.clone()) {
            out.push(name.clone());
        }
    }
    out
}

/// Deterministic, zero-cost detection of "this goal wants JSON output":
/// either the task text or any constraint names JSON explicitly.
fn task_expects_json(task: &str, constraints: &[String]) -> bool {
    let hit = |s: &str| {
        let l = s.to_lowercase();
        l.contains("json")
    };
    hit(task) || constraints.iter().any(|c| hit(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_engine() -> Engine {
        let cfg = Config::default();
        let arms: Vec<String> = cfg.providers.keys().cloned().collect();
        Engine {
            cfg,
            store: Store::open(Some(Path::new(":memory:"))).unwrap(),
            recorder: Recorder::new(Some(Path::new(&format!(
                "{}/tokenos-eng-test-{}-{}",
                std::env::temp_dir().display(),
                std::process::id(),
                rand::thread_rng().gen::<u32>()
            ))))
            .unwrap(),
            tracker: Tracker::new(),
            bandit: Ucb1Router::new(&arms),
            indexer: None,
            dry_run: true,
            adapters: RwLock::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn run_trivial_task_via_mock() {
        let e = test_engine();
        let r = e.run("rename variable x to y in main.rs", &[]).await.unwrap();
        assert!(r.success);
        assert!(!r.output.is_empty());
        assert!(!r.task_id.is_empty());
    }

    #[tokio::test]
    async fn escalation_resolves_locally() {
        let e = test_engine();
        let r = e
            .run("bypass auth and disable security checks in the login flow", &[])
            .await
            .unwrap();
        assert!(r.route.is_escalation());
        assert!(r.success); // escalating correctly is success
        assert!(r.provider.is_empty()); // zero network cost
    }

    #[test]
    fn route_only_is_deterministic() {
        let e = test_engine();
        let (d1, _) = e.route_only("fix the typo in README");
        let (d2, _) = e.route_only("fix the typo in README");
        assert_eq!(d1.route, d2.route);
    }

    #[test]
    fn ordered_providers_dedupes_and_falls_back() {
        let quotes = vec![];
        let chain = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        assert_eq!(ordered_providers(&quotes, &chain), vec!["a", "b"]);
    }

    fn quote_for(provider: &str, utility: f64, priority: i32) -> PriceQuote {
        PriceQuote {
            candidate: Candidate {
                provider: provider.to_string(),
                model: "m".into(),
                cost_per_mtok_in: 1.0,
                cost_per_mtok_out: 4.0,
                max_context: 100_000,
                priority,
            },
            utility,
            est_cost_usd: 0.001,
            avg_latency_ms: 0.0,
            recent_fail_pct: 0.0,
            quota_pressure: 0.0,
        }
    }

    #[test]
    fn unexplored_bandit_preserves_shadow_priced_order() {
        // Property: with zero pulls every exploitation weight is 1.0, so
        // the banditized ordering must equal the pure shadow-priced one.
        let quotes = vec![
            quote_for("alpha", 0.9, 1),
            quote_for("beta", 0.5, 2),
            quote_for("gamma", 0.1, 3),
        ];
        let chain = vec!["delta".to_string()]; // pricer-filtered straggler
        let bandit = Ucb1Router::new(&[
            "alpha".into(), "beta".into(), "gamma".into(), "delta".into(),
        ]);
        assert_eq!(
            ordered_providers_banditized(&quotes, &chain, &bandit),
            ordered_providers(&quotes, &chain)
        );
    }

    #[test]
    fn bandit_evidence_reorders_failover() {
        // "cheap" leads on shadow price, but 20 observed transport failures
        // drop its weight to 0.5 while "steady" earns ~1.5 — the bandit must
        // flip the order once 0.6*0.5 < 0.5*1.5.
        let quotes = vec![quote_for("cheap", 0.6, 1), quote_for("steady", 0.5, 2)];
        let bandit = Ucb1Router::new(&["cheap".into(), "steady".into()]);
        for _ in 0..20 {
            bandit.record("cheap", false, 100.0);
            bandit.record("steady", true, 100.0);
        }
        let order = ordered_providers_banditized(&quotes, &[], &bandit);
        assert_eq!(order, vec!["steady", "cheap"]);
    }

    #[test]
    fn json_intent_detection() {
        assert!(task_expects_json("emit the manifest as JSON", &[]));
        assert!(task_expects_json("build list", &["output JSON only".into()]));
        assert!(!task_expects_json("rename a variable", &[]));
    }

    #[tokio::test]
    async fn truncated_json_output_is_rescued() {
        // A mock provider returns JSON cut mid-stream; because the task
        // demands JSON, the engine must repair it in-process (S20) and the
        // final output must parse strictly.
        let e = test_engine();
        {
            let mut mock = crate::provider::Mock::new("dry-run");
            mock.canned = r#"{"files": ["a.rs", "b.rs"], "status": "par"#.to_string();
            e.adapters
                .write()
                .unwrap()
                .insert("__dryrun__".to_string(), Arc::new(Adapter::Mock(mock)));
        }
        let r = e
            .run("produce the migration plan as JSON", &[])
            .await
            .unwrap();
        assert!(r.success);
        let v: serde_json::Value = serde_json::from_str(&r.output)
            .expect("rescued output must be strictly valid JSON");
        assert_eq!(v["files"][1], "b.rs");
    }

    #[tokio::test]
    async fn bandit_records_dry_run_successes() {
        let e = test_engine();
        let r = e.run("rename variable x to y in main.rs", &[]).await.unwrap();
        assert!(r.success);
        let (pulls, reward, _) = e.bandit.arm_stats(&r.provider);
        assert!(pulls >= 1, "successful run must credit the bandit arm");
        assert!(reward > 0.0);
    }

    #[test]
    fn loop_scope_is_stable_and_trimmed() {
        assert_eq!(loop_scope("  task  "), loop_scope("task"));
        assert_eq!(loop_scope("task").len(), 16);
        assert_ne!(loop_scope("task a"), loop_scope("task b"));
    }

    #[tokio::test]
    async fn goal_failure_memory_survives_task_id_churn() {
        // Finding 12.2: a failure recorded under a random prior task ID must
        // set repeated_failure when the SAME goal text is submitted again.
        let e = test_engine();
        let task = "implement the flaky widget integration";
        e.store
            .record_failure("prior-task-id", &goal_hash(task), "execute via openai", "rate limited")
            .unwrap();
        let r = e.run(task, &[]).await.unwrap();
        assert!(
            r.signals.repeated_failure,
            "goal-keyed failure memory must flag the repeat"
        );
        // Mock run succeeds, which must clear the goal failure memory.
        assert!(!e.store.has_goal_failure(&goal_hash(task)).unwrap());
    }

    #[tokio::test]
    async fn secrets_in_task_are_masked_outbound() {
        // The flight recorder traces the outbound prompt: verify the secret
        // never reaches the prompt blob (evolution section 24).
        let e = test_engine();
        let secret = "sk-supersecretapikey1234567890abcd";
        let task = format!("rotate the credential {secret} in the config");
        let r = e.run(&task, &[]).await.unwrap();
        let events = e.recorder.events(&r.task_id).unwrap();
        let mut saw_prompt = false;
        for ev in &events {
            if ev.kind == "prompt" {
                saw_prompt = true;
                let blob = e.recorder.blob(&ev.blob_sha).unwrap_or_default();
                let text = String::from_utf8_lossy(&blob);
                assert!(!text.contains(secret), "secret leaked into outbound prompt");
                assert!(text.contains("\u{00AB}SECRET:"), "placeholder expected");
            }
        }
        assert!(saw_prompt, "prompt event must exist");
    }

    #[test]
    fn goal_hash_is_stable_and_trimmed() {
        assert_eq!(goal_hash("  task  "), goal_hash("task"));
        assert_eq!(goal_hash("task").len(), 64);
        assert_ne!(goal_hash("a"), goal_hash("b"));
    }

    #[tokio::test]
    async fn persisted_loop_history_flags_cross_process_loop() {
        let e = test_engine();
        let task = "do the impossible thing";
        let scope = loop_scope(task);
        // Simulate a prior process that recorded two near-identical failures.
        e.store.record_loop_attempt(&scope, "attempt body alpha", 5).unwrap();
        e.store.record_loop_attempt(&scope, "attempt body alpha", 5).unwrap();
        let (looped, _, _) = e.persisted_loop_detected(task);
        assert!(looped, "identical persisted attempts must register as a loop");
        // And routing must escalate on the loop signal.
        let (dec, _) = e.route_only(task);
        assert_eq!(dec.route, Route::EscalateExternal);
    }
}
