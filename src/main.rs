//! TokenOS — Token-Optimal Agent Execution Kernel (native Rust).
//!
//! Deterministic, zero-token routing for LLM agents: route locally, spend
//! upstream tokens only when a cheaper local action cannot finish the task.

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::sync::Arc;
use tokenos::engine::{Engine, Options};
use tokenos::{config, contextidx, provider, recorder, store, webui};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "tokenos",
    version = VERSION,
    about = "TokenOS — Token-Optimal Agent Execution Kernel",
    long_about = "Deterministic, zero-token routing kernel for LLM agents.\n\
                  Route locally; spend upstream tokens only when a cheaper\n\
                  local action cannot finish the task."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Flags shared by engine-backed commands.
#[derive(Args, Clone, Default)]
struct EngineFlags {
    /// Config file path (default ~/.config/tokenos/config.yaml)
    #[arg(long, global = false)]
    config: Option<String>,
    /// State database path (default ~/.local/share/tokenos/tokenos.db)
    #[arg(long)]
    db: Option<String>,
    /// Flight recorder directory (default ~/.local/state/tokenos/traces)
    #[arg(long)]
    traces: Option<String>,
    /// Workspace to index for surgical context
    #[arg(long)]
    workspace: Option<String>,
    /// Force the offline mock adapter (zero live tokens)
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Execute a task through the kernel
    Run {
        /// Task description
        task: Vec<String>,
        /// Semicolon-separated constraints
        #[arg(long, default_value = "")]
        constraints: String,
        /// Emit full result as JSON
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        engine: EngineFlags,
    },
    /// Preview the routing decision (deterministic, zero tokens)
    Route {
        /// Task description
        task: Vec<String>,
        #[command(flatten)]
        engine: EngineFlags,
    },
    /// Build the surgical context index for a workspace
    Index {
        /// Workspace root (default ".")
        root: Option<String>,
        /// Index database path (default: in-memory test run)
        #[arg(long)]
        out: Option<String>,
        /// Optional test query against the fresh index
        #[arg(long)]
        query: Option<String>,
    },
    /// List provider profiles and model filter results
    Providers {
        /// Config file path
        #[arg(long)]
        config: Option<String>,
    },
    /// Route/provider effectiveness; cost per successful task
    Telemetry {
        #[command(flatten)]
        engine: EngineFlags,
    },
    /// Validate local config, store integrity, traces, and telemetry surfaces
    Doctor {
        /// Emit diagnostic report as JSON
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        engine: EngineFlags,
    },
    /// List provider attempts, including failed failover legs
    Attempts {
        /// Max attempts to show
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[command(flatten)]
        engine: EngineFlags,
    },
    /// List compressed task states
    Tasks {
        /// Max tasks to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[command(flatten)]
        engine: EngineFlags,
    },
    /// Replay the flight recorder for a task
    Trace {
        /// Task ID
        task_id: String,
        /// Print full payload blobs
        #[arg(long)]
        blobs: bool,
        #[command(flatten)]
        engine: EngineFlags,
    },
    /// Print effective config or write defaults (config init)
    Config {
        /// Subaction: "init" writes a default config
        action: Option<String>,
        /// Config file path
        #[arg(long)]
        config: Option<String>,
    },
    /// Launch the native desktop app (requires the "native" build feature)
    App {
        #[command(flatten)]
        engine: EngineFlags,
    },
    /// Launch the web control panel
    Serve {
        /// Listen port
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Listen host (loopback by default; see --public)
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Explicitly allow binding a non-loopback interface.
        /// Refused unless an auth token is also configured.
        #[arg(long)]
        public: bool,
        /// Bearer token required on every /api/* request.
        /// Falls back to the TOKENOS_AUTH_TOKEN environment variable.
        #[arg(long)]
        auth_token: Option<String>,
        /// PEM certificate file for native HTTPS serving.
        #[arg(long)]
        tls_cert: Option<String>,
        /// PEM private key file for native HTTPS serving.
        #[arg(long)]
        tls_key: Option<String>,
        #[command(flatten)]
        engine: EngineFlags,
    },
    /// Run routing-accuracy evaluation over a labeled dataset
    Eval {
        /// Labeled dataset path
        #[arg(long)]
        dataset: String,
        /// Sweep the confidence threshold to find the cost-accuracy frontier
        #[arg(long)]
        sweep: bool,
        #[command(flatten)]
        engine: EngineFlags,
    },
}

