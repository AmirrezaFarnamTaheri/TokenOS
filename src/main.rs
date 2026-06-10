//! TokenOS — Token-Optimal Agent Execution Kernel (native Rust).
//!
//! Deterministic, zero-token routing for LLM agents: route locally, spend
//! upstream tokens only when a cheaper local action cannot finish the task.

mod config;
mod contextidx;
mod engine;
mod kernel;
mod loopdetect;
mod payload;
mod pricing;
mod provider;
mod recorder;
mod store;
mod tokenizer;
mod verify;
mod webui;

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use engine::{Engine, Options};
use std::sync::Arc;

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
    /// Launch the web control panel
    Serve {
        /// Listen port
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Listen host
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli).await {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Run { task, constraints, json, engine: ef } => {
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
                        println!("task     {}\nroute    {}  ({})", res.task_id, res.route.as_str(), res.reason);
                        if !res.provider.is_empty() {
                            println!("provider {} / {}", res.provider, res.model);
                        }
                        println!(
                            "tokens   in={} out={}   latency={}ms   cost=${:.6}   retries={}",
                            res.tokens_in, res.tokens_out, res.latency_ms, res.cost_usd, res.retries
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
                    println!("  {}:{}-{}  [{}] {}", s.file, s.start_line, s.end_line, s.kind, s.name);
                }
            }
            Ok(())
        }
        Command::Providers { config: cfg_path } => {
            let cfg = config::Config::load(cfg_path.as_deref().map(std::path::Path::new))?;
            println!(
                "{:<12} {:<10} {:<9} {:<28} {}",
                "PROVIDER", "ADAPTER", "ENABLED", "MODEL", "FILTER VERDICT"
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
                println!("{:<12} {:<10} {:<9} {:<28} {}", name, p.adapter, enabled, p.model, verdict);
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
            println!("EFFECTIVE COST PER SUCCESSFUL TASK: ${:.6}\n", sum.cost_per_success);
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
                println!("{:<18} {:<12} {}{}", t.task_id, t.status.as_str(), goal, blocked);
            }
            Ok(())
        }
        Command::Trace { task_id, blobs, engine: ef } => {
            let eng = build_engine(&ef)?;
            let events = eng.recorder.events(&task_id)?;
            if events.is_empty() {
                println!("no flight-recorder events for task {}", task_id);
                return Ok(());
            }
            for ev in events {
                println!("{}  {:<9} {}", ev.ts.format("%H:%M:%S"), ev.kind, ev.summary);
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
        Command::Config { action, config: cfg_path } => {
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
        Command::Serve { port, host, engine: ef } => {
            let eng = Arc::new(build_engine(&ef)?);
            println!(
                "TokenOS control panel listening on http://{}:{} (dry-run={})",
                host, port, ef.dry_run
            );
            webui::serve(eng, &host, port).await
        }
    }
}
