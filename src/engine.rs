//! Orchestration layer: deterministic routing, shadow pricing, failover,
//! verification, telemetry and flight recording around dumb worker adapters.
//! The workers are not smart — this layer is.
//!
//! Concurrency invariant: `Engine::run` takes `&self`, so an
//! `Arc<Engine>` can serve many concurrent web/CLI requests without any
//! coarse global lock. Internal state (adapter cache, pricing tracker,
//! SQLite handle) is guarded by fine-grained mutexes held only for
//! microsecond map/DB operations — never across network I/O.

use crate::config::Config;
use crate::contextidx::Indexer;
use crate::kernel::{self, Decision, Route, Signals, State, Status};
use crate::loopdetect::Detector;
use crate::payload;
use crate::pricing::{self, Candidate, DriftWatchdog, PriceQuote, Tracker, Ucb1Router, Weights};
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
    /// Lock-free UCB1 bandit over the configured provider fleet: live
    /// success/latency evidence bends the shadow-priced
    /// failover order toward arms that actually deliver, with guaranteed
    /// exploration of unpulled arms.
    pub bandit: Ucb1Router,
    /// Estimator drift watchdog: EWMA of actual/estimated
    /// token ratios per provider. Drift outside the trusted band means every
    /// shadow price is silently degrading — surfaced in telemetry.
    pub drift: DriftWatchdog,
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

/// Stable digest of the task text used to key failure memory:
/// "have we failed at THIS GOAL before?" must survive the random per-run
/// task ID, so the key is derived from the goal itself.
/// Cache key for the verified solution cache: the goal
/// digest extended with the constraint set, so the same goal under different
/// constraints never collides.
fn solution_cache_key(task: &str, constraints: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(task.trim().as_bytes());
    for c in constraints {
        h.update(b"\x1f");
        h.update(c.trim().as_bytes());
    }
    format!("{:x}", h.finalize())
}

fn goal_hash(task: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(task.trim().as_bytes()))
}

fn clarifying_question(sig: &Signals) -> String {
    if sig.missing_critical_info {
        "Which exact target, option, or missing requirement should TokenOS use before executing this task?"
            .to_string()
    } else if sig.confidence < 0.2 {
        "What concrete outcome, target, and acceptance criteria should TokenOS use before executing this task?"
            .to_string()
    } else {
        "What missing detail should TokenOS resolve before executing this task safely and cheaply?"
            .to_string()
    }
}