fn build_engine(ef: &EngineFlags) -> Result<Engine> {
    let mut eng = Engine::new(Options {
        config_path: ef.config.clone(),
        db_path: ef.db.clone(),
        trace_dir: ef.traces.clone(),
        dry_run: ef.dry_run,
    })?;
    if let Some(ws) = &ef.workspace {
        let ix = contextidx::Indexer::open(Some(":memory:"))?;
        let n = ix.index_workspace(std::path::Path::new(ws))?;
        eprintln!("indexed {} symbols from {}", n, ws);
        eng.indexer = Some(ix);
    }
    Ok(eng)
}

fn main() {
    let cli = Cli::parse();

    // The native shell's event loop must OWN the main thread (a hard
    // platform requirement on macOS, and the sane default everywhere), so
    // `app` is dispatched before any tokio runtime exists — run_app spins
    // up its own background runtime for the control plane.
    if let Command::App { engine: ef } = &cli.command {
        #[cfg(feature = "native")]
        {
            let result = build_engine(ef)
                .map(Arc::new)
                .and_then(tokenos::nativeapp::run_app);
            if let Err(e) = result {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(feature = "native"))]
        {
            let _ = ef;
            eprintln!(
                "error: this binary was built without the native desktop shell.\n\
                 rebuild with: cargo build --release --features native\n\
                 (or use the browser dashboard: tokenos serve)"
            );
            std::process::exit(1);
        }
    }

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start async runtime: {}", e);
            std::process::exit(1);
        }
    };
    if let Err(e) = rt.block_on(dispatch(cli)) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        // Handled synchronously in main() before the runtime starts.
        Command::App { .. } => unreachable!("app dispatches before the async runtime"),
        Command::Run {
            task,
            constraints,
            json,
            engine: ef,
        } => {
            let task = task.join(" ").trim().to_string();
            if task.is_empty() {
                return Err(anyhow!("usage: tokenos run \"task description\" [flags]"));
            }
            let eng = build_engine(&ef)?;
            let cons: Vec<String> = constraints
                .split(';')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect();
            match eng.run(&task, &cons).await {
                Ok(res) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&res)?);
                    } else {
                        println!(
                            "task     {}\nroute    {}  ({})",
                            res.task_id,
                            res.route.as_str(),
                            res.reason
                        );
                        if !res.provider.is_empty() {
                            println!("provider {} / {}", res.provider, res.model);
                        }
                        println!(
                            "tokens   in={} out={}   latency={}ms   cost=${:.6}   retries={}",
                            res.tokens_in,
                            res.tokens_out,
                            res.latency_ms,
                            res.cost_usd,
                            res.retries
                        );
                        println!("{}", "-".repeat(60));
                        println!("{}", res.output);
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        Command::Route { task, engine: ef } => {
            let task = task.join(" ").trim().to_string();
            if task.is_empty() {
                return Err(anyhow!("usage: tokenos route \"task description\""));
            }
            let eng = build_engine(&ef)?;
            let (dec, _) = eng.route_only(&task);
            let chain = eng.cfg.provider_chain(dec.route.as_str());
            println!(
                "route       {}\nreason      {}\nconfidence  {:.2}\nest tokens  {}\nchain       {}",
                dec.route.as_str(),
                dec.reason,
                dec.signals.confidence,
                dec.signals.estimated_tokens,
                chain.join(" -> ")
            );
            Ok(())
        }
        Command::Index { root, out, query } => {
            let root = root.unwrap_or_else(|| ".".to_string());
            let ix = contextidx::Indexer::open(out.as_deref())?;
            let n = ix.index_workspace(std::path::Path::new(&root))?;
            println!("indexed {} symbols from {}", n, root);
            if let Some(q) = query {
                for s in ix.search(&q, 5)? {
                    println!(
                        "  {}:{}-{}  [{}] {}",
                        s.file, s.start_line, s.end_line, s.kind, s.name
                    );
                }
            }
            Ok(())
        }
        Command::Providers { config: cfg_path } => {
            let cfg = config::Config::load(cfg_path.as_deref().map(std::path::Path::new))?;
            println!(
                "{:<12} {:<10} {:<9} {:<28} FILTER VERDICT",
                "PROVIDER", "ADAPTER", "ENABLED", "MODEL"
            );
            for (name, p) in &cfg.providers {
                let enabled = if p.disabled { "no" } else { "yes" };
                let verdict = if p.model.is_empty() {
                    "-"
                } else if p.models.is_model_allowed(&p.model) {
                    "ALLOWED"
                } else {
                    "BLOCKED by filter matrix"
                };
                println!(
                    "{:<12} {:<10} {:<9} {:<28} {}",
                    name, p.adapter, enabled, p.model, verdict
                );
                if let Ok(a) = provider::Adapter::new(name, p) {
                    let models = a.models();
                    let allowed = p.models.filter(models.iter().map(|m| m.as_str()));
                    if !allowed.is_empty() {
                        println!("{:<12}   manifest: {}", "", allowed.join(", "));
                    }
                }
            }
            Ok(())
        }
        Command::Telemetry { engine: ef } => {
            let eng = build_engine(&ef)?;
            let sum = eng.store.get_summary()?;
            println!(
                "tasks={}  executions={}  successes={}  success_rate={:.1}%",
                sum.tasks,
                sum.executions,
                sum.successes,
                sum.overall_success_pct * 100.0
            );
            println!(
                "total_tokens={}  total_cost=${:.6}  avg_latency={:.0}ms",
                sum.total_tokens, sum.total_cost_usd, sum.avg_latency_ms
            );
            println!(
                "EFFECTIVE COST PER SUCCESSFUL TASK: ${:.6}\n",
                sum.cost_per_success
            );
            let routes = eng.store.stats_by_route()?;
            if !routes.is_empty() {
                println!(
                    "{:<20} {:>6} {:>9} {:>10} {:>10} {:>12} {:>14}",
                    "ROUTE", "RUNS", "SUCCESS", "AVG_IN", "AVG_OUT", "AVG_LAT_MS", "COST/SUCCESS"
                );
                for r in routes {
                    println!(
                        "{:<20} {:>6} {:>8.1}% {:>10.0} {:>10.0} {:>12.0} {:>14.6}",
                        r.route,
                        r.runs,
                        r.success_rate * 100.0,
                        r.avg_tokens_in,
                        r.avg_tokens_out,
                        r.avg_latency_ms,
                        r.cost_per_success
                    );
                }
            }
            let providers = eng.store.stats_by_provider()?;
            if !providers.is_empty() {
                println!(
                    "\n{:<14} {:>6} {:>9} {:>12} {:>12} {:>12}",
                    "PROVIDER", "RUNS", "SUCCESS", "AVG_LAT_MS", "TOKENS", "COST"
                );
                for p in providers {
                    println!(
                        "{:<14} {:>6} {:>8.1}% {:>12.0} {:>12} {:>12.6}",
                        p.provider,
                        p.runs,
                        p.success_rate * 100.0,
                        p.avg_latency_ms,
                        p.total_tokens,
                        p.total_cost_usd
                    );
                }
            }
            let attempts = eng.store.stats_by_attempts(20)?;
            if !attempts.is_empty() {
                println!(
                    "\n{:<14} {:<12} {:>8} {:>9} {:>12} {:>12} {:>12}",
                    "ATTEMPT ARM", "ROUTE", "ATTEMPTS", "SUCCESS", "AVG_LAT_MS", "TOKENS", "COST"
                );
                for a in attempts {
                    println!(
                        "{:<14} {:<12} {:>8} {:>8.1}% {:>12.0} {:>12} {:>12.6}",
                        a.provider,
                        a.route,
                        a.attempts,
                        a.success_rate * 100.0,
                        a.avg_latency_ms,
                        a.total_tokens,
                        a.total_cost_usd
                    );
                }
            }
            // Live UCB1 bandit standings: process-local evidence.
            // Always printed when arms exist: a fresh process legitimately
            // shows every arm as unexplored (the evidence lives and dies
            // with the serving process), and hiding the table entirely made
            // operators think the bandit was disabled.
            let ranked = eng.bandit.ranked();
            if !ranked.is_empty() {
                println!(
                    "\n{:<14} {:>8} {:>13} {:>14} {:>12}",
                    "BANDIT ARM", "PULLS", "MEAN_REWARD", "MEAN_LAT_MS", "UCB1"
                );
                let mut any_explored = false;
                for (p, score) in &ranked {
                    let (pulls, reward, lat) = eng.bandit.arm_stats(p);
                    if pulls == 0 {
                        println!(
                            "{:<14} {:>8} {:>13} {:>14} {:>12}",
                            p, 0, "-", "-", "unexplored"
                        );
                    } else {
                        any_explored = true;
                        println!(
                            "{:<14} {:>8} {:>13.3} {:>14.0} {:>12.3}",
                            p, pulls, reward, lat, score
                        );
                    }
                }
                if !any_explored {
                    println!("(bandit evidence is process-local — arms gain pulls inside a serving process)");
                }
            }
            // Verified solution cache: durable, zero-token replays.
            if let Ok((entries, test_verified, hits)) = eng.store.solution_cache_stats() {
                if entries > 0 {
                    let static_checked = entries - test_verified;
                    println!("\nSOLUTION CACHE: {entries} cache entr{} (statically-checked: {static_checked}, test-verified: {test_verified}) \u{00b7} {hits} zero-token hit{}",
                        if entries == 1 { "y" } else { "ies" },
                        if hits == 1 { "" } else { "s" });
                }
            }
            // Estimator drift watchdog: process-local calibration.
            let drift = eng.drift.all();
            if !drift.is_empty() {
                println!(
                    "\n{:<14} {:>10} {:>12} {:>10}",
                    "ESTIMATOR", "SAMPLES", "RATIO_EWMA", "STATUS"
                );
                for d in drift {
                    println!(
                        "{:<14} {:>10} {:>12.3} {:>10}",
                        d.provider,
                        d.samples,
                        d.ratio_ewma,
                        if d.drifting { "DRIFTING" } else { "ok" }
                    );
                }
            }
            Ok(())
        }
        Command::Doctor { json, engine: ef } => {
            let eng = build_engine(&ef)?;
            let health = eng.store.health_snapshot()?;
            let store_ok = health.quick_check == "ok";
            let enabled = eng.cfg.providers.values().filter(|p| !p.disabled).count();
            let db_path = ef
                .db
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(store::default_path);
            let trace_path = ef
                .traces
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(recorder::default_dir);
            let provider_verdicts: Vec<serde_json::Value> = eng
                .cfg
                .providers
                .iter()
                .map(|(name, p)| {
                    let model_allowed = p.model.is_empty() || p.models.is_model_allowed(&p.model);
                    serde_json::json!({
                        "provider": name,
                        "adapter": p.adapter,
                        "enabled": !p.disabled,
                        "model": p.model,
                        "model_allowed": model_allowed,
                    })
                })
                .collect();
            let report = serde_json::json!({
                "version": VERSION,
                "mode": if eng.dry_run { "dry-run" } else { "live-capable" },
                "database": db_path.display().to_string(),
                "trace_dir": trace_path.display().to_string(),
                "traces_enabled": !eng.cfg.security.disable_traces,
                "providers_total": eng.cfg.providers.len(),
                "providers_enabled": enabled,
                "workspace_index_enabled": eng.indexer.is_some(),
                "store": health,
                "providers": provider_verdicts,
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let store_health = report
                    .get("store")
                    .and_then(|v| v.as_object())
                    .expect("doctor report store object");
                println!("TokenOS doctor");
                println!("version       {}", VERSION);
                println!(
                    "mode          {}",
                    report["mode"].as_str().unwrap_or("unknown")
                );
                println!("database      {}", db_path.display());
                println!(
                    "traces        {} ({})",
                    trace_path.display(),
                    if eng.cfg.security.disable_traces {
                        "disabled"
                    } else {
                        "enabled"
                    }
                );
                println!(
                    "providers     {}/{} enabled",
                    enabled,
                    eng.cfg.providers.len()
                );
                println!(
                    "store         quick_check={}",
                    store_health["quick_check"].as_str().unwrap_or("unknown")
                );
                println!(
                    "rows          tasks={} executions={} attempts={} traces={} api_stats={}",
                    store_health["tasks"],
                    store_health["executions"],
                    store_health["execution_attempts"],
                    store_health["traces"],
                    store_health["api_request_stats"]
                );
                println!(
                    "cache         entries={} hits={}",
                    store_health["solution_cache"], store_health["solution_cache_hits"]
                );
                println!("status        {}", if store_ok { "OK" } else { "CHECK" });
            }
            if store_ok {
                Ok(())
            } else {
                Err(anyhow!("doctor found SQLite integrity problems"))
            }
        }
        Command::Attempts { limit, engine: ef } => {
            let eng = build_engine(&ef)?;
            let attempts = eng.store.list_attempts(limit)?;
            if attempts.is_empty() {
                println!("no provider attempts recorded");
                return Ok(());
            }
            println!(
                "{:<6} {:<18} {:<10} {:<12} {:<18} {:>8} {:>9} {:>10} {:>10}  ERROR",
                "ID", "TASK", "ROUTE", "PROVIDER", "MODEL", "TOKENS", "LAT_MS", "COST", "OK"
            );
            for a in attempts {
                let tokens = a.tokens_in + a.tokens_out;
                let mut err = a.error_message.clone();
                if err.chars().count() > 80 {
                    err = format!("{}...", err.chars().take(77).collect::<String>());
                }
                println!(
                    "{:<6} {:<18} {:<10} {:<12} {:<18} {:>8} {:>9} {:>10.6} {:>10}  {}",
                    a.id,
                    a.task_id,
                    a.route,
                    a.provider,
                    a.model,
                    tokens,
                    a.latency_ms,
                    a.cost_usd,
                    if a.success { "yes" } else { "no" },
                    err
                );
            }
            Ok(())
        }
        Command::Tasks { limit, engine: ef } => {
            let eng = build_engine(&ef)?;
            let tasks = eng.store.list_tasks(limit)?;
            if tasks.is_empty() {
                println!("no tasks recorded");
                return Ok(());
            }
            for t in tasks {
                let blocked = if t.blocked { "  [BLOCKED]" } else { "" };
                let mut goal = t.goal.clone();
                if goal.chars().count() > 70 {
                    goal = format!("{}...", goal.chars().take(67).collect::<String>());
                }
                println!(
                    "{:<18} {:<12} {}{}",
                    t.task_id,
                    t.status.as_str(),
                    goal,
                    blocked
                );
            }
            Ok(())
        }
        Command::Trace {
            task_id,
            blobs,
            engine: ef,
        } => {
            let eng = build_engine(&ef)?;
            let events = eng.recorder.events(&task_id)?;
            if events.is_empty() {
                println!("no flight-recorder events for task {}", task_id);
                return Ok(());
            }
            for ev in events {
                println!(
                    "{}  {:<9} {}",
                    ev.ts.format("%H:%M:%S"),
                    ev.kind,
                    ev.summary
                );
                if blobs && !ev.blob_sha.is_empty() {
                    if let Ok(blob) = eng.recorder.blob(&ev.blob_sha) {
                        println!("  +- blob {}", &ev.blob_sha[..12]);
                        for line in String::from_utf8_lossy(&blob).split('\n') {
                            println!("  | {}", line);
                        }
                        println!("  +-");
                    }
                }
            }
            Ok(())
        }
        Command::Config {
            action,
            config: cfg_path,
        } => {
            if action.as_deref() == Some("init") {
                let path = cfg_path
                    .as_deref()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(config::Config::default_path);
                if path.exists() {
                    return Err(anyhow!("config already exists at {}", path.display()));
                }
                config::Config::default().save(Some(&path))?;
                println!("wrote default config to {}", path.display());
                return Ok(());
            }
            let cfg = config::Config::load(cfg_path.as_deref().map(std::path::Path::new))?;
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
        Command::Serve {
            port,
            host,
            public,
            auth_token,
            tls_cert,
            tls_key,
            engine: ef,
        } => {
            // The dashboard binds loopback by default.
            // A non-loopback bind requires BOTH --public and an auth token so
            // an unauthenticated control plane can never face a network.
            // An empty token ("") authenticates nothing — treat it as absent
            // from BOTH sources so `--public --auth-token ""` is rejected the
            // same way as a missing token.
            let token = auth_token.filter(|t| !t.is_empty()).or_else(|| {
                std::env::var("TOKENOS_AUTH_TOKEN")
                    .ok()
                    .filter(|t| !t.is_empty())
            });
            let loopback = matches!(host.as_str(), "127.0.0.1" | "::1" | "localhost");
            if !loopback {
                if !public {
                    return Err(anyhow!(
                        "refusing to bind non-loopback host {:?} without --public                          (the dashboard can trigger paid API executions)",
                        host
                    ));
                }
                if token.is_none() {
                    return Err(anyhow!(
                        "--public requires an auth token: pass --auth-token or set                          TOKENOS_AUTH_TOKEN"
                    ));
                }
                eprintln!(
                    "WARNING: dashboard exposed on {host}:{port}; bearer auth is ENFORCED on /api/*"
                );
            }
            let eng = Arc::new(build_engine(&ef)?);
            let tls_paths = match (tls_cert, tls_key) {
                (Some(cert), Some(key)) => Some((cert, key)),
                (None, None) => None,
                _ => {
                    return Err(anyhow!(
                        "--tls-cert and --tls-key must be provided together"
                    ))
                }
            };
            println!(
                "TokenOS control panel listening on {}://{}:{} (dry-run={}, auth={})",
                if tls_paths.is_some() { "https" } else { "http" },
                host,
                port,
                ef.dry_run,
                if token.is_some() {
                    "on"
                } else {
                    "off (loopback only)"
                }
            );
            if let Some((cert, key)) = tls_paths {
                webui::serve_tls(
                    eng,
                    &host,
                    port,
                    token,
                    std::path::Path::new(&cert),
                    std::path::Path::new(&key),
                )
                .await
            } else {
                webui::serve(eng, &host, port, token).await
            }
        }
        Command::Eval {
            dataset,
            sweep,
            engine: ef,
        } => run_eval(&dataset, sweep, &ef).await,
    }
}

#[derive(Debug, serde::Deserialize)]
struct EvalItem {
    #[serde(alias = "prompt", alias = "goal")]
    task: String,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(alias = "expected")]
    expected_route: String,
}

