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
use crate::pricing::{self, Candidate, PriceQuote, Tracker, Weights};
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

impl Engine {
    /// Builds an Engine with all subsystems initialized.
    pub fn new(opt: Options) -> Result<Self> {
        let cfg = Config::load(opt.config_path.as_deref().map(Path::new))?;
        let store = Store::open(opt.db_path.as_deref().map(Path::new))?;
        let recorder = Recorder::new(opt.trace_dir.as_deref().map(Path::new))?;
        Ok(Engine {
            cfg,
            store,
            recorder,
            tracker: Tracker::new(),
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
    pub fn route_only(&self, task: &str) -> (Decision, String) {
        let ctx_block = self.minimum_viable_context(task);
        let est = tokenizer::estimate(task)
            + tokenizer::estimate(&ctx_block)
            + tokenizer::estimate(payload::KERNEL_CONTRACT);
        let index_hit = !ctx_block.is_empty();
        let loop_detected = self.persisted_loop_detected(task).0;
        let sig = kernel::extract_signals(task, est, index_hit, false, loop_detected);
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

        // Step 3: failure memory check (local SQLite).
        let repeated = self.store.has_similar_failure(&task_id, task).unwrap_or(false);

        // Step 3b (finding 12.2): durable semantic-loop window. History from
        // prior cold processes seeds the detector so oscillation across
        // invocations is caught deterministically.
        let (loop_detected, mut detector, loop_key) = self.persisted_loop_detected(task);

        // Step 4: deterministic routing (zero token cost).
        let est = tokenizer::estimate(task)
            + tokenizer::estimate(&st.context)
            + tokenizer::estimate(payload::KERNEL_CONTRACT);
        let sig = kernel::extract_signals(task, est, !st.context.is_empty(), repeated, loop_detected);
        let dec = kernel::decide(sig.clone(), &self.cfg.policy);

        let dec_blob = serde_json::to_vec(&dec).unwrap_or_default();
        let _ = self.recorder.record(
            &task_id,
            "decision",
            &format!("{}: {}", dec.route.as_str(), dec.reason),
            &dec_blob,
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

        st.status = Status::Routed;
        let _ = self.store.save_task(&mut st);

        // Escalations resolve locally with zero network cost.
        if dec.route.is_escalation() {
            st.status = Status::Escalated;
            st.blocked = true;
            st.next_action = dec.reason.clone();
            let _ = self.store.save_task(&mut st);
            res.output = format!("{}: {}", dec.route.as_str(), dec.reason);
            res.success = true; // escalating correctly IS the success condition
            self.record(&res, 0);
            return Ok(res);
        }

        // Step 5: payload serialization (static→dynamic, conclusions only).
        let prompt = payload::build(dec.route, &st);

        // Step 6: shadow pricing across the provider chain, then execute with
        // deterministic failover.
        let chain = self.cfg.provider_chain(dec.route.as_str());
        if chain.is_empty() {
            return Err(anyhow!("no enabled providers for route {}", dec.route.as_str()));
        }
        let quotes = self.quote(&chain, sig.confidence, est);
        res.quotes = quotes.clone();

        st.status = Status::InProgress;
        let _ = self.store.save_task(&mut st);

        let timeout = self.cfg.timeout_for(dec.route.as_str());
        let mut last_err: Option<anyhow::Error> = None;

        for prov_name in ordered_providers(&quotes, &chain) {
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

            let _ = self.recorder.record(
                &task_id,
                "prompt",
                &format!("→ {}/{}", prov_name, model),
                prompt.as_bytes(),
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

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    res.retries += 1;
                    let _ = self
                        .recorder
                        .record(&task_id, "error", &format!("{}: {}", prov_name, e), &[]);
                    let _ = self
                        .store
                        .record_failure(&task_id, &format!("execute via {}", prov_name), &e.to_string());
                    last_err = Some(e.into());
                    continue; // deterministic failover to next quote
                }
            };

            let out = payload::extract_solution(&resp.text);
            let _ = self
                .recorder
                .record(&task_id, "response", &format!("← {}", prov_name), resp.text.as_bytes());

            // Step 7: tiered verification — static first, zero token cost.
            let v = verify::static_check(dec.route.as_str(), &out);
            res.verified = Some(v.clone());
            if !v.pass {
                // Fast local loopback: remember failure, try next provider.
                let reason = format!("static verification failed: {:?}", v.issues);
                st.remember_failure(&format!("output from {}", prov_name), &reason);
                let _ = self
                    .store
                    .record_failure(&task_id, &format!("output from {}", prov_name), &reason);
                let _ = self.recorder.record(&task_id, "verify", &reason, out.as_bytes());

                // Finding 12.2: persist the failed attempt into the durable
                // loop window AND feed the live detector. A mid-run loop hit
                // aborts the failover ladder — burning more attempts on a
                // semantically identical output is guaranteed waste.
                let _ = self
                    .store
                    .record_loop_attempt(&loop_key, &out, detector.window);
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

            // Success clears the durable loop window for this task text.
            let _ = self.store.clear_loop_history(&loop_key);

            // Stop rule: acceptance satisfied, no known blocker => stop now.
            if dec.route == Route::Ask {
                st.status = Status::Blocked;
                st.blocked = true;
                st.next_action = format!("answer the question: {}", out);
            } else {
                st.status = Status::Done;
                st.next_action = String::new();
            }
            let _ = self.store.save_task(&mut st);
            self.record(&res, lat);
            return Ok(res);
        }

        st.status = Status::Failed;
        let _ = self.store.save_task(&mut st);
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

    fn record(&self, r: &RunResult, latency_ms: i64) {
        let _ = self.store.record_execution(&Execution {
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
        });
    }
}

/// Shadow-priced order first, then chain order for providers the pricer
/// filtered out (context overflow).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_engine() -> Engine {
        let cfg = Config::default();
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

    #[test]
    fn loop_scope_is_stable_and_trimmed() {
        assert_eq!(loop_scope("  task  "), loop_scope("task"));
        assert_eq!(loop_scope("task").len(), 16);
        assert_ne!(loop_scope("task a"), loop_scope("task b"));
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