/// Best-effort persistence: failures are surfaced on stderr instead of being
/// silently swallowed. Used ONLY for telemetry/trace writes —
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
        let owner_only_permissions = cfg.security.owner_only_permissions;
        let store = Store::open_with_owner_permissions(
            opt.db_path.as_deref().map(Path::new),
            owner_only_permissions,
        )?;
        let recorder = Recorder::new_with_owner_permissions(
            opt.trace_dir.as_deref().map(Path::new),
            owner_only_permissions,
        )?;
        let arms: Vec<String> = cfg.providers.keys().cloned().collect();
        let engine = Engine {
            cfg,
            store,
            recorder,
            tracker: Tracker::new(),
            bandit: Ucb1Router::new(&arms),
            drift: DriftWatchdog::new(),
            indexer: None,
            dry_run: opt.dry_run,
            adapters: RwLock::new(HashMap::new()),
        };
        // Startup pruning of telemetry and traces.
        if engine.cfg.security.retention_days > 0 {
            let _ = engine
                .store
                .prune_old_records(engine.cfg.security.retention_days);
            let _ = engine
                .recorder
                .prune_old_traces(engine.cfg.security.retention_days);
        }

        // Backfill routing observations and provider health from the most
        // durable evidence available.
        let hydrated_bandit = match engine.store.load_bandit_states() {
            Ok(states) if !states.is_empty() => {
                let mut total_pulls = 0;
                for s in &states {
                    engine
                        .bandit
                        .set_state(&s.provider, s.pulls, s.reward_sum, s.latency_sum_ms);
                    total_pulls += s.pulls;
                }
                engine.bandit.set_total_pulls(total_pulls);
                true
            }
            _ => false,
        };

        // Attempt rows include failed failover legs; older databases may only have final execution rows.
        let hydrated_from_attempts = match engine.store.list_attempts(1000) {
            Ok(mut attempts) => {
                if attempts.is_empty() {
                    false
                } else {
                    attempts.reverse();
                    for attempt in &attempts {
                        if !hydrated_bandit {
                            engine.bandit.record(
                                &attempt.provider,
                                attempt.success,
                                attempt.latency_ms as f64,
                            );
                        }
                        engine.tracker.record(
                            &attempt.provider,
                            attempt.latency_ms as f64,
                            attempt.success,
                        );
                    }
                    true
                }
            }
            Err(e) => {
                eprintln!(
                    "tokenos: WARNING: provider attempt hydration failed; falling back to final executions: {e:#}"
                );
                false
            }
        };
        if !hydrated_from_attempts {
            match engine.store.list_executions(1000) {
                Ok(mut execs_chronological) => {
                    execs_chronological.reverse();
                    for exec in &execs_chronological {
                        if !hydrated_bandit {
                            engine.bandit.record(
                                &exec.provider,
                                exec.success,
                                exec.latency_ms as f64,
                            );
                        }
                        engine
                            .tracker
                            .record(&exec.provider, exec.latency_ms as f64, exec.success);
                    }
                }
                Err(e) => {
                    eprintln!("tokenos: WARNING: final execution hydration failed: {e:#}");
                }
            }
        }

        // Backfill drift ratios from the store to make decisions durable
        if let Ok(ratios) = engine.store.load_drift_ratios() {
            for (provider, (ewma, samples)) in ratios {
                engine.drift.set_ratio(&provider, ewma, samples);
            }
        }

        Ok(engine)
    }

    /// Computes the cache key incorporating workspace hash if indexer is present.
    pub fn solution_cache_key(&self, task: &str, constraints: &[String]) -> String {
        let mut key = solution_cache_key(task, constraints);
        if let Some(ix) = &self.indexer {
            if let Ok(wh) = ix.workspace_hash() {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(key.as_bytes());
                h.update(b"\x1f");
                h.update(wh.as_bytes());
                key = format!("{:x}", h.finalize());
            }
        }
        key
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
    /// Budget estimates use the conservative counter: the
    /// max of the calibrated heuristic and the greedy BPE segmenter, so a
    /// route is never selected on an underestimate.
    pub fn route_only(&self, task: &str) -> (Decision, String) {
        self.route_only_with_constraints(task, &[])
    }

    /// Deterministic routing with the same cache signal used by execution.
    /// Workspace context is prompt context only; REUSE requires an exact,
    /// verified, replayable solution-cache hit for the goal+constraints pair.
    pub fn route_only_with_constraints(
        &self,
        task: &str,
        constraints: &[String],
    ) -> (Decision, String) {
        self.route_only_with_policy_constraints(task, constraints, &self.cfg.policy)
    }

    pub fn route_only_with_policy_constraints(
        &self,
        task: &str,
        constraints: &[String],
        policy: &crate::kernel::RouterPolicy,
    ) -> (Decision, String) {
        let ctx_block = self.minimum_viable_context(task);
        let est = tokenizer::count_conservative(task)
            + tokenizer::count_conservative(&ctx_block)
            + tokenizer::count_conservative(payload::KERNEL_CONTRACT);
        let has_existing_solution =
            self.replayable_cache_hit(&self.solution_cache_key(task, constraints), task);
        let loop_detected = self.persisted_loop_detected(task).0;
        let repeated = self
            .store
            .has_goal_failure(&goal_hash(task))
            .unwrap_or(false);
        let sig =
            kernel::extract_signals(task, est, has_existing_solution, repeated, loop_detected);
        let mut dec = kernel::decide(sig.clone(), policy);

        // P10: Opt-in learned routing classifier fallback
        if policy.opt_in_learned_routing && sig.confidence >= 0.2 && sig.confidence < 0.5 {
            if let Ok(Some((route_str, sim))) = self.store.get_similar_successful_route(task, 0.3) {
                let matched_route = match route_str.as_str() {
                    "DIRECT" => Some(kernel::Route::Direct),
                    "REUSE" => Some(kernel::Route::Reuse),
                    "PATCH" => Some(kernel::Route::Patch),
                    "IMPLEMENT" => Some(kernel::Route::Implement),
                    "PARTIAL" => Some(kernel::Route::Partial),
                    "DELEGATE" => Some(kernel::Route::Delegate),
                    "ASK" => Some(kernel::Route::Ask),
                    _ => None,
                };
                if let Some(r) = matched_route {
                    dec.route = r;
                    dec.reason = format!(
                        "learned classifier override: matches past successful task with similarity {:.2}",
                        sim
                    );
                }
            }
        }

        (dec, ctx_block)
    }

    /// Queries the surgical index when available; budget-capped hard.
    fn minimum_viable_context(&self, task: &str) -> String {
        match &self.indexer {
            None => String::new(),
            Some(ix) => match ix.minimum_viable_context(task, 2000) {
                Ok(ctx) => ctx,
                Err(e) => {
                    warn_persist::<String>("index context err", Err(e));
                    String::new()
                }
            },
        }
    }

    fn replayable_cache_hit(&self, cache_key: &str, task: &str) -> bool {
        if !self.cfg.policy.reuse_cache {
            return false;
        }
        let exact = self
            .store
            .peek_cached_solution(cache_key)
            .map(|hit| {
                hit.map(|(_, out, _)| !crate::maskcodec::contains_placeholder(&out))
                    .unwrap_or(false)
            })
            .unwrap_or(false);

        if exact {
            true
        } else if self.cfg.policy.semantic_cache_threshold > 0.0 {
            self.store
                .peek_semantic_cached_solution(task, self.cfg.policy.semantic_cache_threshold)
                .map(|hit| {
                    hit.map(|(_, out, _)| !crate::maskcodec::contains_placeholder(&out))
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        } else {
            false
        }
    }

    fn record_event(
        &self,
        task_id: &str,
        kind: &str,
        summary: &str,
        payload: &[u8],
    ) -> Result<String> {
        if self.cfg.security.disable_traces {
            return Ok(String::new());
        }
        let sha = self.recorder.record(task_id, kind, summary, payload)?;
        let blob_ref = if sha.is_empty() {
            "journal".to_string()
        } else {
            format!("sha256:{sha}")
        };
        self.store.record_trace(task_id, kind, &blob_ref)?;
        Ok(sha)
    }

    /// Loads the persisted loop window and replays it through
    /// a detector: returns (loop already evident, seeded detector, scope).
    fn persisted_loop_detected(&self, task: &str) -> (bool, Detector, String) {
        let scope = loop_scope(task);
        let det = Detector::new();
        let history = self
            .store
            .loop_history(&scope, det.window)
            .unwrap_or_default();
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

        // Enforce configured daily and monthly spend limits before execution.
        if self.cfg.security.daily_spend_limit_usd > 0.0 {
            if let Ok(spend) = self.store.aggregate_spend_usd(1) {
                if spend >= self.cfg.security.daily_spend_limit_usd {
                    return Err(anyhow::anyhow!(
                        "Daily spend limit of ${:.2} exceeded (current spend: ${:.2})",
                        self.cfg.security.daily_spend_limit_usd,
                        spend
                    ));
                }
            }
        }
        if self.cfg.security.monthly_spend_limit_usd > 0.0 {
            if let Ok(spend) = self.store.aggregate_spend_usd(30) {
                if spend >= self.cfg.security.monthly_spend_limit_usd {
                    return Err(anyhow::anyhow!(
                        "Monthly spend limit of ${:.2} exceeded (current spend: ${:.2})",
                        self.cfg.security.monthly_spend_limit_usd,
                        spend
                    ));
                }
            }
        }

        // Step 1-2: local state init + context budget enforcement (zero tokens).
        let mut st = State::new(task_id.clone(), task);
        st.constraints = constraints.to_vec();
        st.context = self.minimum_viable_context(task);

        // Failure memory is keyed by the goal digest,
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

        // Durable semantic-loop window. History from
        // prior cold processes seeds the detector so oscillation across
        // invocations is caught deterministically.
        let (loop_detected, mut detector, loop_key) = self.persisted_loop_detected(task);
        let cache_key = self.solution_cache_key(task, constraints);
        let has_existing_solution = self.replayable_cache_hit(&cache_key, task);

        // Step 4: deterministic routing (zero token cost). Budgeting uses
        // the conservative counter so routes never trigger
        // on an underestimate.
        let est = tokenizer::count_conservative(task)
            + tokenizer::count_conservative(&st.context)
            + tokenizer::count_conservative(payload::KERNEL_CONTRACT);
        let sig =
            kernel::extract_signals(task, est, has_existing_solution, repeated, loop_detected);
        let dec = kernel::decide(sig.clone(), &self.cfg.policy);

        let dec_blob = serde_json::to_vec(&dec).unwrap_or_default();
        warn_persist(
            "flight-recorder decision",
            self.record_event(
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

        // Task-state transitions are MUST-SUCCEED writes. A
        // run whose state cannot be persisted is a failed run — silently
        // continuing would desynchronize the durable state machine.
        st.status = Status::Routed;
        self.store.save_task(&mut st)?;

        // ASK resolves locally with zero network cost. A clarifying question is
        // the completed action for this route, not something to outsource to a
        // provider after the router has already decided information is missing.
        if dec.route == Route::Ask {
            let question = clarifying_question(&sig);
            let v = verify::static_check(dec.route.as_str(), &question);
            st.status = Status::Blocked;
            st.blocked = true;
            st.next_action = format!("answer the question: {question}");
            self.store.save_task(&mut st)?;
            warn_persist(
                "flight-recorder ask",
                self.record_event(
                    &task_id,
                    "ask",
                    "local clarifying question emitted at zero token cost",
                    question.as_bytes(),
                ),
            );
            res.output = question;
            res.verified = Some(v);
            res.success = true;
            self.record(&res, 0);
            return Ok(res);
        }

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

        // Verified solution cache. An exact goal+constraints
        // re-request is served from the durable cache at ZERO tokens — the
        // cheapest possible execution. Only verified successes are admitted
        // (below), so a cache hit is by construction a verified answer.
        // ASK is excluded: a question is a request for input, not a solution.
        if self.cfg.policy.reuse_cache && dec.route != Route::Ask {
            let mut cache_hit = None;
            if let Ok(Some((cached_route, cached_out, cached_tier))) =
                self.store.cached_solution(&cache_key)
            {
                if crate::maskcodec::contains_placeholder(&cached_out) {
                    warn_persist(
                        "solution cache evict",
                        self.store.evict_solution(&cache_key),
                    );
                } else {
                    cache_hit = Some((cached_route, cached_out, cached_tier, "solution-cache"));
                }
            } else if self.cfg.policy.semantic_cache_threshold > 0.0 {
                if let Ok(Some((cached_route, cached_out, cached_tier))) = self
                    .store
                    .semantic_cached_solution(task, self.cfg.policy.semantic_cache_threshold)
                {
                    if !crate::maskcodec::contains_placeholder(&cached_out) {
                        cache_hit = Some((cached_route, cached_out, cached_tier, "semantic-cache"));
                    }
                }
            }

            if let Some((cached_route, cached_out, cached_tier, source)) = cache_hit {
                warn_persist(
                    "flight-recorder cache",
                    self.record_event(
                        &task_id,
                        "cache",
                        &format!("verified solution served from {source} (route {cached_route}, zero tokens)"),
                        cached_out.as_bytes(),
                    ),
                );
                st.status = Status::Done;
                st.next_action = String::new();
                self.store.save_task(&mut st)?;
                res.provider = "cache".into();
                res.model = source.into();
                res.output = cached_out;
                res.verified = Some(VerifyResult {
                    pass: true,
                    tier: cached_tier,
                    issues: vec![],
                    cost_tokens: 0,
                    score: 1.0,
                });
                res.success = true;
                self.record(&res, 0);
                return Ok(res);
            }
        }

        // Step 5: payload serialization (static→dynamic, conclusions only).
        // Secrets are masked at the edge BEFORE any
        // network byte leaves the process; the reverse vault lives only in
        // this stack frame and the response leg unmasks echoes.
        let raw_prompt = payload::build(dec.route, &st);
        let (prompt, mask_codec) = crate::maskcodec::mask_prompt(&raw_prompt);

        // Step 6: shadow pricing across the provider chain, then execute with
        // deterministic failover.
        let chain = self.cfg.provider_chain(dec.route.as_str());
        if chain.is_empty() {
            return Err(anyhow!(
                "no enabled providers for route {}",
                dec.route.as_str()
            ));
        }
        // The quote includes the route's output budget so the context-fit
        // check covers prompt + allowed output, not just the prompt.
        let est_out = dec.route.max_output_tokens().max(0) as usize;
        let quotes = self.quote(&chain, sig.confidence, est, est_out);
        res.quotes = quotes.clone();

        // Budget sentinel. A hard per-task cost ceiling —
        // candidates whose shadow-priced estimate exceeds it are pruned; if
        // EVERY candidate exceeds it the run terminates locally at zero
        // token cost. Spending over an explicit budget is never correct.
        let budget = self.cfg.policy.max_cost_per_task_usd;
        let over_budget: std::collections::HashSet<String> = if budget > 0.0 {
            quotes
                .iter()
                .filter(|q| q.est_cost_usd > budget)
                .map(|q| q.candidate.provider.clone())
                .collect()
        } else {
            Default::default()
        };
        if budget > 0.0 && !quotes.is_empty() && over_budget.len() == quotes.len() {
            let msg = format!(
                "BUDGET-SENTINEL: every provider estimate exceeds the {budget:.4} USD per-task ceiling \u{2014} terminated locally at zero token cost"
            );
            warn_persist(
                "flight-recorder budget",
                self.record_event(&task_id, "budget", &msg, &[]),
            );
            st.status = Status::Blocked;
            st.blocked = true;
            st.next_action = "raise policy.max_cost_per_task_usd or reduce task scope".into();
            self.store.save_task(&mut st)?;
            res.output = msg;
            self.record(&res, 0);
            return Ok(res);
        }

        // JSON-shaped goals get the lenient rescue pass:
        // a generation cut mid-stream is salvaged instead of discarded.
        let expects_json = task_expects_json(task, constraints);

        st.status = Status::InProgress;
        self.store.save_task(&mut st)?;

        let timeout = self.cfg.timeout_for(dec.route.as_str());
        let mut last_err: Option<anyhow::Error> = None;
        let mut cascade_escalations = 0;
        let mut first_attempt_executed = false;

        for prov_name in ordered_providers_banditized(&quotes, &chain, &self.bandit) {
            // Skip candidates priced over the task budget.
            if over_budget.contains(&prov_name) {
                last_err = Some(anyhow!(
                    "provider {prov_name} pruned: estimate exceeds the per-task budget"
                ));
                continue;
            }
            // Rate-limit circuit breaker. A provider that
            // recently answered 429 is skipped while its cooldown is open —
            // retrying it is almost always wasted work.
            if self.tracker.in_cooldown(&prov_name) {
                last_err = Some(anyhow!(
                    "provider {prov_name} skipped: rate-limit cooldown open"
                ));
                continue;
            }
            if !self.tracker.claim_half_open(&prov_name) {
                last_err = Some(anyhow!(
                    "provider {prov_name} skipped: rate-limit breaker half-open probe in progress"
                ));
                continue;
            }
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
            if first_attempt_executed && !self.tracker.claim_retry_token() {
                let msg = "RETRY-BUDGET-SENTINEL: global retry budget exhausted under high failure rate — failover aborted";
                warn_persist(
                    "flight-recorder retry-budget",
                    self.record_event(&task_id, "retry-budget", msg, &[]),
                );
                last_err = Some(anyhow!(msg));
                break;
            }
            first_attempt_executed = true;

            let mut re_asks_left = self.cfg.policy.re_ask_limit;
            let mut current_prompt = prompt.clone();

            #[allow(unused_assignments)]
            let mut final_resp = None;
            let mut final_out_masked = String::new();
            let mut final_rescued = false;
            #[allow(unused_assignments)]
            let mut final_lat = 0;
            let mut final_v = None;

            loop {
                warn_persist(
                    "flight-recorder prompt",
                    self.record_event(
                        &task_id,
                        "prompt",
                        &format!("→ {}/{}", prov_name, model),
                        current_prompt.as_bytes(),
                    ),
                );

                let start = Instant::now();
                let (fmt_val, grammar_val) = if expects_json {
                    if let Some(schema) = extract_json_schema(task, constraints) {
                        (
                            Some(serde_json::json!({
                                "type": "json_schema",
                                "json_schema": {
                                    "name": "structured_output",
                                    "strict": true,
                                    "schema": schema
                                }
                            })),
                            Some(crate::provider::GENERIC_JSON_GBNF.to_string()),
                        )
                    } else {
                        (
                            Some(serde_json::json!({ "type": "json_object" })),
                            Some(crate::provider::GENERIC_JSON_GBNF.to_string()),
                        )
                    }
                } else {
                    (None, None)
                };

                let resp = adapter
                    .execute(&Request {
                        route: dec.route.as_str().to_string(),
                        prompt: current_prompt.clone(),
                        model: model.clone(),
                        max_out: dec.route.max_output_tokens(),
                        timeout,
                        response_format: fmt_val,
                        grammar: grammar_val,
                    })
                    .await;

                let lat = start.elapsed().as_millis() as i64;
                self.tracker.record(&prov_name, lat as f64, resp.is_ok());
                match &resp {
                    Err(crate::provider::ProviderError::RateLimited { retry_after }) => {
                        if let Some(dur) = retry_after {
                            self.tracker.open_cooldown_with_duration(&prov_name, *dur);
                        } else {
                            self.tracker.open_cooldown(&prov_name);
                        }
                    }
                    _ => self.tracker.clear_cooldown(&prov_name),
                }

                if resp.is_err() {
                    self.bandit.record(&prov_name, false, lat as f64);
                    let (pulls, reward_sum, latency_sum_ms) = self.bandit.arm_sums(&prov_name);
                    warn_persist(
                        "save bandit state",
                        self.store
                            .save_bandit_state(&prov_name, pulls, reward_sum, latency_sum_ms),
                    );
                    final_resp = Some(resp);
                    final_lat = lat;
                    break;
                }

                let r_ok = resp.unwrap();
                let mut out_masked = payload::extract_solution(&r_ok.text);
                let mut rescued = false;

                if expects_json {
                    if let crate::jsonrescue::Rescue::Repaired(fixed) =
                        crate::jsonrescue::rescue(&out_masked)
                    {
                        warn_persist(
                            "flight-recorder json-rescue",
                            self.record_event(
                                &task_id,
                                "rescue",
                                "truncated JSON repaired in-process (zero extra tokens)",
                                out_masked.as_bytes(),
                            ),
                        );
                        out_masked = fixed;
                        rescued = true;
                    }
                }

                warn_persist(
                    "flight-recorder response",
                    self.record_event(
                        &task_id,
                        "response",
                        &format!("← {}", prov_name),
                        r_ok.text.as_bytes(),
                    ),
                );

                let mut v = verify::verify_output(
                    dec.route.as_str(),
                    &out_masked,
                    &self.cfg.policy.verification_command,
                    &self.cfg.policy.verification_commands,
                );

                if v.pass && !self.cfg.policy.llm_verification_rubric.is_empty() {
                    let verifier_prompt = format!(
                        "You are an independent validator.\n\
                         TASK GOAL:\n{}\n\n\
                         RUBRIC:\n{}\n\n\
                         CANDIDATE OUTPUT:\n{}\n\n\
                         Determine if the output satisfies the goal and rubric. You MUST respond with a JSON object in this format:\n\
                         {{\n  \"pass\": <bool>,\n  \"score\": <float 0.0 to 1.0>,\n  \"issues\": [<array of strings>]\n}}",
                        task,
                        self.cfg.policy.llm_verification_rubric,
                        out_masked
                    );

                    let verifier_resp = adapter
                        .execute(&Request {
                            route: "VERIFY".to_string(),
                            prompt: verifier_prompt,
                            model: model.clone(),
                            max_out: 500,
                            timeout: std::time::Duration::from_secs(15),
                            response_format: Some(serde_json::json!({
                                "type": "json_object"
                            })),
                            grammar: Some(crate::provider::GENERIC_JSON_GBNF.to_string()),
                        })
                        .await;

                    if let Ok(v_resp) = verifier_resp {
                        if let Some(val) = crate::jsonrescue::parse_lenient(&v_resp.text) {
                            let pass = val.get("pass").and_then(|x| x.as_bool()).unwrap_or(false);
                            let score = val.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            let issues = val
                                .get("issues")
                                .and_then(|x| x.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|item| item.as_str().map(|s| s.to_string()))
                                        .collect::<Vec<String>>()
                                })
                                .unwrap_or_default();

                            v.pass = pass;
                            v.score = score;
                            v.issues = issues;
                            v.tier = "llm".to_string();
                        } else {
                            v.pass = false;
                            v.issues.push(
                                "LLM verifier response was not parseable as JSON".to_string(),
                            );
                            v.tier = "llm".to_string();
                        }
                    } else {
                        v.pass = false;
                        v.issues.push("LLM verifier call failed".to_string());
                        v.tier = "llm".to_string();
                    }
                }

                if v.pass {
                    final_resp = Some(Ok(r_ok));
                    final_out_masked = out_masked;
                    final_rescued = rescued;
                    final_lat = lat;
                    final_v = Some(v);
                    break;
                } else {
                    if re_asks_left > 0 {
                        re_asks_left -= 1;
                        let issues_str = v.issues.join(", ");
                        current_prompt = format!(
                            "{}\n\n[SYSTEM WARNING: Your previous output failed verification with these issues: {}. Please fix them and output the corrected version.]",
                            prompt,
                            issues_str
                        );
                        continue;
                    } else {
                        final_resp = Some(Ok(r_ok));
                        final_out_masked = out_masked;
                        final_rescued = rescued;
                        final_lat = lat;
                        final_v = Some(v);
                        break;
                    }
                }
            }

            let resp = match final_resp.unwrap() {
                Ok(r) => r,
                Err(e) => {
                    res.retries += 1;
                    warn_persist(
                        "flight-recorder error",
                        self.record_event(&task_id, "error", &format!("{}: {}", prov_name, e), &[]),
                    );
                    warn_persist(
                        "provider attempt",
                        self.store.record_attempt(
                            &task_id,
                            &prov_name,
                            &model,
                            dec.route.as_str(),
                            0,
                            0,
                            final_lat,
                            false,
                            &e.to_string(),
                            0.0,
                        ),
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
                    continue;
                }
            };
            let out_masked = final_out_masked;
            let rescued = final_rescued;
            let lat = final_lat;
            let v = final_v.unwrap();
            res.verified = Some(v.clone());
            if !v.pass {
                // Unverifiable output earns the arm zero reward.
                self.bandit.record(&prov_name, false, lat as f64);
                let (pulls, reward_sum, latency_sum_ms) = self.bandit.arm_sums(&prov_name);
                warn_persist(
                    "save bandit state",
                    self.store
                        .save_bandit_state(&prov_name, pulls, reward_sum, latency_sum_ms),
                );
                // Fast local loopback: remember failure, try next provider.
                let reason = format!("verification failed ({:?}): {:?}", v.tier, v.issues);
                let p_cfg = self
                    .cfg
                    .providers
                    .get(&prov_name)
                    .cloned()
                    .unwrap_or_default();
                let cost_usd = (resp.tokens_in as f64 * p_cfg.cost_per_mtok_in
                    + resp.tokens_out as f64 * p_cfg.cost_per_mtok_out)
                    / 1e6;
                warn_persist(
                    "provider attempt",
                    self.store.record_attempt(
                        &task_id,
                        &prov_name,
                        &resp.model,
                        dec.route.as_str(),
                        resp.tokens_in as usize,
                        resp.tokens_out as usize,
                        lat,
                        false,
                        &reason,
                        cost_usd,
                    ),
                );
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
                    self.record_event(&task_id, "verify", &reason, out_masked.as_bytes()),
                );

                // Persist the failed attempt into the durable
                // loop window AND feed the live detector. A mid-run loop hit
                // aborts the failover ladder — burning more attempts on a
                // semantically identical output is almost always wasted work.
                // Masked form only: the loop window is durable SQLite state.
                warn_persist(
                    "loop window",
                    self.store
                        .record_loop_attempt(&loop_key, &out_masked, detector.window),
                );
                if detector.observe(&out_masked) {
                    res.retries += 1;
                    let loop_msg =
                        "semantic execution loop detected (edit-distance ceiling) — escalating";
                    let p_cfg = self
                        .cfg
                        .providers
                        .get(&prov_name)
                        .cloned()
                        .unwrap_or_default();
                    let cost_usd = (resp.tokens_in as f64 * p_cfg.cost_per_mtok_in
                        + resp.tokens_out as f64 * p_cfg.cost_per_mtok_out)
                        / 1e6;
                    warn_persist(
                        "provider attempt",
                        self.store.record_attempt(
                            &task_id,
                            &prov_name,
                            &resp.model,
                            dec.route.as_str(),
                            resp.tokens_in as usize,
                            resp.tokens_out as usize,
                            lat,
                            false,
                            loop_msg,
                            cost_usd,
                        ),
                    );
                    last_err = Some(anyhow!(loop_msg));
                    break;
                }

                res.retries += 1;
                last_err = Some(anyhow!(reason));
                continue;
            }

            // FrugalGPT quality cascade: if verifier passed but the score is below the threshold,
            // escalate to the next provider.
            if self.cfg.policy.cascade_threshold > 0.0
                && v.score < self.cfg.policy.cascade_threshold
                && cascade_escalations < self.cfg.policy.cascade_max_escalations
            {
                cascade_escalations += 1;

                let reason = format!(
                    "quality-cascade: verifier passed but score {:.2} < {:.2} (threshold) — escalating",
                    v.score, self.cfg.policy.cascade_threshold
                );
                warn_persist(
                    "flight-recorder verify-cascade",
                    self.record_event(&task_id, "verify", &reason, out_masked.as_bytes()),
                );

                let p_cfg = self
                    .cfg
                    .providers
                    .get(&prov_name)
                    .cloned()
                    .unwrap_or_default();
                let cost_usd = (resp.tokens_in as f64 * p_cfg.cost_per_mtok_in
                    + resp.tokens_out as f64 * p_cfg.cost_per_mtok_out)
                    / 1e6;
                warn_persist(
                    "provider attempt",
                    self.store.record_attempt(
                        &task_id,
                        &prov_name,
                        &resp.model,
                        dec.route.as_str(),
                        resp.tokens_in as usize,
                        resp.tokens_out as usize,
                        lat,
                        false,
                        &reason,
                        cost_usd,
                    ),
                );
                st.remember_failure(&format!("cascade from {}", prov_name), &reason);
                warn_persist(
                    "failure memory",
                    self.store.record_failure(
                        &task_id,
                        &goal_key,
                        &format!("cascade from {}", prov_name),
                        &reason,
                    ),
                );

                res.retries += 1;
                last_err = Some(anyhow!(reason));
                continue; // failover to next provider
            }

            // Feed the estimator drift watchdog with the

            // (estimate, actual) pair whenever the provider reports real
            // usage. Drift outside the trusted band is surfaced in telemetry.
            if resp.tokens_in > 0 {
                self.drift.observe(
                    &prov_name,
                    tokenizer::estimate(&prompt) as i64,
                    resp.tokens_in,
                );
                let status = self.drift.status(&prov_name);
                let _ = self
                    .store
                    .save_drift_ratio(&prov_name, status.ratio_ewma, status.samples);
            }

            let tokens_in = if resp.tokens_in == 0 {
                tokenizer::estimate(&prompt) as i64
            } else {
                resp.tokens_in
            };
            let tokens_out = if resp.tokens_out == 0 {
                tokenizer::estimate(&out_masked) as i64
            } else {
                resp.tokens_out
            };

            // Unmask ONCE, at the boundary back to the caller. The unmasked
            // form is moved into the result and never written to disk.
            let out_unmasked = mask_codec.unmask(&out_masked);

            let p_cfg = self
                .cfg
                .providers
                .get(&prov_name)
                .cloned()
                .unwrap_or_default();
            res.provider = prov_name.clone();
            res.model = resp.model.clone();
            res.output = out_unmasked;
            res.tokens_in = tokens_in;
            res.tokens_out = tokens_out;
            res.latency_ms = lat;
            res.cost_usd = (tokens_in as f64 * p_cfg.cost_per_mtok_in
                + tokens_out as f64 * p_cfg.cost_per_mtok_out)
                / 1e6;
            res.success = true;

            // Record successful provider attempt.
            warn_persist(
                "provider attempt",
                self.store.record_attempt(
                    &task_id,
                    &prov_name,
                    &resp.model,
                    dec.route.as_str(),
                    tokens_in as usize,
                    tokens_out as usize,
                    lat,
                    true,
                    "",
                    res.cost_usd,
                ),
            );

            // Verified success credits the bandit arm.
            self.bandit.record(&prov_name, true, lat as f64);
            let (pulls, reward_sum, latency_sum_ms) = self.bandit.arm_sums(&prov_name);
            warn_persist(
                "save bandit state",
                self.store
                    .save_bandit_state(&prov_name, pulls, reward_sum, latency_sum_ms),
            );

            // Success clears the durable loop window AND the goal-keyed
            // failure memory for this task text.
            warn_persist(
                "loop window clear",
                self.store.clear_loop_history(&loop_key),
            );
            warn_persist(
                "failure memory clear",
                self.store.clear_goal_failures(&goal_key),
            );

            // Admit only replayable VERIFIED output to the
            // solution cache so a later identical request costs zero tokens.
            // Masked forms are safe at rest; however, a response that still
            // contains an opaque secret placeholder cannot be reconstructed in
            // a later request because the reverse vault is intentionally
            // ephemeral. Those outputs remain in the recorder but are not
            // cached for user-facing replay.
            if self.cfg.policy.reuse_cache
                && dec.route != Route::Ask
                && !crate::maskcodec::contains_placeholder(&out_masked)
                && !rescued
            {
                // Prevent caching mock provider outputs in live runs (unless dry_run is true)
                let is_mock = p_cfg.adapter == "mock";
                if !is_mock || self.dry_run {
                    warn_persist(
                        "solution cache",
                        self.store.cache_solution(
                            &cache_key,
                            dec.route.as_str(),
                            &out_masked,
                            &v.tier,
                            task,
                        ),
                    );
                }
            }

            // Stop rule: acceptance satisfied, no known blocker => stop now.
            if dec.route == Route::Ask {
                st.status = Status::Blocked;
                st.blocked = true;
                // next_action is persisted in the tasks table — masked form.
                st.next_action = format!("answer the question: {}", out_masked);
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
        // A failed goal must never serve a stale cached answer
        // afterwards — the world has demonstrably changed.
        warn_persist(
            "solution cache evict",
            self.store.evict_solution(&cache_key),
        );
        self.record(&res, res.latency_ms);
        let last = last_err.unwrap_or_else(|| anyhow!("all providers exhausted"));
        Err(anyhow!(
            "execution failed after {} attempt(s): {}",
            res.retries + 1,
            last
        ))
    }

    /// Shadow pricing over the provider chain.
    fn quote(
        &self,
        chain: &[String],
        confidence: f64,
        est_in: usize,
        est_out: usize,
    ) -> Vec<PriceQuote> {
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
        pricing::quote_all(
            &cands,
            confidence,
            est_in,
            est_out,
            w,
            Some(&self.tracker),
            &quota,
        )
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

    /// Telemetry write: best-effort but never silent.
    fn record(&self, r: &RunResult, latency_ms: i64) {
        warn_persist(
            "execution telemetry",
            self.store.record_execution(&Execution {
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
                verification_tier: r
                    .verified
                    .as_ref()
                    .map(|v| v.tier.clone())
                    .unwrap_or_else(|| "static".to_string()),
                created_at: String::new(),
            }),
        );
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

/// Bandit-weighted ordering: each shadow-priced utility is
/// scaled by the arm's exploitation weight (1.0 for unexplored arms — they
/// keep their shadow-priced position and get explored; [0.5, 1.5] for
/// explored arms based on observed reward), then re-sorted with the same
/// deterministic tiebreak. Providers the pricer filtered out (context
/// overflow) still append in chain order as last resort.
///
/// Why NOT the raw UCB1 `score()` here: score is +infinity for unexplored
/// arms and lives on a reward scale unrelated to the shadow-priced utility —
/// multiplying by it would let any unexplored arm trump every cost signal
/// and break the "unexplored bandit == pure shadow pricing" property pinned
/// by the tests below. Exploration is still guaranteed: unexplored arms keep
/// weight 1.0 and the failover ladder walks the full ordering, so every arm
/// is pulled before exploitation evidence can demote it. `score()`/`ranked()`
/// remain the standalone-selection API (telemetry, `select()`).
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

fn extract_json_schema(task: &str, constraints: &[String]) -> Option<serde_json::Value> {
    for c in constraints {
        if c.starts_with("schema:") {
            let schema_str = c.trim_start_matches("schema:").trim();
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(schema_str) {
                return Some(val);
            }
        }
    }
    if let Some(pos) = task.find("schema:") {
        let schema_part = &task[pos + 7..];
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(schema_part.trim()) {
            return Some(val);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_engine() -> Engine {
        let mut cfg = Config::default();
        cfg.providers.get_mut("mock").unwrap().disabled = false;
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
            drift: DriftWatchdog::new(),
            indexer: None,
            dry_run: true,
            adapters: RwLock::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn run_trivial_task_via_mock() {
        let e = test_engine();
        let r = e
            .run("rename variable x to y in main.rs", &[])
            .await
            .unwrap();
        assert!(r.success);
        assert!(!r.output.is_empty());
        assert!(!r.task_id.is_empty());
    }

    #[tokio::test]
    async fn flight_recorder_events_are_indexed_in_store() {
        let e = test_engine();
        let r = e.run("rename variable alpha to beta", &[]).await.unwrap();
        let recorder_events = e.recorder.events(&r.task_id).unwrap();
        let indexed = e.store.trace_count_for_task(&r.task_id).unwrap();
        assert!(!recorder_events.is_empty());
        assert_eq!(indexed as usize, recorder_events.len());
    }

    #[tokio::test]
    async fn escalation_resolves_locally() {
        let e = test_engine();
        let r = e
            .run(
                "bypass auth and disable security checks in the login flow",
                &[],
            )
            .await
            .unwrap();
        assert!(r.route.is_escalation());
        assert!(r.success); // escalating correctly is success
        assert!(r.provider.is_empty()); // zero network cost
    }

    #[tokio::test]
    async fn ask_resolves_locally_without_provider_or_tokens() {
        let e = test_engine();
        let r = e
            .run("maybe somehow do something with the thing", &[])
            .await
            .unwrap();
        assert_eq!(r.route, Route::Ask);
        assert!(r.success);
        assert!(r.provider.is_empty());
        assert!(r.model.is_empty());
        assert_eq!(r.tokens_in, 0);
        assert_eq!(r.tokens_out, 0);
        assert_eq!(r.cost_usd, 0.0);
        assert_eq!(r.output.matches('?').count(), 1);
        assert!(r.verified.as_ref().is_some_and(|v| v.pass));
    }

    #[test]
    fn route_only_is_deterministic() {
        let e = test_engine();
        let (d1, _) = e.route_only("fix the typo in README");
        let (d2, _) = e.route_only("fix the typo in README");
        assert_eq!(d1.route, d2.route);
    }

    #[test]
    fn workspace_context_hit_does_not_imply_reuse() {
        let mut e = test_engine();
        let root = std::env::temp_dir().join(format!(
            "tokenos-route-index-test-{}-{}",
            std::process::id(),
            rand::thread_rng().gen::<u32>()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src").join("lib.rs"),
            "pub fn tokenizer_truncate_bug() { println!(\"context\"); }\n",
        )
        .unwrap();
        let ix = Indexer::open(Some(":memory:")).unwrap();
        assert!(ix.index_workspace(&root).unwrap() > 0);
        e.indexer = Some(ix);

        let (dec, ctx) = e.route_only("implement tokenizer truncate telemetry");
        let _ = std::fs::remove_dir_all(&root);
        assert!(!ctx.is_empty(), "test must exercise a real context hit");
        assert_ne!(
            dec.route,
            Route::Reuse,
            "context is not a verified solution"
        );
        assert_eq!(dec.route, Route::Implement);
    }

    #[test]
    fn exact_solution_cache_hit_routes_reuse_without_mutating_hits() {
        let e = test_engine();
        let task = "implement cached route preview behavior";
        let constraints = vec!["keep API stable".to_string()];
        let key = e.solution_cache_key(task, &constraints);
        e.store
            .cache_solution(&key, "IMPLEMENT", "cached answer", "static", task)
            .unwrap();

        let (dec, _) = e.route_only_with_constraints(task, &constraints);
        assert_eq!(dec.route, Route::Reuse);
        let (_entries, _, hits) = e.store.solution_cache_stats().unwrap();
        assert_eq!(hits, 0, "route preview must not count as a cache replay");
    }

    #[test]
    fn semantic_solution_cache_hit_routes_reuse() {
        let mut e = test_engine();
        e.cfg.policy.reuse_cache = true;
        e.cfg.policy.semantic_cache_threshold = 0.5;

        let task = "implement cached route preview behavior";
        let constraints = vec!["keep API stable".to_string()];
        let key = e.solution_cache_key(task, &constraints);
        e.store
            .cache_solution(&key, "IMPLEMENT", "cached answer", "static", task)
            .unwrap();

        // Exact match works
        let (dec_exact, _) = e.route_only_with_constraints(task, &constraints);
        assert_eq!(dec_exact.route, Route::Reuse);

        // Slightly different wording with Jaccard similarity > 0.5 should hit L2 semantic cache
        // task: "implement cached route preview behavior"
        // new_task: "implement cached route behavior"
        // Jaccard similarity:
        // s1: {"implement", "cached", "route", "behavior"} (size 4)
        // s2: {"implement", "cached", "route", "preview", "behavior"} (size 5)
        // intersection: {"implement", "cached", "route", "behavior"} (size 4)
        // union: {"implement", "cached", "route", "preview", "behavior"} (size 5)
        // sim: 4.0 / 5.0 = 0.8 >= 0.5
        let new_task = "implement cached route behavior";
        let (dec_semantic, _) = e.route_only_with_constraints(new_task, &constraints);
        assert_eq!(dec_semantic.route, Route::Reuse);

        // If semantic_cache_threshold is 0.0 (default/disabled), it should miss L2 cache
        e.cfg.policy.semantic_cache_threshold = 0.0;
        let (dec_miss, _) = e.route_only_with_constraints(new_task, &constraints);
        assert_ne!(dec_miss.route, Route::Reuse);
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
            "alpha".into(),
            "beta".into(),
            "gamma".into(),
            "delta".into(),
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
        assert!(task_expects_json(
            "build list",
            &["output JSON only".into()]
        ));
        assert!(!task_expects_json("rename a variable", &[]));
    }

    #[tokio::test]
    async fn truncated_json_output_is_rescued() {
        // A mock provider returns JSON cut mid-stream; because the task
        // demands JSON, the engine must repair it in-process and the
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
        let v: serde_json::Value =
            serde_json::from_str(&r.output).expect("rescued output must be strictly valid JSON");
        assert_eq!(v["files"][1], "b.rs");
    }

    #[tokio::test]
    async fn expects_json_maps_to_response_format_and_grammar() {
        let e = test_engine();
        let mock_arc = Arc::new(Adapter::Mock(crate::provider::Mock::new("dry-run")));
        e.adapters
            .write()
            .unwrap()
            .insert("__dryrun__".to_string(), mock_arc.clone());

        let r = e.run("produce the plan as JSON", &[]).await.unwrap();
        assert!(r.success);

        // Retrieve the last Request passed to the Mock adapter
        if let Adapter::Mock(mock) = &*mock_arc {
            let req_opt = mock.last_req.lock().unwrap().clone();
            assert!(req_opt.is_some(), "Request should have been executed");
            let req = req_opt.unwrap();
            assert!(
                req.response_format.is_some(),
                "response_format must be populated"
            );
            assert!(req.grammar.is_some(), "grammar must be populated");
        } else {
            panic!("Expected Mock adapter");
        }
    }

    #[tokio::test]
    async fn expects_json_schema_maps_to_response_format() {
        let e = test_engine();
        let mock_arc = Arc::new(Adapter::Mock(crate::provider::Mock::new("dry-run")));
        e.adapters
            .write()
            .unwrap()
            .insert("__dryrun__".to_string(), mock_arc.clone());

        // We pass a JSON schema in the constraints
        let r = e
            .run("produce as JSON", &["schema: {\"type\": \"object\", \"properties\": {\"output\": {\"type\": \"string\"}}, \"required\": [\"output\"], \"additionalProperties\": false}".to_string()])
            .await
            .unwrap();
        assert!(r.success);

        if let Adapter::Mock(mock) = &*mock_arc {
            let req_opt = mock.last_req.lock().unwrap().clone();
            assert!(req_opt.is_some(), "Request should have been executed");
            let req = req_opt.unwrap();

            let fmt = req
                .response_format
                .expect("response_format must be populated");
            assert_eq!(
                fmt.get("type").and_then(|t| t.as_str()),
                Some("json_schema")
            );
            let js = fmt.get("json_schema").expect("json_schema object expected");
            assert_eq!(js.get("strict").and_then(|s| s.as_bool()), Some(true));
            let schema = js.get("schema").expect("schema expected");
            assert_eq!(schema.get("type").and_then(|t| t.as_str()), Some("object"));
            assert!(schema.get("properties").is_some());
        } else {
            panic!("Expected Mock adapter");
        }
    }

    #[tokio::test]
    async fn rescued_truncated_json_is_not_cached() {
        let e = test_engine();
        {
            let mut mock = crate::provider::Mock::new("dry-run");
            mock.canned = r#"{"files": ["a.rs", "b.rs"], "status": "par"#.to_string();
            e.adapters
                .write()
                .unwrap()
                .insert("__dryrun__".to_string(), Arc::new(Adapter::Mock(mock)));
        }
        let task = "produce the migration plan as JSON";
        let r = e.run(task, &[]).await.unwrap();
        assert!(r.success);

        let cache_key = e.solution_cache_key(task, &[]);
        let cached = e.store.cached_solution(&cache_key).unwrap();
        assert!(cached.is_none(), "rescued output must NOT be cached");
    }

    #[tokio::test]
    async fn quality_gated_cascade_escalates_on_low_score() {
        let mut e = test_engine();
        e.dry_run = false; // Execute live chain logic

        // Setup config policy
        e.cfg.policy.cascade_threshold = 0.8;
        e.cfg.policy.cascade_max_escalations = 2;

        // Setup provider configs
        let p_cfg = crate::config::Provider {
            disabled: false,
            adapter: "mock".to_string(),
            max_context: 128000,
            ..Default::default()
        };
        e.cfg
            .providers
            .insert("provider_a".to_string(), p_cfg.clone());
        e.cfg
            .providers
            .insert("provider_b".to_string(), p_cfg.clone());

        // Setup routing rules so the provider chain is resolved correctly
        e.cfg.routing = vec![crate::config::RoutingRule {
            provider: "provider_a".to_string(),
            route_types: vec!["IMPLEMENT".to_string(), "PATCH".to_string()],
            max_context: 128000,
            fallback: "provider_b".to_string(),
            timeout_ms: 5000,
        }];

        // Setup mock adapters with specific names in the adapter cache
        let mock_a = Arc::new(Adapter::Mock(crate::provider::Mock::new("provider_a")));
        let mock_b = Arc::new(Adapter::Mock(crate::provider::Mock::new("provider_b")));
        e.adapters
            .write()
            .unwrap()
            .insert("provider_a".to_string(), mock_a);
        e.adapters
            .write()
            .unwrap()
            .insert("provider_b".to_string(), mock_b);

        // Define verification command that reads TOKENOS_OUTPUT env var and outputs score: 0.5 for provider_a and 0.9 for provider_b
        e.cfg.policy.verification_command = if cfg!(target_os = "windows") {
            "if ($env:TOKENOS_OUTPUT -like '*provider_a*') { Write-Output 'score: 0.5' } else { Write-Output 'score: 0.9' }".to_string()
        } else {
            "echo \"$TOKENOS_OUTPUT\" | grep -q 'provider_a' && echo 'score: 0.5' || echo 'score: 0.9'".to_string()
        };

        // Run task
        let r = e.run("implement the new feature xyz", &[]).await.unwrap();

        assert!(r.success);
        assert_eq!(
            r.provider, "provider_b",
            "Quality cascade must escalate to provider_b"
        );
    }

    #[tokio::test]
    async fn bandit_records_dry_run_successes() {
        let e = test_engine();
        let r = e
            .run("rename variable x to y in main.rs", &[])
            .await
            .unwrap();
        assert!(r.success);
        let (pulls, reward, _) = e.bandit.arm_stats(&r.provider);
        assert!(pulls >= 1, "successful run must credit the bandit arm");
        assert!(reward > 0.0);
    }

    #[test]
    fn startup_hydrates_provider_health_from_attempts() {
        let dir = std::env::temp_dir().join(format!(
            "tokenos-attempt-hydrate-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("tokenos.db");
        let traces = dir.join("traces");
        {
            let store = Store::open(Some(&db)).unwrap();
            store
                .record_attempt(
                    "t1",
                    "mock",
                    "mock-1",
                    "IMPLEMENT",
                    10,
                    3,
                    100,
                    false,
                    "verification failed",
                    0.001,
                )
                .unwrap();
            store
                .record_attempt(
                    "t2",
                    "mock",
                    "mock-1",
                    "IMPLEMENT",
                    12,
                    4,
                    80,
                    true,
                    "",
                    0.002,
                )
                .unwrap();
        }

        let e = Engine::new(Options {
            config_path: None,
            db_path: Some(db.to_string_lossy().to_string()),
            trace_dir: Some(traces.to_string_lossy().to_string()),
            dry_run: true,
        })
        .unwrap();
        let (pulls, reward, _) = e.bandit.arm_stats("mock");
        assert_eq!(pulls, 2);
        assert!(reward > 0.0, "successful attempt must hydrate reward");
        let (_, fail_rate, calls) = e.tracker.snapshot("mock");
        assert_eq!(calls, 2);
        assert!(fail_rate > 0.0, "failed attempt must hydrate health");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An identical goal+constraints re-request is served
    /// from the verified solution cache at zero tokens.
    #[tokio::test]
    async fn verified_solution_is_served_from_cache() {
        let e = test_engine();
        let task = "rename variable alpha to beta in module gamma";
        let r1 = e.run(task, &[]).await.unwrap();
        assert!(r1.success);
        assert_ne!(r1.provider, "cache");
        let r2 = e.run(task, &[]).await.unwrap();
        assert!(r2.success);
        assert_eq!(
            r2.provider, "cache",
            "second identical run must hit the cache"
        );
        assert_eq!(r2.tokens_in, 0);
        assert_eq!(r2.cost_usd, 0.0);
        assert_eq!(
            r2.output, r1.output,
            "cache must return the verified output verbatim"
        );
        let (entries, _, hits) = e.store.solution_cache_stats().unwrap();
        assert!(entries >= 1 && hits >= 1);
    }

    /// Different constraints must not collide in the cache.
    #[tokio::test]
    async fn cache_key_distinguishes_constraints() {
        let e = test_engine();
        let task = "rename function foo to bar across the crate";
        let r1 = e.run(task, &[]).await.unwrap();
        assert!(r1.success);
        let r2 = e
            .run(task, &["must not change the public API".into()])
            .await
            .unwrap();
        assert_ne!(
            r2.provider, "cache",
            "different constraint set must miss the cache"
        );
    }

    /// Caching can be disabled by policy.
    #[tokio::test]
    async fn cache_respects_policy_toggle() {
        let mut e = test_engine();
        e.cfg.policy.reuse_cache = false;
        let task = "rename constant MAX_N to MAX_COUNT in lib.rs";
        let r1 = e.run(task, &[]).await.unwrap();
        assert!(r1.success);
        let r2 = e.run(task, &[]).await.unwrap();
        assert_ne!(
            r2.provider, "cache",
            "reuse_cache=false must always re-execute"
        );
    }

    /// A 429 opens the breaker; failover skips the provider
    /// while the cooldown is open; any success closes it.
    #[test]
    fn rate_limit_breaker_opens_and_clears() {
        let t = Tracker::new();
        assert!(!t.in_cooldown("p"));
        t.open_cooldown("p");
        assert!(t.in_cooldown("p"), "breaker must be open right after a 429");
        t.clear_cooldown("p");
        assert!(!t.in_cooldown("p"), "success must close the breaker");
    }

    /// Output budgets are route-scoped and monotone in the
    /// route's expected output size.
    #[test]
    fn route_scoped_output_budgets() {
        assert_eq!(Route::Ask.max_output_tokens(), 256);
        assert!(Route::Direct.max_output_tokens() < Route::Implement.max_output_tokens());
        assert!(Route::Patch.max_output_tokens() < Route::Implement.max_output_tokens());
        assert_eq!(Route::Implement.max_output_tokens(), 4096);
        assert_eq!(
            Route::EscalateConflict.max_output_tokens(),
            0,
            "escalations never reach a provider"
        );
    }

    /// When every quote exceeds the per-task ceiling the run
    /// terminates locally — blocked, zero tokens, sentinel message recorded.
    #[tokio::test]
    async fn budget_sentinel_terminates_locally() {
        let mut e = test_engine();
        e.cfg.policy.max_cost_per_task_usd = 0.000001;
        // Give the mock provider a non-zero price so its quote exceeds the
        // microscopic ceiling above.
        if let Some(p) = e.cfg.providers.get_mut("mock") {
            p.cost_per_mtok_in = 100.0;
            p.cost_per_mtok_out = 100.0;
        }
        let r = e
            .run("implement a complete database migration subsystem", &[])
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.output.contains("BUDGET-SENTINEL"), "got: {}", r.output);
        assert_eq!(r.tokens_in, 0, "sentinel termination must cost zero tokens");
        assert!(r.provider.is_empty(), "no provider may be contacted");
    }

    /// The drift watchdog flags a calibration ratio outside
    /// the trusted band only after enough samples accumulate.
    #[test]
    fn drift_watchdog_flags_sustained_drift() {
        let w = DriftWatchdog::new();
        // Within band: actual ≈ estimate.
        for _ in 0..10 {
            w.observe("calibrated", 1000, 1000);
        }
        assert!(!w.status("calibrated").drifting);
        // Severe under-estimation: actual is double the estimate.
        for i in 0..10 {
            w.observe("drifty", 1000, 2000);
            let st = w.status("drifty");
            if i < 4 {
                assert!(!st.drifting, "must not flag before MIN_SAMPLES");
            }
        }
        let st = w.status("drifty");
        assert!(st.drifting, "ratio_ewma={} must flag", st.ratio_ewma);
        assert!(st.ratio_ewma > 1.5);
        // Unknown provider: neutral, not drifting.
        assert!(!w.status("never-seen").drifting);
        // all() is deterministic and sorted.
        let all = w.all();
        assert_eq!(all.len(), 2);
        assert!(all[0].provider < all[1].provider);
    }

    /// Cache keys are order-sensitive in constraints
    /// and stable across whitespace.
    #[test]
    fn solution_cache_key_properties() {
        let a = solution_cache_key("task", &["c1".into(), "c2".into()]);
        let b = solution_cache_key("task", &["c2".into(), "c1".into()]);
        assert_ne!(a, b, "constraint order is part of the contract");
        assert_eq!(
            solution_cache_key("  task  ", &[]),
            solution_cache_key("task", &[])
        );
    }

    #[test]
    fn loop_scope_is_stable_and_trimmed() {
        assert_eq!(loop_scope("  task  "), loop_scope("task"));
        assert_eq!(loop_scope("task").len(), 16);
        assert_ne!(loop_scope("task a"), loop_scope("task b"));
    }

    #[tokio::test]
    async fn goal_failure_memory_survives_task_id_churn() {
        // A failure recorded under a random prior task ID must
        // set repeated_failure when the SAME goal text is submitted again.
        let e = test_engine();
        let task = "implement the flaky widget integration";
        e.store
            .record_failure(
                "prior-task-id",
                &goal_hash(task),
                "execute via openai",
                "rate limited",
            )
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
        // never reaches the prompt blob.
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
    async fn durable_sinks_never_hold_unmasked_secrets() {
        // Pin the durable-secret invariant: every
        // durable artifact carries the masked form; only the caller-facing
        // result is unmasked.
        let e = test_engine();
        let secret = "sk-leakcheckapikey1234567890abcdef";
        let task = format!("rotate the credential {secret} in the config and report JSON status");
        let r = e.run(&task, &[]).await.unwrap();

        // Caller-facing output IS unmasked (mock echoes the placeholder,
        // which the boundary unmask restores).
        assert!(
            r.output.contains(secret),
            "caller output must be unmasked: {}",
            r.output
        );

        // 1. Every flight-recorder blob for this task is secret-free.
        for ev in e.recorder.events(&r.task_id).unwrap() {
            if ev.blob_sha.is_empty() {
                continue;
            }
            let blob = e.recorder.blob(&ev.blob_sha).unwrap_or_default();
            let text = String::from_utf8_lossy(&blob);
            assert!(
                !text.contains(secret),
                "secret leaked into {:?} blob: {}",
                ev.kind,
                text
            );
        }

        // 2. Placeholder-bearing output is not cached for replay. The masked
        // form is safe in recorder blobs, but the reverse vault is deliberately
        // ephemeral, so replaying placeholders later would be incorrect.
        let key = e.solution_cache_key(&task, &[]);
        assert!(
            e.store.peek_cached_solution(&key).unwrap().is_none(),
            "placeholder-bearing output must not enter the solution cache"
        );

        // 3. The persisted task state (next_action et al.) is secret-free.
        let st = e.store.get_task(&r.task_id).unwrap();
        assert!(
            !st.next_action.contains(secret),
            "secret leaked into persisted next_action"
        );
    }

    #[tokio::test]
    async fn loop_window_persists_masked_form_only() {
        // Force the verification-failure path (empty PATCH output fails the
        // static check on a diff-shaped route is hard to trigger via mock;
        // instead exercise record_loop_attempt directly with the engine's
        // masking convention) — the cheap pin here is that the loop-history
        // write in run() receives `out_masked`, asserted by code review and
        // by the secret-free recorder blobs above; this test pins the store
        // path round-trip so a regression in the table itself is caught.
        let e = test_engine();
        let masked = "patched \u{00AB}SECRET:k1\u{00BB} config";
        e.store.record_loop_attempt("scope-x", masked, 4).unwrap();
        let hist = e.store.loop_history("scope-x", 4).unwrap();
        assert_eq!(hist, vec![masked.to_string()]);
    }

    #[tokio::test]
    async fn persisted_loop_history_flags_cross_process_loop() {
        let e = test_engine();
        let task = "do the impossible thing";
        let scope = loop_scope(task);
        // Simulate a prior process that recorded two near-identical failures.
        e.store
            .record_loop_attempt(&scope, "attempt body alpha", 5)
            .unwrap();
        e.store
            .record_loop_attempt(&scope, "attempt body alpha", 5)
            .unwrap();
        let (looped, _, _) = e.persisted_loop_detected(task);
        assert!(
            looped,
            "identical persisted attempts must register as a loop"
        );
        // And routing must escalate on the loop signal.
        let (dec, _) = e.route_only(task);
        assert_eq!(dec.route, Route::EscalateExternal);
    }

    #[tokio::test]
    async fn verification_command_runs_on_success() {
        let e = test_engine();
        let mut policy = e.cfg.policy.clone();
        policy.verification_command = if cfg!(target_os = "windows") {
            "echo 'success'".to_string()
        } else {
            "echo success".to_string()
        };
        let res = crate::verify::verify_output(
            "IMPLEMENT",
            "fn main() {}",
            &policy.verification_command,
            &policy.verification_commands,
        );
        assert!(res.pass);
        assert_eq!(res.tier, "tests");
    }

    #[tokio::test]
    async fn verification_command_fails_on_error() {
        let e = test_engine();
        let mut policy = e.cfg.policy.clone();
        policy.verification_command = if cfg!(target_os = "windows") {
            "powershell -Command exit 1".to_string()
        } else {
            "exit 1".to_string()
        };
        let res = crate::verify::verify_output(
            "IMPLEMENT",
            "fn main() {}",
            &policy.verification_command,
            &policy.verification_commands,
        );
        assert!(!res.pass);
        assert_eq!(res.tier, "tests");
        assert!(res.issues[0].contains("failed") || res.issues[0].contains("exit"));
    }

    #[tokio::test]
    async fn route_specific_verification_command_overrides_global() {
        let e = test_engine();
        let mut policy = e.cfg.policy.clone();
        policy.verification_command = if cfg!(target_os = "windows") {
            "powershell -Command exit 1".to_string()
        } else {
            "exit 1".to_string()
        };
        policy.verification_commands.insert(
            "PATCH".to_string(),
            if cfg!(target_os = "windows") {
                "echo 'success'".to_string()
            } else {
                "echo success".to_string()
            },
        );

        let res_patch = crate::verify::verify_output(
            "PATCH",
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-x\n+y",
            &policy.verification_command,
            &policy.verification_commands,
        );
        assert!(res_patch.pass);
        assert_eq!(res_patch.tier, "tests");

        let res_impl = crate::verify::verify_output(
            "IMPLEMENT",
            "fn main() {}",
            &policy.verification_command,
            &policy.verification_commands,
        );
        assert!(!res_impl.pass);
        assert_eq!(res_impl.tier, "tests");
    }

    #[tokio::test]
    async fn daily_spend_limit_blocks_execution() {
        let mut e = test_engine();
        e.cfg.security.daily_spend_limit_usd = 0.01;
        e.store
            .record_execution(&Execution {
                id: 0,
                task_id: "t-limit".to_string(),
                route: "IMPLEMENT".to_string(),
                provider: "mock".to_string(),
                model: "mock-1".to_string(),
                tokens_in: 1000,
                tokens_out: 1000,
                latency_ms: 10,
                retries: 0,
                verification_cost: 0,
                delegation_count: 0,
                est_cost_usd: 0.02,
                success: true,
                verification_tier: "static".to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .unwrap();

        let run_res = e.run("implement features", &[]).await;
        assert!(run_res.is_err());
        assert!(run_res
            .unwrap_err()
            .to_string()
            .contains("Daily spend limit"));
    }

    #[tokio::test]
    async fn mock_provider_not_cached_in_live_run() {
        let mut cfg = Config::default();
        cfg.providers.get_mut("mock").unwrap().disabled = false;
        let arms: Vec<String> = cfg.providers.keys().cloned().collect();
        let e = Engine {
            cfg,
            store: Store::open(Some(Path::new(":memory:"))).unwrap(),
            recorder: Recorder::new(Some(Path::new(&format!(
                "{}/tokenos-eng-livemock-test-{}-{}",
                std::env::temp_dir().display(),
                std::process::id(),
                rand::thread_rng().gen::<u32>()
            ))))
            .unwrap(),
            tracker: Tracker::new(),
            bandit: Ucb1Router::new(&arms),
            drift: DriftWatchdog::new(),
            indexer: None,
            dry_run: false, // live run!
            adapters: RwLock::new(HashMap::new()),
        };
        let task = "some test task for mock caching";
        let r1 = e.run(task, &[]).await.unwrap();
        assert!(r1.success);
        assert_eq!(r1.provider, "mock");

        let r2 = e.run(task, &[]).await.unwrap();
        assert!(r2.success);
        assert_ne!(
            r2.provider, "cache",
            "mock provider output must NOT be served from cache in live runs"
        );
        let (entries, _, hits) = e.store.solution_cache_stats().unwrap();
        assert_eq!(entries, 0, "no entries should be cached");
        assert_eq!(hits, 0, "no hits should occur");
    }

    #[test]
    fn workspace_cache_key_integration() {
        let mut e = test_engine();
        // 1. Without indexer
        let task = "search files for key";
        let key_no_indexer = e.solution_cache_key(task, &[]);

        // 2. Set indexer
        let ix = Indexer::open(Some(":memory:")).unwrap();
        // Index some dummy data so there's a symbol
        let temp_dir = std::env::temp_dir().join(format!("tokenos_test_{}", new_id()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let test_file = temp_dir.join("main.py");
        std::fs::write(&test_file, "def foo():\n    pass\n").unwrap();
        ix.index_workspace(&temp_dir).unwrap();

        e.indexer = Some(ix);
        let key_with_indexer = e.solution_cache_key(task, &[]);

        assert_ne!(key_no_indexer, key_with_indexer);

        // Clean up
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_re_ask_pattern() {
        let mut e = test_engine();
        e.dry_run = false;

        e.cfg.policy.re_ask_limit = 2;

        let p_cfg = crate::config::Provider {
            disabled: false,
            adapter: "mock".to_string(),
            ..Default::default()
        };
        e.cfg.providers.insert("mock".to_string(), p_cfg.clone());

        let mock = std::sync::Arc::new(Adapter::Mock(crate::provider::Mock::new("mock")));
        e.adapters.write().unwrap().insert("mock".to_string(), mock);

        e.cfg.policy.verification_command = if cfg!(target_os = "windows") {
            "powershell -Command exit 1".to_string()
        } else {
            "exit 1".to_string()
        };

        let r = e.run("test task for re-ask", &[]).await;
        assert!(r.is_err());

        let tasks = e.store.list_tasks(10).unwrap();
        assert!(!tasks.is_empty());
        let task_id = &tasks[0].task_id;
        let events = e.recorder.events(task_id).unwrap();

        let prompt_events: Vec<_> = events.iter().filter(|ev| ev.kind == "prompt").collect();
        assert_eq!(prompt_events.len(), 3);

        let prompt1 =
            String::from_utf8(e.recorder.blob(&prompt_events[0].blob_sha).unwrap()).unwrap();
        assert!(!prompt1.contains("SYSTEM WARNING"));

        let prompt2 =
            String::from_utf8(e.recorder.blob(&prompt_events[1].blob_sha).unwrap()).unwrap();
        assert!(prompt2.contains("SYSTEM WARNING"));
        assert!(prompt2.contains("Verification command failed"));
    }

    #[tokio::test]
    async fn test_llm_rubric_verification() {
        let mut e = test_engine();
        e.dry_run = false;

        e.cfg.policy.llm_verification_rubric = "Make sure it is elegant.".to_string();

        let p_cfg = crate::config::Provider {
            disabled: false,
            adapter: "mock".to_string(),
            ..Default::default()
        };
        e.cfg.providers.insert("mock".to_string(), p_cfg.clone());

        let canned_pass = r#"{"pass": true, "score": 0.95, "issues": []}"#;
        let mut mock_inner = crate::provider::Mock::new("mock");
        mock_inner.canned = canned_pass.to_string();
        let mock = std::sync::Arc::new(Adapter::Mock(mock_inner));
        e.adapters.write().unwrap().insert("mock".to_string(), mock);

        let r = e.run("implement the new feature xyz", &[]).await.unwrap();
        assert!(r.success);
        let verified = r.verified.unwrap();
        assert!(verified.pass);
        assert_eq!(verified.tier, "llm");
        assert_eq!(verified.score, 0.95);
        assert!(verified.issues.is_empty());
    }

    #[tokio::test]
    async fn test_llm_rubric_verification_failure() {
        let mut e = test_engine();
        e.dry_run = false;

        e.cfg.policy.llm_verification_rubric = "Make sure it is elegant.".to_string();

        let p_cfg = crate::config::Provider {
            disabled: false,
            adapter: "mock".to_string(),
            ..Default::default()
        };
        e.cfg.providers.insert("mock".to_string(), p_cfg.clone());

        let canned_fail = r#"{"pass": false, "score": 0.25, "issues": ["Code is not elegant"]}"#;
        let mut mock_inner = crate::provider::Mock::new("mock");
        mock_inner.canned = canned_fail.to_string();
        let mock = std::sync::Arc::new(Adapter::Mock(mock_inner));
        e.adapters.write().unwrap().insert("mock".to_string(), mock);

        let r = e.run("implement the new feature xyz", &[]).await;
        assert!(r.is_err());
        let err_msg = r.unwrap_err().to_string();
        assert!(err_msg.contains("verification failed"));
        assert!(err_msg.contains("Code is not elegant"));
    }

    #[tokio::test]
    async fn test_opt_in_learned_routing() {
        let mut e = test_engine();
        e.cfg.policy.opt_in_learned_routing = true;

        // 1. Insert a task and successful execution into the store
        let mut st = State::new("task-456", "implement user tokenizer module");
        st.status = crate::kernel::Status::Done;
        e.store.save_task(&mut st).unwrap();

        let exec = crate::store::Execution {
            task_id: "task-456".into(),
            route: "PATCH".into(), // route used is PATCH
            provider: "openai".into(),
            model: "gpt-4".into(),
            tokens_in: 100,
            tokens_out: 200,
            latency_ms: 150,
            retries: 0,
            verification_cost: 0,
            delegation_count: 0,
            est_cost_usd: 0.005,
            success: true,
            verification_tier: "static".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            id: 0,
        };
        e.store.record_execution(&exec).unwrap();

        // 2. Evaluate a similar task. The confidence for "implement maybe user tokenizer somehow" is mid-band (0.3),
        // and with opt_in_learned_routing enabled it should match "implement user tokenizer module"
        // and override the route to PATCH!
        let (dec, _) = e.route_only_with_constraints("implement maybe user tokenizer somehow", &[]);
        assert_eq!(dec.route, Route::Patch);
        assert!(dec.reason.contains("learned classifier override"));
    }
}