fn route_cost(r: &str) -> f64 {
    match r {
        "DIRECT" => tokenos::kernel::Route::Direct.cost(),
        "REUSE" => tokenos::kernel::Route::Reuse.cost(),
        "PATCH" => tokenos::kernel::Route::Patch.cost(),
        "IMPLEMENT" => tokenos::kernel::Route::Implement.cost(),
        "PARTIAL" => tokenos::kernel::Route::Partial.cost(),
        "DELEGATE" => tokenos::kernel::Route::Delegate.cost(),
        "ASK" => tokenos::kernel::Route::Ask.cost(),
        "VERIFY" => tokenos::kernel::Route::Verify.cost(),
        "ESCALATE-CONFLICT" => tokenos::kernel::Route::EscalateConflict.cost(),
        "ESCALATE-SAFETY" => tokenos::kernel::Route::EscalateSafety.cost(),
        "ESCALATE-EXTERNAL" => tokenos::kernel::Route::EscalateExternal.cost(),
        _ => 0.02,
    }
}

async fn run_eval(dataset_path: &str, sweep: bool, ef: &EngineFlags) -> Result<()> {
    let content = std::fs::read_to_string(dataset_path)
        .with_context(|| format!("failed to read dataset file at {}", dataset_path))?;

    let items: Vec<EvalItem> = if dataset_path.ends_with(".yaml") || dataset_path.ends_with(".yml")
    {
        serde_yaml::from_str(&content).context("failed to parse dataset as YAML")?
    } else if dataset_path.ends_with(".json") {
        serde_json::from_str(&content).context("failed to parse dataset as JSON")?
    } else {
        serde_json::from_str(&content)
            .or_else(|_| serde_yaml::from_str(&content))
            .context("failed to parse dataset as JSON or YAML")?
    };

    if items.is_empty() {
        return Err(anyhow!("evaluation dataset is empty"));
    }

    let total = items.len();

    // Compute weak baseline: always predicting the most frequent expected route
    let mut counts = std::collections::HashMap::new();
    for item in &items {
        *counts
            .entry(item.expected_route.trim().to_uppercase())
            .or_insert(0) += 1;
    }
    let most_frequent_route = counts
        .iter()
        .max_by_key(|e| e.1)
        .map(|e| e.0.clone())
        .unwrap_or_else(|| "IMPLEMENT".to_string());
    let weak_correct = items
        .iter()
        .filter(|item| item.expected_route.trim().to_uppercase() == most_frequent_route)
        .count();
    let accuracy_weak = weak_correct as f64 / total as f64;

    let mut eng = build_engine(ef)?;

    if sweep {
        println!(
            "{:<15} {:<12} {:<15} {:<15} {:<10}",
            "THRESHOLD", "ACCURACY", "ROUTER COST", "SAVINGS", "APGR"
        );
        println!("{}", "-".repeat(70));
        let thresholds = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];
        for &t in &thresholds {
            eng.cfg.policy.ask_threshold = t;
            let mut correct = 0;
            let mut cost_router = 0.0;
            for item in &items {
                let (dec, _) = eng.route_only_with_constraints(&item.task, &item.constraints);
                let predicted = dec.route.as_str();
                let expected = item.expected_route.trim().to_uppercase();
                if predicted == expected {
                    correct += 1;
                }
                cost_router += route_cost(predicted);
            }
            let accuracy = (correct as f64 / total as f64) * 100.0;
            let cost_strong = total as f64 * route_cost("IMPLEMENT");
            let savings = cost_strong - cost_router;
            let apgr = if accuracy_weak < 1.0 {
                (((accuracy / 100.0) - accuracy_weak) / (1.0 - accuracy_weak)).max(0.0) * 100.0
            } else {
                100.0
            };
            println!(
                "{:<15.2} {:<12.2}% ${:<14.4} ${:<14.4} {:<9.2}%",
                t, accuracy, cost_router, savings, apgr
            );
        }
        return Ok(());
    }

    let mut correct = 0;
    let mut mismatches = Vec::new();
    let mut total_router_cost = 0.0;
    let mut total_strong_cost = 0.0;

    println!("{:<4} {:<15} {:<15} TASK", "NUM", "EXPECTED", "PREDICTED");
    println!("{}", "-".repeat(80));

    for (idx, item) in items.iter().enumerate() {
        let (dec, _) = eng.route_only_with_constraints(&item.task, &item.constraints);
        let predicted = dec.route.as_str();
        let expected = item.expected_route.trim().to_uppercase();
        let is_ok = predicted == expected;

        total_router_cost += route_cost(predicted);
        total_strong_cost += route_cost("IMPLEMENT");

        let status_mark = if is_ok {
            correct += 1;
            "+"
        } else {
            mismatches.push((idx + 1, item, predicted.to_string(), dec.reason.clone()));
            "x"
        };

        let task_trunc = if item.task.chars().count() > 45 {
            format!("{}...", item.task.chars().take(42).collect::<String>())
        } else {
            item.task.clone()
        };

        println!(
            "{:<4} {:<4} {:<15} {:<15} {:?}",
            status_mark,
            idx + 1,
            expected,
            predicted,
            task_trunc
        );
    }

    println!("{}", "-".repeat(80));
    let accuracy = (correct as f64 / total as f64) * 100.0;
    let savings_usd = total_strong_cost - total_router_cost;
    let savings_pct = (savings_usd / total_strong_cost) * 100.0;
    let apgr = if accuracy_weak < 1.0 {
        (((accuracy / 100.0) - accuracy_weak) / (1.0 - accuracy_weak)).max(0.0) * 100.0
    } else {
        100.0
    };

    println!("Evaluation results:");
    println!("  Total items:       {}", total);
    println!("  Correct:           {}", correct);
    println!("  Incorrect:         {}", total - correct);
    println!("  Accuracy:          {:.2}%", accuracy);
    println!(
        "  Weak Baseline Acc: {:.2}% (always {})",
        accuracy_weak * 100.0,
        most_frequent_route
    );
    println!("  APGR Metric:       {:.2}%", apgr);
    println!("  Router Est Cost:   ${:.4}", total_router_cost);
    println!(
        "  Strong Est Cost:   ${:.4} (always IMPLEMENT)",
        total_strong_cost
    );
    println!(
        "  Est USD Savings:   ${:.4} ({:.2}% saved)",
        savings_usd, savings_pct
    );

    if !mismatches.is_empty() {
        println!("\nMismatches detail:");
        println!("{}", "=".repeat(80));
        for (num, item, pred, reason) in mismatches {
            println!("Item #{} - {}", num, item.task);
            println!("  Expected:  {}", item.expected_route.trim().to_uppercase());
            println!("  Predicted: {}", pred);
            println!("  Reason:    {}", reason);
            if !item.constraints.is_empty() {
                println!("  Constraints: {:?}", item.constraints);
            }
            println!("{}", "-".repeat(80));
        }
    }

    Ok(())
}
