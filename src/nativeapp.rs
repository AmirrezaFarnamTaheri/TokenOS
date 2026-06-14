//! Native desktop application for TokenOS (feature `native`).
//!
//! `tokenos app` is a real native desktop surface built with egui/eframe. It
//! talks directly to the Rust engine and SQLite store; it does not start the
//! Axum web dashboard, does not bind a loopback port, and does not open a
//! browser.

use std::collections::HashMap;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use eframe::egui::{
    self, Align, Color32, Context, CornerRadius, FontFamily, FontId, Grid, Layout, RichText,
    ScrollArea, Sense, Stroke, StrokeKind, TextEdit, Ui, Vec2, ViewportBuilder,
};
use eframe::{App, Frame, NativeOptions};
use serde::Deserialize;
use tokio::runtime::Runtime;

use crate::engine::{Engine, RunResult};
use crate::kernel::{Decision, Route, RouterPolicy, Signals, State};
use crate::pricing::DriftStatus;
use crate::recorder::Event;
use crate::store::{
    ApiRequestStats, AttemptStats, DailySpend, Execution, ExecutionAttempt, GenAiStats,
    ProviderStats, RouteStats, StoreHealth, Summary,
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const INITIAL_TASK: &str = "Fix the typo in the README header";
const SAMPLE_EVAL_DATASET: &str = r#"- task: "fix the typo in the README header"
  expected_route: "DIRECT"
- task: "adjust memory allocation bounds in the server config"
  constraints:
    - "keep public CLI flags stable"
  expected_route: "PATCH"
- task: "maybe somehow do something with the thing"
  expected_route: "ASK"
"#;

/// Launches the native desktop application and blocks until the user closes it.
pub fn run_app(engine: Arc<Engine>) -> Result<()> {
    let rt = Runtime::new().map_err(|e| anyhow!("starting native async runtime: {e}"))?;
    let native_options = NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("TokenOS")
            .with_inner_size([1280.0, 840.0])
            .with_min_inner_size([980.0, 680.0]),
        ..Default::default()
    };

    eframe::run_native(
        "TokenOS",
        native_options,
        Box::new(move |cc| {
            install_style(&cc.egui_ctx);
            Ok(Box::new(TokenOsNativeApp::new(engine, rt)))
        }),
    )
    .map_err(|e| anyhow!("running native app: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Dashboard,
    Console,
    Planner,
    PolicyLab,
    Calibration,
    Operations,
    Tasks,
    Executions,
    Config,
}

impl View {
    fn label(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Console => "Run Console",
            Self::Planner => "Route Planner",
            Self::PolicyLab => "Policy Lab",
            Self::Calibration => "Calibration",
            Self::Operations => "Operations",
            Self::Tasks => "Tasks",
            Self::Executions => "Executions",
            Self::Config => "Configuration",
        }
    }
}

#[derive(Default)]
struct Snapshot {
    summary: Option<Summary>,
    routes: Vec<RouteStats>,
    providers: Vec<ProviderStats>,
    attempts: Vec<AttemptStats>,
    api_stats: Vec<ApiRequestStats>,
    raw_attempts: Vec<ExecutionAttempt>,
    gen_ai: Vec<GenAiStats>,
    bandit: Vec<BanditArmView>,
    drift: Vec<DriftStatus>,
    breakers: Vec<BreakerView>,
    history: Vec<DailySpend>,
    tasks: Vec<State>,
    executions: Vec<Execution>,
    health: Option<StoreHealth>,
    solution_cache: Option<(i64, i64, i64)>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct BanditArmView {
    provider: String,
    score: Option<f64>,
    pulls: u64,
    mean_reward: f64,
    mean_latency_ms: f64,
}

#[derive(Debug, Clone)]
struct BreakerView {
    provider: String,
    avg_latency_ms: f64,
    fail_rate: f64,
    calls_in_window: usize,
    status: String,
    consecutive_429s: u32,
}

struct RunMessage {
    task: String,
    result: Result<RunResult, String>,
}

#[derive(Debug, Clone, Deserialize)]
struct EvalItem {
    #[serde(alias = "prompt", alias = "goal")]
    task: String,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(alias = "expected")]
    expected_route: String,
}

#[derive(Debug, Clone)]
struct EvalMismatch {
    index: usize,
    task: String,
    expected_route: String,
    predicted_route: String,
    reason: String,
    constraints: Vec<String>,
}

#[derive(Debug, Clone)]
struct SweepRow {
    threshold: f64,
    accuracy: f64,
    router_cost: f64,
    savings: f64,
    apgr: f64,
}

#[derive(Debug, Clone)]
struct EvalReport {
    total: usize,
    correct: usize,
    accuracy: f64,
    accuracy_weak: f64,
    most_frequent_route: String,
    apgr: f64,
    total_router_cost: f64,
    total_strong_cost: f64,
    savings_usd: f64,
    savings_pct: f64,
    mismatches: Vec<EvalMismatch>,
    sweep: Option<Vec<SweepRow>>,
}

#[derive(Debug, Clone)]
struct BatchRouteResult {
    index: usize,
    task: String,
    route: Route,
    confidence: f64,
    estimated_tokens: usize,
    provider_chain: Vec<String>,
    reason: String,
}

#[derive(Debug, Clone)]
struct PolicyLabResult {
    current: Decision,
    simulated: Decision,
    constraints: Vec<String>,
}

struct TokenOsNativeApp {
    engine: Arc<Engine>,
    rt: Runtime,
    view: View,
    snapshot: Snapshot,
    last_refresh: Instant,
    task_input: String,
    constraints_input: String,
    preview: Option<(Decision, String)>,
    running: bool,
    run_tx: mpsc::Sender<RunMessage>,
    run_rx: mpsc::Receiver<RunMessage>,
    last_run: Option<RunMessage>,
    task_filter: String,
    exec_filter: String,
    batch_input: String,
    batch_constraints_input: String,
    batch_results: Vec<BatchRouteResult>,
    batch_error: Option<String>,
    policy_lab_task: String,
    policy_lab_constraints: String,
    policy_lab_policy: RouterPolicy,
    policy_lab_result: Option<PolicyLabResult>,
    eval_dataset_input: String,
    eval_sweep: bool,
    eval_result: Option<EvalReport>,
    eval_error: Option<String>,
    selected_task_id: Option<String>,
    trace_events: Vec<Event>,
    trace_error: Option<String>,
    status: String,
}

impl TokenOsNativeApp {
    fn new(engine: Arc<Engine>, rt: Runtime) -> Self {
        let (run_tx, run_rx) = mpsc::channel();
        let policy_lab_policy = engine.cfg.policy.clone();
        let mut app = Self {
            engine,
            rt,
            view: View::Dashboard,
            snapshot: Snapshot::default(),
            last_refresh: Instant::now() - REFRESH_INTERVAL,
            task_input: INITIAL_TASK.to_string(),
            constraints_input: String::new(),
            preview: None,
            running: false,
            run_tx,
            run_rx,
            last_run: None,
            task_filter: String::new(),
            exec_filter: String::new(),
            batch_input: "fix typo in README\nadjust memory allocation bounds in server config\nmaybe somehow do something with the thing".to_string(),
            batch_constraints_input: String::new(),
            batch_results: Vec::new(),
            batch_error: None,
            policy_lab_task: "adjust memory allocation bounds in the server config".to_string(),
            policy_lab_constraints: String::new(),
            policy_lab_policy,
            policy_lab_result: None,
            eval_dataset_input: SAMPLE_EVAL_DATASET.to_string(),
            eval_sweep: true,
            eval_result: None,
            eval_error: None,
            selected_task_id: None,
            trace_events: Vec::new(),
            trace_error: None,
            status: "ready".to_string(),
        };
        app.refresh_snapshot();
        app
    }

    fn refresh_snapshot(&mut self) {
        self.last_refresh = Instant::now();
        self.snapshot = match load_snapshot(&self.engine) {
            Ok(snapshot) => {
                self.status = "telemetry refreshed".to_string();
                snapshot
            }
            Err(e) => Snapshot {
                error: Some(e.to_string()),
                ..Snapshot::default()
            },
        };
    }

    fn poll_run(&mut self) {
        while let Ok(msg) = self.run_rx.try_recv() {
            self.running = false;
            self.status = match &msg.result {
                Ok(res) => format!(
                    "completed {} through {} in {}ms",
                    msg.task, res.route, res.latency_ms
                ),
                Err(e) => format!("execution failed: {e}"),
            };
            self.last_run = Some(msg);
            self.refresh_snapshot();
        }
    }

    fn route_preview(&mut self) {
        let task = self.task_input.trim();
        if task.is_empty() {
            self.status = "enter a task before previewing".to_string();
            return;
        }
        let constraints = constraints_from_text(&self.constraints_input);
        let (decision, reason) = self.engine.route_only_with_constraints(task, &constraints);
        self.preview = Some((decision, reason));
        self.status = "route preview updated without provider spend".to_string();
    }

    fn execute(&mut self) {
        let task = self.task_input.trim().to_string();
        if task.is_empty() {
            self.status = "enter a task before executing".to_string();
            return;
        }
        if self.running {
            self.status = "an execution is already running".to_string();
            return;
        }
        let constraints = constraints_from_text(&self.constraints_input);
        let engine = self.engine.clone();
        let tx = self.run_tx.clone();
        let task_for_status = task.clone();
        self.running = true;
        self.status = format!("running {task_for_status}");
        self.rt.spawn(async move {
            let result = engine
                .run(&task, &constraints)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(RunMessage { task, result });
        });
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(6.0);
            ui.label(
                RichText::new("⬢ TokenOS")
                    .strong()
                    .size(18.0)
                    .color(accent()),
            );
            ui.separator();
            let mode = if self.engine.dry_run {
                "DRY-RUN · offline"
            } else {
                "LIVE · provider spend enabled"
            };
            ui.label(
                RichText::new(mode)
                    .monospace()
                    .color(if self.engine.dry_run {
                        accent()
                    } else {
                        warn()
                    }),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Refresh").clicked() {
                    self.refresh_snapshot();
                }
                ui.label(RichText::new(&self.status).small().color(muted()));
            });
        });
    }

    fn side_nav(&mut self, ui: &mut Ui) {
        ui.add_space(8.0);
        for view in [
            View::Dashboard,
            View::Console,
            View::Planner,
            View::PolicyLab,
            View::Calibration,
            View::Operations,
            View::Tasks,
            View::Executions,
            View::Config,
        ] {
            let selected = self.view == view;
            if ui
                .add_sized(
                    [166.0, 34.0],
                    egui::Button::selectable(selected, view.label()),
                )
                .clicked()
            {
                self.view = view;
            }
        }
        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            ui.label(
                RichText::new("Cost per successful task is the primary metric.")
                    .small()
                    .color(muted()),
            );
        });
    }

    fn content(&mut self, ui: &mut Ui) {
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| match self.view {
                View::Dashboard => self.dashboard(ui),
                View::Console => self.console(ui),
                View::Planner => self.planner(ui),
                View::PolicyLab => self.policy_lab(ui),
                View::Calibration => self.calibration(ui),
                View::Operations => self.operations(ui),
                View::Tasks => self.tasks(ui),
                View::Executions => self.executions(ui),
                View::Config => self.config(ui),
            });
    }

    fn dashboard(&mut self, ui: &mut Ui) {
        heading(
            ui,
            "Dashboard",
            "native telemetry, local state, and kernel health",
        );
        if let Some(err) = &self.snapshot.error {
            error_box(ui, err);
        }
        if let Some(sum) = &self.snapshot.summary {
            metric_grid(
                ui,
                &[
                    ("Cost / Success", usd(sum.cost_per_success), true),
                    ("Estimated Savings", usd(sum.savings_usd), true),
                    ("Total Cost", usd(sum.total_cost_usd), false),
                    (
                        "Success Rate",
                        pct(sum.overall_success_pct),
                        sum.overall_success_pct >= 0.9,
                    ),
                    ("Executions", sum.executions.to_string(), false),
                    ("Tasks", sum.tasks.to_string(), false),
                    ("Total Tokens", sum.total_tokens.to_string(), false),
                    ("Avg Latency", ms(sum.avg_latency_ms), false),
                ],
            );
        }

        ui.columns(2, |cols| {
            panel(&mut cols[0], "Route Effectiveness", |ui| {
                route_stats_table(ui, &self.snapshot.routes)
            });
            panel(&mut cols[1], "Provider Health", |ui| {
                provider_stats_table(ui, &self.snapshot.providers)
            });
        });
        ui.columns(2, |cols| {
            panel(&mut cols[0], "Daily Spend & Success", |ui| {
                spend_chart(ui, &self.snapshot.history)
            });
            panel(&mut cols[1], "System Health", |ui| {
                if let Some(h) = &self.snapshot.health {
                    Grid::new("health_grid").striped(true).show(ui, |ui| {
                        kv_row(ui, "SQLite", &h.quick_check);
                        kv_row(ui, "Tasks", h.tasks.to_string());
                        kv_row(ui, "Executions", h.executions.to_string());
                        kv_row(ui, "Attempts", h.execution_attempts.to_string());
                        kv_row(ui, "Traces", h.traces.to_string());
                        kv_row(ui, "API stats", h.api_request_stats.to_string());
                        kv_row(ui, "Cache entries", h.solution_cache.to_string());
                        kv_row(ui, "Cache hits", h.solution_cache_hits.to_string());
                    });
                } else {
                    ui.label(RichText::new("No health snapshot available.").color(muted()));
                }
            });
        });
        ui.columns(2, |cols| {
            panel(&mut cols[0], "Bandit Standings", |ui| {
                bandit_table(ui, &self.snapshot.bandit)
            });
            panel(&mut cols[1], "Estimator Drift & Cache", |ui| {
                drift_table(ui, &self.snapshot.drift, self.snapshot.solution_cache)
            });
        });
        panel(ui, "Provider Attempt Aggregates", |ui| {
            attempt_stats_table(ui, &self.snapshot.attempts)
        });
    }

    fn console(&mut self, ui: &mut Ui) {
        heading(
            ui,
            "Run Console",
            "preview locally, execute through the engine, inspect the result",
        );
        panel(ui, "Task", |ui| {
            ui.label(RichText::new("Task").color(muted()));
            ui.add(
                TextEdit::multiline(&mut self.task_input)
                    .desired_rows(3)
                    .hint_text("Describe the task to route or execute"),
            );
            ui.add_space(8.0);
            ui.label(RichText::new("Constraints, one per line").color(muted()));
            ui.add(
                TextEdit::multiline(&mut self.constraints_input)
                    .desired_rows(2)
                    .hint_text("must not change public API"),
            );
            ui.horizontal(|ui| {
                if ui.button("Preview Route").clicked() {
                    self.route_preview();
                }
                let run_label = if self.running {
                    "Running..."
                } else {
                    "Execute"
                };
                if ui
                    .add_enabled(!self.running, egui::Button::new(run_label).fill(accent()))
                    .clicked()
                {
                    self.execute();
                }
                if ui.button("Example").clicked() {
                    self.task_input = INITIAL_TASK.to_string();
                    self.constraints_input.clear();
                    self.preview = None;
                }
                if ui.button("Clear").clicked() {
                    self.task_input.clear();
                    self.constraints_input.clear();
                    self.preview = None;
                    self.last_run = None;
                }
            });
        });
        if let Some((decision, reason)) = &self.preview {
            panel(ui, "Routing Decision", |ui| {
                ui.horizontal(|ui| {
                    route_pill(ui, decision.route);
                    ui.label(
                        RichText::new(format!(
                            "confidence {:.0}%",
                            decision.signals.confidence * 100.0
                        ))
                        .color(muted()),
                    );
                    ui.label(
                        RichText::new(format!(
                            "estimated {} tokens",
                            decision.signals.estimated_tokens
                        ))
                        .color(muted()),
                    );
                });
                ui.add_space(8.0);
                ui.label(reason);
                ui.add_space(8.0);
                signal_grid(ui, &decision.signals);
                ui.add_space(8.0);
                provider_chain_table(ui, &self.engine, decision.route);
            });
        }
        if let Some(msg) = &self.last_run {
            panel(ui, "Execution Result", |ui| match &msg.result {
                Ok(res) => {
                    ui.horizontal(|ui| {
                        route_pill(ui, res.route);
                        ui.label(RichText::new(&res.provider).monospace().color(muted()));
                        ui.label(RichText::new(&res.model).monospace().color(muted()));
                        ui.label(RichText::new(format!("{}ms", res.latency_ms)).color(muted()));
                        ui.label(RichText::new(usd(res.cost_usd)).color(accent()));
                    });
                    ui.add_space(8.0);
                    ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                        ui.add(
                            TextEdit::multiline(&mut res.output.clone())
                                .font(egui::TextStyle::Monospace)
                                .desired_rows(10)
                                .interactive(false),
                        );
                    });
                }
                Err(e) => error_box(ui, e),
            });
        }
    }

    fn planner(&mut self, ui: &mut Ui) {
        heading(
            ui,
            "Route Planner",
            "bulk zero-cost triage for tasks, provider chains, and savings",
        );
        panel(ui, "Batch Input", |ui| {
            ui.label(RichText::new("Tasks, one per line").color(muted()));
            ui.add(
                TextEdit::multiline(&mut self.batch_input)
                    .desired_rows(8)
                    .hint_text(
                        "fix typo in README\nimplement login rate limiter\nmaybe update the thing",
                    ),
            );
            ui.add_space(8.0);
            ui.label(RichText::new("Shared constraints, one per line").color(muted()));
            ui.add(
                TextEdit::multiline(&mut self.batch_constraints_input)
                    .desired_rows(2)
                    .hint_text("no public API changes"),
            );
            ui.horizontal(|ui| {
                if ui.button("Preview Batch").clicked() {
                    match parse_batch_tasks(&self.batch_input) {
                        Ok(tasks) => {
                            let constraints = constraints_from_text(&self.batch_constraints_input);
                            self.batch_results = route_batch(&self.engine, &tasks, &constraints);
                            self.batch_error = None;
                            self.status = format!("planned {} tasks locally", self.batch_results.len());
                        }
                        Err(e) => {
                            self.batch_results.clear();
                            self.batch_error = Some(e.to_string());
                            self.status = "batch planner input rejected".to_string();
                        }
                    }
                }
                if ui.button("Load Sample").clicked() {
                    self.batch_input = "fix typo in README\nadjust memory allocation bounds in server config\nimplement a complete audit export command\nmaybe somehow do something with the thing".to_string();
                    self.batch_constraints_input = "keep public CLI flags stable".to_string();
                    self.batch_results.clear();
                    self.batch_error = None;
                }
                if ui.button("Clear Results").clicked() {
                    self.batch_results.clear();
                    self.batch_error = None;
                }
            });
        });
        if let Some(err) = &self.batch_error {
            error_box(ui, err);
        }
        if !self.batch_results.is_empty() {
            let total = self.batch_results.len();
            let router_cost: f64 = self.batch_results.iter().map(|r| r.route.cost()).sum();
            let strong_cost = total as f64 * Route::Implement.cost();
            let savings = strong_cost - router_cost;
            let avg_confidence =
                self.batch_results.iter().map(|r| r.confidence).sum::<f64>() / total as f64;
            let local_stops = self
                .batch_results
                .iter()
                .filter(|r| r.route.is_terminal_local() || r.route == Route::Reuse)
                .count();
            metric_grid(
                ui,
                &[
                    ("Tasks", total.to_string(), false),
                    ("Avg Confidence", pct(avg_confidence), avg_confidence >= 0.6),
                    ("Local Stops", local_stops.to_string(), local_stops > 0),
                    ("Router Cost", usd(router_cost), router_cost <= strong_cost),
                    ("Strong Cost", usd(strong_cost), false),
                    ("Estimated Savings", usd(savings), savings >= 0.0),
                    (
                        "Savings %",
                        format!(
                            "{:.1}%",
                            if strong_cost > 0.0 {
                                savings / strong_cost * 100.0
                            } else {
                                0.0
                            }
                        ),
                        savings >= 0.0,
                    ),
                    (
                        "Total Est Tokens",
                        self.batch_results
                            .iter()
                            .map(|r| r.estimated_tokens)
                            .sum::<usize>()
                            .to_string(),
                        false,
                    ),
                ],
            );
            ui.columns(2, |cols| {
                panel(&mut cols[0], "Route Mix", |ui| {
                    route_mix_table(ui, &self.batch_results)
                });
                panel(&mut cols[1], "Provider Demand", |ui| {
                    provider_demand_table(ui, &self.batch_results)
                });
            });
            panel(ui, "Planned Tasks", |ui| {
                batch_results_table(ui, &self.batch_results)
            });
        }
    }

    fn policy_lab(&mut self, ui: &mut Ui) {
        heading(
            ui,
            "Policy Lab",
            "simulate routing thresholds without mutating config or spending tokens",
        );
        ui.columns(2, |cols| {
            panel(&mut cols[0], "Scenario", |ui| {
                ui.label(RichText::new("Task").color(muted()));
                ui.add(
                    TextEdit::multiline(&mut self.policy_lab_task)
                        .desired_rows(4)
                        .hint_text("Describe one task to simulate"),
                );
                ui.add_space(8.0);
                ui.label(RichText::new("Constraints, one per line").color(muted()));
                ui.add(
                    TextEdit::multiline(&mut self.policy_lab_constraints)
                        .desired_rows(3)
                        .hint_text("keep public API stable"),
                );
                ui.horizontal(|ui| {
                    if ui.button("Simulate").clicked() {
                        let task = self.policy_lab_task.trim();
                        if task.is_empty() {
                            self.status = "policy lab requires a task".to_string();
                            self.policy_lab_result = None;
                        } else {
                            let constraints = constraints_from_text(&self.policy_lab_constraints);
                            let (current, _) =
                                self.engine.route_only_with_constraints(task, &constraints);
                            let (simulated, _) = self.engine.route_only_with_policy_constraints(
                                task,
                                &constraints,
                                &self.policy_lab_policy,
                            );
                            self.status = format!(
                                "policy simulation: {} -> {}",
                                current.route, simulated.route
                            );
                            self.policy_lab_result = Some(PolicyLabResult {
                                current,
                                simulated,
                                constraints,
                            });
                        }
                    }
                    if ui.button("Reset Policy").clicked() {
                        self.policy_lab_policy = self.engine.cfg.policy.clone();
                        self.policy_lab_result = None;
                        self.status = "policy lab reset to effective config".to_string();
                    }
                    if ui.button("Use Console Task").clicked() {
                        self.policy_lab_task = self.task_input.clone();
                        self.policy_lab_constraints = self.constraints_input.clone();
                    }
                });
            });
            panel(&mut cols[1], "Policy Controls", |ui| {
                policy_controls(ui, &mut self.policy_lab_policy);
            });
        });
        if let Some(result) = &self.policy_lab_result {
            let changed = result.current.route != result.simulated.route;
            let delta_cost = result.simulated.route.cost() - result.current.route.cost();
            metric_grid(
                ui,
                &[
                    ("Route Changed", yes_no(changed), changed),
                    ("Current Route", result.current.route.to_string(), false),
                    (
                        "Simulated Route",
                        result.simulated.route.to_string(),
                        changed,
                    ),
                    ("Cost Delta", usd(delta_cost), delta_cost <= 0.0),
                    (
                        "Current Confidence",
                        pct(result.current.signals.confidence),
                        result.current.signals.confidence >= 0.6,
                    ),
                    (
                        "Sim Confidence",
                        pct(result.simulated.signals.confidence),
                        result.simulated.signals.confidence >= 0.6,
                    ),
                    (
                        "Est Tokens",
                        result.simulated.signals.estimated_tokens.to_string(),
                        false,
                    ),
                    (
                        "Constraints",
                        result.constraints.len().to_string(),
                        !result.constraints.is_empty(),
                    ),
                ],
            );
            ui.columns(2, |cols| {
                panel(&mut cols[0], "Effective Config Decision", |ui| {
                    decision_panel(ui, &self.engine, &result.current)
                });
                panel(&mut cols[1], "Simulated Decision", |ui| {
                    decision_panel(ui, &self.engine, &result.simulated)
                });
            });
        }
    }

    fn calibration(&mut self, ui: &mut Ui) {
        heading(
            ui,
            "Calibration",
            "evaluate route labels and tune ASK threshold locally",
        );
        panel(ui, "Evaluation Dataset", |ui| {
            ui.label(
                RichText::new("YAML or JSON list of tasks with expected_route").color(muted()),
            );
            ui.add(
                TextEdit::multiline(&mut self.eval_dataset_input)
                    .font(egui::TextStyle::Monospace)
                    .desired_rows(12)
                    .hint_text(SAMPLE_EVAL_DATASET),
            );
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.eval_sweep, "Threshold sweep");
                if ui.button("Load Sample").clicked() {
                    self.eval_dataset_input = SAMPLE_EVAL_DATASET.to_string();
                    self.eval_result = None;
                    self.eval_error = None;
                }
                if ui.button("Run Evaluation").clicked() {
                    match parse_eval_items(&self.eval_dataset_input)
                        .map(|items| evaluate_items(&self.engine, &items, self.eval_sweep))
                    {
                        Ok(report) => {
                            self.status = format!("evaluated {} route labels", report.total);
                            self.eval_result = Some(report);
                            self.eval_error = None;
                        }
                        Err(e) => {
                            self.status = "evaluation dataset rejected".to_string();
                            self.eval_result = None;
                            self.eval_error = Some(e.to_string());
                        }
                    }
                }
            });
        });
        if let Some(err) = &self.eval_error {
            error_box(ui, err);
        }
        if let Some(report) = &self.eval_result {
            metric_grid(
                ui,
                &[
                    (
                        "Accuracy",
                        format!("{:.1}%", report.accuracy),
                        report.accuracy >= 80.0,
                    ),
                    (
                        "Weak Baseline",
                        format!("{:.1}%", report.accuracy_weak),
                        false,
                    ),
                    ("APGR", format!("{:.1}%", report.apgr), report.apgr >= 50.0),
                    (
                        "Savings",
                        usd(report.savings_usd),
                        report.savings_usd >= 0.0,
                    ),
                    (
                        "Correct",
                        format!("{}/{}", report.correct, report.total),
                        false,
                    ),
                    (
                        "Router Cost",
                        usd(report.total_router_cost),
                        report.total_router_cost <= report.total_strong_cost,
                    ),
                    ("Strong Cost", usd(report.total_strong_cost), false),
                    (
                        "Savings %",
                        format!("{:.1}%", report.savings_pct),
                        report.savings_pct >= 0.0,
                    ),
                    ("Common Label", report.most_frequent_route.clone(), false),
                ],
            );
            if let Some(rows) = &report.sweep {
                panel(ui, "ASK Threshold Sweep", |ui| {
                    sweep_chart(ui, rows);
                    sweep_table(ui, rows);
                });
            }
            panel(ui, "Mismatches", |ui| {
                mismatch_table(ui, &report.mismatches)
            });
        }
    }

    fn operations(&mut self, ui: &mut Ui) {
        heading(
            ui,
            "Operations",
            "control-plane parity, live breakers, attempts, and GenAI telemetry",
        );
        ui.columns(2, |cols| {
            panel(&mut cols[0], "Circuit Breakers", |ui| {
                breaker_table(ui, &self.snapshot.breakers)
            });
            panel(&mut cols[1], "API Surface", |ui| {
                api_stats_table(ui, &self.snapshot.api_stats)
            });
        });
        ui.columns(2, |cols| {
            panel(&mut cols[0], "Estimator Drift & Cache", |ui| {
                drift_table(ui, &self.snapshot.drift, self.snapshot.solution_cache)
            });
            panel(&mut cols[1], "OpenTelemetry GenAI Rollup", |ui| {
                gen_ai_table(ui, &self.snapshot.gen_ai)
            });
        });
        panel(ui, "Recent Provider Attempts", |ui| {
            raw_attempts_table(ui, &self.snapshot.raw_attempts)
        });
    }

    fn tasks(&mut self, ui: &mut Ui) {
        heading(ui, "Tasks", "compressed state objects persisted in SQLite");
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.text_edit_singleline(&mut self.task_filter);
        });
        let q = self.task_filter.to_lowercase();
        panel(ui, "Recent Tasks", |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                Grid::new("task_table")
                    .striped(true)
                    .min_col_width(90.0)
                    .show(ui, |ui| {
                        table_head(ui, &["ID", "Status", "Blocked", "Goal", "Next Action"]);
                        for task in self.snapshot.tasks.iter().filter(|t| {
                            q.is_empty()
                                || t.goal.to_lowercase().contains(&q)
                                || t.task_id.contains(&q)
                        }) {
                            let selected = self.selected_task_id.as_deref() == Some(&task.task_id);
                            ui.label(RichText::new(&task.task_id).monospace().small());
                            ui.label(status_text(task.status.as_str()));
                            ui.label(if task.blocked { "yes" } else { "no" });
                            ui.label(wrap(&task.goal, 72));
                            ui.label(RichText::new(wrap(&task.next_action, 56)).color(muted()));
                            if ui
                                .add(egui::Button::selectable(selected, "Trace"))
                                .clicked()
                            {
                                self.selected_task_id = Some(task.task_id.clone());
                                match self.engine.recorder.events(&task.task_id) {
                                    Ok(events) => {
                                        self.trace_events = events;
                                        self.trace_error = None;
                                    }
                                    Err(e) => {
                                        self.trace_events.clear();
                                        self.trace_error = Some(e.to_string());
                                    }
                                }
                            }
                            ui.end_row();
                        }
                    });
            });
        });
        if let Some(id) = &self.selected_task_id {
            panel(ui, "Flight Recorder", |ui| {
                ui.label(RichText::new(id).monospace().color(muted()));
                if let Some(err) = &self.trace_error {
                    error_box(ui, err);
                } else {
                    trace_table(ui, &self.trace_events);
                }
            });
        }
    }

    fn executions(&mut self, ui: &mut Ui) {
        heading(ui, "Executions", "durable execution telemetry");
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.text_edit_singleline(&mut self.exec_filter);
        });
        let q = self.exec_filter.to_lowercase();
        panel(ui, "Recent Executions", |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                Grid::new("execution_table")
                    .striped(true)
                    .min_col_width(68.0)
                    .show(ui, |ui| {
                        table_head(
                            ui,
                            &[
                                "#", "Task", "Route", "Provider", "Tokens", "Latency", "Cost", "OK",
                            ],
                        );
                        for e in self.snapshot.executions.iter().filter(|e| {
                            q.is_empty()
                                || e.task_id.to_lowercase().contains(&q)
                                || e.route.to_lowercase().contains(&q)
                                || e.provider.to_lowercase().contains(&q)
                        }) {
                            ui.label(e.id.to_string());
                            ui.label(RichText::new(&e.task_id).monospace().small());
                            route_label(ui, &e.route);
                            ui.label(&e.provider);
                            ui.label((e.tokens_in + e.tokens_out).to_string());
                            ui.label(format!("{}ms", e.latency_ms));
                            ui.label(usd(e.est_cost_usd));
                            ui.label(if e.success {
                                RichText::new("ok").color(good())
                            } else {
                                RichText::new("fail").color(bad())
                            });
                            ui.end_row();
                        }
                    });
            });
        });
        panel(ui, "Provider Attempts", |ui| {
            raw_attempts_table(ui, &self.snapshot.raw_attempts)
        });
    }

    fn config(&mut self, ui: &mut Ui) {
        heading(
            ui,
            "Configuration",
            "effective providers, policy, and native runtime mode",
        );
        ui.columns(2, |cols| {
            panel(&mut cols[0], "Native Runtime", |ui| {
                kv_row(ui, "UI", "egui/eframe native desktop");
                kv_row(ui, "Control plane", "direct engine/store calls");
                kv_row(ui, "Web server", "not started by tokenos app");
                kv_row(
                    ui,
                    "Mode",
                    if self.engine.dry_run {
                        "dry-run"
                    } else {
                        "live"
                    },
                );
                if let Some((entries, verified, hits)) = self.snapshot.solution_cache {
                    kv_row(ui, "Cache entries", entries.to_string());
                    kv_row(ui, "Test verified", verified.to_string());
                    kv_row(ui, "Zero-token hits", hits.to_string());
                }
            });
            panel(&mut cols[1], "Router Policy", |ui| {
                let p = &self.engine.cfg.policy;
                kv_row(ui, "ASK threshold", format!("{:.2}", p.ask_threshold));
                kv_row(ui, "DIRECT max tokens", p.direct_max_tokens.to_string());
                kv_row(ui, "Delegation penalty", p.delegation_penalty.to_string());
                kv_row(
                    ui,
                    "Delegation min scale",
                    format!("{:.2}", p.delegation_min_scale),
                );
                kv_row(ui, "Max cost per task", usd(p.max_cost_per_task_usd));
                kv_row(ui, "Re-ask limit", p.re_ask_limit.to_string());
            });
        });
        panel(ui, "Providers", |ui| {
            Grid::new("config_providers").striped(true).show(ui, |ui| {
                table_head(ui, &["Provider", "Adapter", "Model", "Priority", "Enabled"]);
                for (name, provider) in &self.engine.cfg.providers {
                    ui.label(name);
                    ui.label(&provider.adapter);
                    ui.label(if provider.model.is_empty() {
                        "default"
                    } else {
                        &provider.model
                    });
                    ui.label(provider.priority.to_string());
                    ui.label(if provider.disabled { "no" } else { "yes" });
                    ui.end_row();
                }
            });
        });
    }
}

impl App for TokenOsNativeApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut Frame) {
        self.poll_run();
        if self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.refresh_snapshot();
        }
        self.top_bar(ui);
        ui.separator();
        ui.horizontal(|ui| {
            ui.set_height(ui.available_height());
            ui.vertical(|ui| {
                ui.set_width(190.0);
                self.side_nav(ui);
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                self.content(ui);
            });
        });
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}

fn load_snapshot(engine: &Engine) -> Result<Snapshot> {
    Ok(Snapshot {
        summary: Some(engine.store.get_summary()?),
        routes: engine.store.stats_by_route()?,
        providers: engine.store.stats_by_provider()?,
        attempts: engine.store.stats_by_attempts(100)?,
        api_stats: engine.store.stats_by_api_route(100)?,
        raw_attempts: engine.store.list_attempts(300)?,
        gen_ai: engine.store.stats_by_gen_ai()?,
        bandit: bandit_snapshot(engine),
        drift: engine.drift.all(),
        breakers: breaker_snapshot(engine),
        history: engine.store.get_daily_spend()?,
        tasks: engine.store.list_tasks(100)?,
        executions: engine.store.list_executions(200)?,
        health: Some(engine.store.health_snapshot()?),
        solution_cache: Some(engine.store.solution_cache_stats()?),
        error: None,
    })
}

fn bandit_snapshot(engine: &Engine) -> Vec<BanditArmView> {
    engine
        .bandit
        .ranked()
        .into_iter()
        .map(|(provider, score)| {
            let (pulls, mean_reward, mean_latency_ms) = engine.bandit.arm_stats(&provider);
            BanditArmView {
                provider,
                score: score.is_finite().then_some(score),
                pulls,
                mean_reward,
                mean_latency_ms,
            }
        })
        .collect()
}

fn breaker_snapshot(engine: &Engine) -> Vec<BreakerView> {
    let mut out: Vec<_> = engine
        .tracker
        .health_snapshots()
        .into_iter()
        .map(
            |(
                provider,
                avg_latency_ms,
                fail_rate,
                calls_in_window,
                in_cooldown,
                consecutive_429s,
                half_open_in_flight,
            )| {
                let status = if in_cooldown {
                    "COOLDOWN"
                } else if consecutive_429s > 0 && half_open_in_flight {
                    "HALF-OPEN"
                } else {
                    "CLOSED"
                };
                BreakerView {
                    provider,
                    avg_latency_ms,
                    fail_rate,
                    calls_in_window,
                    status: status.to_string(),
                    consecutive_429s,
                }
            },
        )
        .collect();
    out.sort_by(|a, b| a.provider.cmp(&b.provider));
    out
}

fn parse_batch_tasks(input: &str) -> Result<Vec<String>> {
    let tasks: Vec<String> = input
        .lines()
        .map(str::trim)
        .map(|line| {
            line.trim_start_matches(|c: char| {
                c == '-' || c == '*' || c == '•' || c.is_ascii_digit() || c == '.' || c == ')'
            })
            .trim()
        })
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if tasks.is_empty() {
        Err(anyhow!("batch planner requires at least one task line"))
    } else {
        Ok(tasks)
    }
}

fn route_batch(engine: &Engine, tasks: &[String], constraints: &[String]) -> Vec<BatchRouteResult> {
    tasks
        .iter()
        .enumerate()
        .map(|(idx, task)| {
            let (decision, _) = engine.route_only_with_constraints(task, constraints);
            BatchRouteResult {
                index: idx + 1,
                task: task.clone(),
                route: decision.route,
                confidence: decision.signals.confidence,
                estimated_tokens: decision.signals.estimated_tokens,
                provider_chain: engine.cfg.provider_chain(decision.route.as_str()),
                reason: decision.reason,
            }
        })
        .collect()
}

fn parse_eval_items(input: &str) -> Result<Vec<EvalItem>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("evaluation dataset is empty"));
    }
    let mut items: Vec<EvalItem> = serde_yaml::from_str(trimmed).or_else(|yaml_err| {
        serde_json::from_str(trimmed).map_err(|json_err| {
            anyhow!("dataset must be YAML or JSON: yaml={yaml_err}; json={json_err}")
        })
    })?;
    if items.is_empty() {
        return Err(anyhow!("evaluation dataset contains no items"));
    }
    for (idx, item) in items.iter_mut().enumerate() {
        item.task = item.task.trim().to_string();
        item.expected_route = item.expected_route.trim().to_uppercase();
        item.constraints = item
            .constraints
            .iter()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect();
        if item.task.is_empty() {
            return Err(anyhow!("item {} has an empty task", idx + 1));
        }
        if item.expected_route.is_empty() {
            return Err(anyhow!("item {} has an empty expected_route", idx + 1));
        }
    }
    Ok(items)
}

fn evaluate_items(engine: &Engine, items: &[EvalItem], sweep: bool) -> EvalReport {
    let total = items.len();
    let mut counts = HashMap::<String, usize>::new();
    for item in items {
        *counts.entry(item.expected_route.clone()).or_default() += 1;
    }
    let most_frequent_route = counts
        .iter()
        .max_by_key(|(_, count)| *count)
        .map(|(route, _)| route.clone())
        .unwrap_or_else(|| "IMPLEMENT".to_string());
    let weak_correct = items
        .iter()
        .filter(|item| item.expected_route == most_frequent_route)
        .count();
    let accuracy_weak = weak_correct as f64 / total as f64;

    let sweep_rows = if sweep {
        let thresholds = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];
        Some(
            thresholds
                .into_iter()
                .map(|threshold| {
                    let mut policy = engine.cfg.policy.clone();
                    policy.ask_threshold = threshold;
                    let mut correct = 0;
                    let mut router_cost = 0.0;
                    for item in items {
                        let (dec, _) = engine.route_only_with_policy_constraints(
                            &item.task,
                            &item.constraints,
                            &policy,
                        );
                        if dec.route.as_str() == item.expected_route {
                            correct += 1;
                        }
                        router_cost += dec.route.cost();
                    }
                    let accuracy = correct as f64 / total as f64 * 100.0;
                    let strong_cost = total as f64 * Route::Implement.cost();
                    let savings = strong_cost - router_cost;
                    SweepRow {
                        threshold,
                        accuracy,
                        router_cost,
                        savings,
                        apgr: apgr(accuracy / 100.0, accuracy_weak),
                    }
                })
                .collect(),
        )
    } else {
        None
    };

    let mut correct = 0;
    let mut total_router_cost = 0.0;
    let mut mismatches = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let (dec, _) = engine.route_only_with_constraints(&item.task, &item.constraints);
        let predicted = dec.route.as_str().to_string();
        total_router_cost += dec.route.cost();
        if predicted == item.expected_route {
            correct += 1;
        } else {
            mismatches.push(EvalMismatch {
                index: idx + 1,
                task: item.task.clone(),
                expected_route: item.expected_route.clone(),
                predicted_route: predicted,
                reason: dec.reason,
                constraints: item.constraints.clone(),
            });
        }
    }

    let total_strong_cost = total as f64 * Route::Implement.cost();
    let savings_usd = total_strong_cost - total_router_cost;
    EvalReport {
        total,
        correct,
        accuracy: correct as f64 / total as f64 * 100.0,
        accuracy_weak: accuracy_weak * 100.0,
        most_frequent_route,
        apgr: apgr(correct as f64 / total as f64, accuracy_weak),
        total_router_cost,
        total_strong_cost,
        savings_usd,
        savings_pct: if total_strong_cost > 0.0 {
            savings_usd / total_strong_cost * 100.0
        } else {
            0.0
        },
        mismatches,
        sweep: sweep_rows,
    }
}

fn apgr(accuracy: f64, weak_accuracy: f64) -> f64 {
    if weak_accuracy < 1.0 {
        ((accuracy - weak_accuracy) / (1.0 - weak_accuracy)).max(0.0) * 100.0
    } else {
        100.0
    }
}

fn install_style(ctx: &Context) {
    let mut style = (*ctx.global_style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.window_fill = bg();
    style.visuals.panel_fill = bg();
    style.visuals.extreme_bg_color = panel_dark();
    style.visuals.widgets.inactive.bg_fill = panel_fill();
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(33, 41, 59);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(38, 48, 69);
    style.visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(94, 234, 212, 70);
    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 8.0);
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        FontId::new(13.0, FontFamily::Monospace),
    );
    ctx.set_global_style(style);
}

fn heading(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).size(24.0).strong());
    ui.label(RichText::new(subtitle).color(muted()));
    ui.add_space(14.0);
}

fn panel<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong().color(accent()));
        ui.add_space(8.0);
        add(ui)
    })
    .inner
}

fn metric_grid(ui: &mut Ui, metrics: &[(&str, String, bool)]) {
    egui::Grid::new("metric_grid")
        .num_columns(4)
        .spacing([12.0, 12.0])
        .show(ui, |ui| {
            for (idx, (label, value, highlighted)) in metrics.iter().enumerate() {
                metric_card(ui, label, value, *highlighted);
                if (idx + 1) % 4 == 0 {
                    ui.end_row();
                }
            }
            if !metrics.is_empty() && !metrics.len().is_multiple_of(4) {
                ui.end_row();
            }
        });
    ui.add_space(10.0);
}

fn metric_card(ui: &mut Ui, label: &str, value: &str, highlighted: bool) {
    let fill = if highlighted {
        Color32::from_rgb(20, 42, 50)
    } else {
        panel_fill()
    };
    egui::Frame::default()
        .fill(fill)
        .stroke(Stroke::new(1.0, Color32::from_rgb(43, 52, 74)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_width(170.0);
            ui.label(RichText::new(label).small().color(muted()));
            ui.label(
                RichText::new(value)
                    .size(20.0)
                    .monospace()
                    .strong()
                    .color(if highlighted { accent() } else { text() }),
            );
        });
}

fn route_stats_table(ui: &mut Ui, routes: &[RouteStats]) {
    if routes.is_empty() {
        ui.label(RichText::new("No route telemetry yet.").color(muted()));
        return;
    }
    Grid::new("route_stats").striped(true).show(ui, |ui| {
        table_head(
            ui,
            &[
                "Route",
                "Runs",
                "Success",
                "Avg Tokens",
                "Latency",
                "Cost / Success",
            ],
        );
        for r in routes {
            route_label(ui, &r.route);
            ui.label(r.runs.to_string());
            ui.label(pct(r.success_rate));
            ui.label(format!("{:.0}/{:.0}", r.avg_tokens_in, r.avg_tokens_out));
            ui.label(ms(r.avg_latency_ms));
            ui.label(usd(r.cost_per_success));
            ui.end_row();
        }
    });
}

fn provider_stats_table(ui: &mut Ui, providers: &[ProviderStats]) {
    if providers.is_empty() {
        ui.label(RichText::new("No provider calls yet.").color(muted()));
        return;
    }
    Grid::new("provider_stats").striped(true).show(ui, |ui| {
        table_head(
            ui,
            &["Provider", "Runs", "Success", "Latency", "Tokens", "Cost"],
        );
        for p in providers {
            ui.label(&p.provider);
            ui.label(p.runs.to_string());
            ui.label(pct(p.success_rate));
            ui.label(ms(p.avg_latency_ms));
            ui.label(p.total_tokens.to_string());
            ui.label(usd(p.total_cost_usd));
            ui.end_row();
        }
    });
}

fn attempt_stats_table(ui: &mut Ui, attempts: &[AttemptStats]) {
    if attempts.is_empty() {
        ui.label(RichText::new("No provider attempts recorded yet.").color(muted()));
        return;
    }
    Grid::new("attempt_stats").striped(true).show(ui, |ui| {
        table_head(
            ui,
            &[
                "Provider", "Route", "Attempts", "Success", "Latency", "Tokens", "Cost",
            ],
        );
        for a in attempts.iter().take(16) {
            ui.label(&a.provider);
            route_label(ui, &a.route);
            ui.label(a.attempts.to_string());
            ui.label(pct(a.success_rate));
            ui.label(ms(a.avg_latency_ms));
            ui.label(a.total_tokens.to_string());
            ui.label(usd(a.total_cost_usd));
            ui.end_row();
        }
    });
}

fn route_mix_table(ui: &mut Ui, rows: &[BatchRouteResult]) {
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for row in rows {
        *counts.entry(row.route.as_str()).or_default() += 1;
    }
    let mut mix: Vec<_> = counts.into_iter().collect();
    mix.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    Grid::new("route_mix").striped(true).show(ui, |ui| {
        table_head(ui, &["Route", "Tasks", "Share"]);
        for (route, count) in mix {
            route_label(ui, route);
            ui.label(count.to_string());
            ui.label(pct(count as f64 / rows.len() as f64));
            ui.end_row();
        }
    });
}

fn provider_demand_table(ui: &mut Ui, rows: &[BatchRouteResult]) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut local = 0;
    for row in rows {
        if row.route.is_terminal_local() || row.route == Route::Reuse {
            local += 1;
        } else if let Some(provider) = row.provider_chain.first() {
            *counts.entry(provider.clone()).or_default() += 1;
        } else {
            local += 1;
        }
    }
    let mut demand: Vec<_> = counts.into_iter().collect();
    demand.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Grid::new("provider_demand").striped(true).show(ui, |ui| {
        table_head(ui, &["Provider", "First-leg Tasks", "Share"]);
        if local > 0 {
            ui.label(RichText::new("local").monospace().color(accent()));
            ui.label(local.to_string());
            ui.label(pct(local as f64 / rows.len() as f64));
            ui.end_row();
        }
        for (provider, count) in demand {
            ui.label(provider);
            ui.label(count.to_string());
            ui.label(pct(count as f64 / rows.len() as f64));
            ui.end_row();
        }
    });
}

fn batch_results_table(ui: &mut Ui, rows: &[BatchRouteResult]) {
    ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
        Grid::new("batch_results").striped(true).show(ui, |ui| {
            table_head(
                ui,
                &[
                    "#",
                    "Route",
                    "Confidence",
                    "Tokens",
                    "Cost",
                    "Provider Chain",
                    "Task",
                    "Reason",
                ],
            );
            for r in rows {
                ui.label(r.index.to_string());
                route_label(ui, r.route.as_str());
                ui.label(pct(r.confidence));
                ui.label(r.estimated_tokens.to_string());
                ui.label(usd(r.route.cost()));
                ui.label(wrap(&r.provider_chain.join(" -> "), 34));
                ui.label(wrap(&r.task, 42));
                ui.label(RichText::new(wrap(&r.reason, 56)).color(muted()));
                ui.end_row();
            }
        });
    });
}

fn api_stats_table(ui: &mut Ui, rows: &[ApiRequestStats]) {
    if rows.is_empty() {
        ui.label(RichText::new("No web/API requests recorded yet.").color(muted()));
        return;
    }
    Grid::new("api_stats").striped(true).show(ui, |ui| {
        table_head(
            ui,
            &[
                "Method",
                "Path",
                "Status",
                "Count",
                "Avg",
                "Max",
                "Last Seen",
            ],
        );
        for r in rows.iter().take(18) {
            ui.label(RichText::new(&r.method).monospace());
            ui.label(wrap(&r.path, 36));
            ui.label(r.status.to_string());
            ui.label(r.count.to_string());
            ui.label(ms(r.avg_latency_ms));
            ui.label(ms(r.max_latency_ms));
            ui.label(wrap(&r.last_seen_at, 24));
            ui.end_row();
        }
    });
}

fn bandit_table(ui: &mut Ui, arms: &[BanditArmView]) {
    if arms.is_empty() {
        ui.label(RichText::new("No provider arms configured.").color(muted()));
        return;
    }
    Grid::new("bandit_stats").striped(true).show(ui, |ui| {
        table_head(ui, &["Provider", "Pulls", "Reward", "Latency", "UCB1"]);
        for arm in arms {
            ui.label(&arm.provider);
            ui.label(arm.pulls.to_string());
            ui.label(if arm.pulls == 0 {
                "-".to_string()
            } else {
                format!("{:.3}", arm.mean_reward)
            });
            ui.label(if arm.pulls == 0 {
                "-".to_string()
            } else {
                ms(arm.mean_latency_ms)
            });
            ui.label(match arm.score {
                Some(score) => format!("{score:.3}"),
                None => "unexplored".to_string(),
            });
            ui.end_row();
        }
    });
}

fn drift_table(ui: &mut Ui, drift: &[DriftStatus], cache: Option<(i64, i64, i64)>) {
    if let Some((entries, verified, hits)) = cache {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("cache entries {entries}")).monospace());
            ui.label(RichText::new(format!("test verified {verified}")).monospace());
            ui.label(RichText::new(format!("zero-token hits {hits}")).monospace());
        });
        ui.add_space(8.0);
    }
    if drift.is_empty() {
        ui.label(RichText::new("No drift samples yet.").color(muted()));
        return;
    }
    Grid::new("drift_stats").striped(true).show(ui, |ui| {
        table_head(ui, &["Provider", "Samples", "Ratio", "Status"]);
        for d in drift {
            ui.label(&d.provider);
            ui.label(d.samples.to_string());
            ui.label(format!("{:.3}", d.ratio_ewma));
            ui.label(if d.drifting {
                RichText::new("DRIFTING").color(bad())
            } else {
                RichText::new("ok").color(good())
            });
            ui.end_row();
        }
    });
}

fn breaker_table(ui: &mut Ui, rows: &[BreakerView]) {
    if rows.is_empty() {
        ui.label(RichText::new("No live provider breaker observations yet.").color(muted()));
        return;
    }
    Grid::new("breaker_stats").striped(true).show(ui, |ui| {
        table_head(
            ui,
            &["Provider", "Status", "Calls", "Fail", "Latency", "429s"],
        );
        for r in rows {
            ui.label(&r.provider);
            ui.label(match r.status.as_str() {
                "CLOSED" => RichText::new("CLOSED").color(good()),
                "HALF-OPEN" => RichText::new("HALF-OPEN").color(warn()),
                _ => RichText::new(&r.status).color(bad()),
            });
            ui.label(r.calls_in_window.to_string());
            ui.label(pct(r.fail_rate));
            ui.label(ms(r.avg_latency_ms));
            ui.label(r.consecutive_429s.to_string());
            ui.end_row();
        }
    });
}

fn gen_ai_table(ui: &mut Ui, rows: &[GenAiStats]) {
    if rows.is_empty() {
        ui.label(RichText::new("No GenAI attempt telemetry yet.").color(muted()));
        return;
    }
    Grid::new("gen_ai_stats").striped(true).show(ui, |ui| {
        table_head(
            ui,
            &[
                "Provider", "Model", "Route", "Runs", "OK", "Tokens", "Cost", "Latency",
            ],
        );
        for r in rows.iter().take(18) {
            ui.label(&r.provider);
            ui.label(wrap(&r.model, 22));
            route_label(ui, &r.route);
            ui.label(r.runs.to_string());
            ui.label(r.successes.to_string());
            ui.label((r.tokens_in + r.tokens_out).to_string());
            ui.label(usd(r.cost_usd));
            ui.label(ms(r.avg_latency_ms));
            ui.end_row();
        }
    });
}

fn raw_attempts_table(ui: &mut Ui, rows: &[ExecutionAttempt]) {
    if rows.is_empty() {
        ui.label(RichText::new("No provider attempt rows recorded yet.").color(muted()));
        return;
    }
    ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
        Grid::new("raw_attempts").striped(true).show(ui, |ui| {
            table_head(
                ui,
                &[
                    "#", "Task", "Route", "Provider", "Model", "Tokens", "Latency", "Cost", "OK",
                    "Error",
                ],
            );
            for a in rows.iter().take(60) {
                ui.label(a.id.to_string());
                ui.label(RichText::new(wrap(&a.task_id, 16)).monospace().small());
                route_label(ui, &a.route);
                ui.label(&a.provider);
                ui.label(wrap(&a.model, 18));
                ui.label((a.tokens_in + a.tokens_out).to_string());
                ui.label(format!("{}ms", a.latency_ms));
                ui.label(usd(a.cost_usd));
                ui.label(if a.success {
                    RichText::new("ok").color(good())
                } else {
                    RichText::new("fail").color(bad())
                });
                ui.label(RichText::new(wrap(&a.error_message, 40)).color(muted()));
                ui.end_row();
            }
        });
    });
}

fn mismatch_table(ui: &mut Ui, rows: &[EvalMismatch]) {
    if rows.is_empty() {
        ui.label(RichText::new("No mismatches.").color(good()));
        return;
    }
    Grid::new("eval_mismatches").striped(true).show(ui, |ui| {
        table_head(
            ui,
            &[
                "#",
                "Expected",
                "Predicted",
                "Task",
                "Constraints",
                "Reason",
            ],
        );
        for m in rows.iter().take(40) {
            ui.label(m.index.to_string());
            route_label(ui, &m.expected_route);
            route_label(ui, &m.predicted_route);
            ui.label(wrap(&m.task, 44));
            ui.label(wrap(&m.constraints.join("; "), 36));
            ui.label(RichText::new(wrap(&m.reason, 54)).color(muted()));
            ui.end_row();
        }
    });
}

fn sweep_table(ui: &mut Ui, rows: &[SweepRow]) {
    Grid::new("eval_sweep").striped(true).show(ui, |ui| {
        table_head(
            ui,
            &[
                "ASK Threshold",
                "Accuracy",
                "APGR",
                "Savings",
                "Router Cost",
            ],
        );
        for r in rows {
            ui.label(format!("{:.1}", r.threshold));
            ui.label(format!("{:.1}%", r.accuracy));
            ui.label(format!("{:.1}%", r.apgr));
            ui.label(usd(r.savings));
            ui.label(usd(r.router_cost));
            ui.end_row();
        }
    });
}

fn signal_grid(ui: &mut Ui, signals: &Signals) {
    Grid::new("routing_signals").striped(true).show(ui, |ui| {
        table_head(ui, &["Signal", "Value", "Signal", "Value"]);
        signal_row(
            ui,
            ("confidence", format!("{:.0}%", signals.confidence * 100.0)),
            ("estimated_tokens", signals.estimated_tokens.to_string()),
        );
        signal_row(
            ui,
            ("trivial", yes_no(signals.trivial)),
            ("localized_change", yes_no(signals.localized_change)),
        );
        signal_row(
            ui,
            (
                "has_existing_solution",
                yes_no(signals.has_existing_solution),
            ),
            ("repetitive", yes_no(signals.repetitive)),
        );
        signal_row(
            ui,
            ("bounded", yes_no(signals.bounded)),
            ("external_blocker", yes_no(signals.external_blocker)),
        );
        signal_row(
            ui,
            (
                "conflicting_requirements",
                yes_no(signals.conflicting_requirements),
            ),
            ("safety_violation", yes_no(signals.safety_violation)),
        );
        signal_row(
            ui,
            (
                "missing_critical_info",
                yes_no(signals.missing_critical_info),
            ),
            ("repeated_failure", yes_no(signals.repeated_failure)),
        );
        signal_row(
            ui,
            ("loop_detected", yes_no(signals.loop_detected)),
            ("route_budget", "local decision".to_string()),
        );
    });
}

fn policy_controls(ui: &mut Ui, policy: &mut RouterPolicy) {
    ui.add(egui::Slider::new(&mut policy.ask_threshold, 0.0..=1.0).text("ASK threshold"));
    ui.add(egui::Slider::new(&mut policy.direct_max_tokens, 0..=10_000).text("DIRECT max tokens"));
    ui.add(
        egui::Slider::new(&mut policy.delegation_penalty, 0.0..=10_000.0)
            .text("Delegation penalty"),
    );
    ui.add(
        egui::Slider::new(&mut policy.delegation_min_scale, 0.0..=10.0)
            .text("Delegation min scale"),
    );
    ui.add(
        egui::Slider::new(&mut policy.max_cost_per_task_usd, 0.0..=1.0).text("Max cost per task"),
    );
    ui.add(
        egui::Slider::new(&mut policy.semantic_cache_threshold, 0.0..=1.0)
            .text("Semantic cache threshold"),
    );
    ui.add(egui::Slider::new(&mut policy.cascade_threshold, 0.0..=1.0).text("Cascade threshold"));
    ui.add(
        egui::Slider::new(&mut policy.cascade_max_escalations, 0..=10)
            .text("Cascade max escalations"),
    );
    ui.add(egui::Slider::new(&mut policy.re_ask_limit, 0..=5).text("Re-ask limit"));
    ui.checkbox(&mut policy.reuse_cache, "Reuse verified solution cache");
    ui.checkbox(
        &mut policy.opt_in_learned_routing,
        "Opt-in learned routing fallback",
    );
}

fn decision_panel(ui: &mut Ui, engine: &Engine, decision: &Decision) {
    ui.horizontal(|ui| {
        route_pill(ui, decision.route);
        ui.label(
            RichText::new(format!("cost {}", usd(decision.route.cost())))
                .monospace()
                .color(muted()),
        );
    });
    ui.add_space(8.0);
    ui.label(&decision.reason);
    ui.add_space(8.0);
    signal_grid(ui, &decision.signals);
    ui.add_space(8.0);
    provider_chain_table(ui, engine, decision.route);
}

fn signal_row(ui: &mut Ui, left: (&str, String), right: (&str, String)) {
    ui.label(RichText::new(left.0).small().color(muted()));
    ui.label(RichText::new(left.1).monospace());
    ui.label(RichText::new(right.0).small().color(muted()));
    ui.label(RichText::new(right.1).monospace());
    ui.end_row();
}

fn provider_chain_table(ui: &mut Ui, engine: &Engine, route: Route) {
    let chain = engine.cfg.provider_chain(route.as_str());
    if chain.is_empty() {
        ui.label(RichText::new("No enabled providers in this route chain.").color(warn()));
        return;
    }
    ui.label(RichText::new("Provider chain").strong().color(muted()));
    Grid::new("provider_chain").striped(true).show(ui, |ui| {
        table_head(
            ui,
            &[
                "#", "Provider", "Adapter", "Model", "Priority", "Context", "Input", "Output",
            ],
        );
        for (idx, name) in chain.iter().enumerate() {
            if let Some(p) = engine.cfg.providers.get(name) {
                ui.label((idx + 1).to_string());
                ui.label(name);
                ui.label(&p.adapter);
                ui.label(if p.model.is_empty() {
                    "default"
                } else {
                    &p.model
                });
                ui.label(p.priority.to_string());
                ui.label(p.max_context.to_string());
                ui.label(usd(p.cost_per_mtok_in));
                ui.label(usd(p.cost_per_mtok_out));
                ui.end_row();
            }
        }
    });
}

fn trace_table(ui: &mut Ui, events: &[Event]) {
    if events.is_empty() {
        ui.label(RichText::new("No trace events recorded for this task.").color(muted()));
        return;
    }
    Grid::new("trace_events").striped(true).show(ui, |ui| {
        table_head(ui, &["When", "Kind", "Summary", "Blob"]);
        for ev in events.iter().take(40) {
            ui.label(wrap(&ev.ts.to_rfc3339(), 24));
            ui.label(RichText::new(&ev.kind).monospace());
            ui.label(wrap(&ev.summary, 58));
            ui.label(
                RichText::new(wrap(&ev.blob_sha, 18))
                    .monospace()
                    .color(muted()),
            );
            ui.end_row();
        }
    });
}

fn spend_chart(ui: &mut Ui, history: &[DailySpend]) {
    let desired = Vec2::new(ui.available_width(), 180.0);
    let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(8), panel_dark());
    painter.rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0, Color32::from_rgb(43, 52, 74)),
        StrokeKind::Outside,
    );
    if history.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No spend data recorded yet",
            FontId::proportional(13.0),
            muted(),
        );
        return;
    }
    let max_cost = history
        .iter()
        .map(|h| h.cost_usd)
        .fold(0.0001_f64, f64::max);
    let n = history.len().max(1) as f32;
    let gap = 5.0;
    let bar_w = ((rect.width() - gap * (n + 1.0)) / n).clamp(4.0, 30.0);
    for (idx, day) in history.iter().enumerate() {
        let x = rect.left() + gap + idx as f32 * (bar_w + gap);
        let h = ((day.cost_usd / max_cost) as f32 * (rect.height() - 40.0)).max(3.0);
        let y = rect.bottom() - 26.0 - h;
        let bar = egui::Rect::from_min_size(egui::pos2(x, y), Vec2::new(bar_w, h));
        painter.rect_filled(bar, CornerRadius::same(3), accent());
        if day.runs > 0 {
            let success_h = h * (day.successes as f32 / day.runs as f32).clamp(0.0, 1.0);
            let success = egui::Rect::from_min_size(
                egui::pos2(x, y + h - success_h),
                Vec2::new(bar_w, success_h),
            );
            painter.rect_filled(success, CornerRadius::same(3), good());
        }
    }
}

fn sweep_chart(ui: &mut Ui, rows: &[SweepRow]) {
    let desired = Vec2::new(ui.available_width(), 180.0);
    let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(8), panel_dark());
    painter.rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0, Color32::from_rgb(43, 52, 74)),
        StrokeKind::Outside,
    );
    if rows.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No sweep rows",
            FontId::proportional(13.0),
            muted(),
        );
        return;
    }
    let n = rows.len() as f32;
    let gap = 5.0;
    let bar_w = ((rect.width() - gap * (n + 1.0)) / n).clamp(8.0, 36.0);
    for (idx, row) in rows.iter().enumerate() {
        let x = rect.left() + gap + idx as f32 * (bar_w + gap);
        let accuracy_h = ((row.accuracy / 100.0) as f32 * (rect.height() - 34.0)).max(2.0);
        let y = rect.bottom() - 22.0 - accuracy_h;
        let bar = egui::Rect::from_min_size(egui::pos2(x, y), Vec2::new(bar_w, accuracy_h));
        painter.rect_filled(bar, CornerRadius::same(3), Color32::from_rgb(96, 165, 250));
        let apgr_h = ((row.apgr / 100.0) as f32 * (rect.height() - 34.0)).max(2.0);
        let apgr_bar = egui::Rect::from_min_size(
            egui::pos2(x + bar_w * 0.58, rect.bottom() - 22.0 - apgr_h),
            Vec2::new(bar_w * 0.42, apgr_h),
        );
        painter.rect_filled(apgr_bar, CornerRadius::same(2), accent());
    }
}

fn table_head(ui: &mut Ui, labels: &[&str]) {
    for label in labels {
        ui.label(RichText::new(*label).small().strong().color(muted()));
    }
    ui.end_row();
}

fn kv_row(ui: &mut Ui, label: impl Into<String>, value: impl Into<String>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label.into()).color(muted()));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value.into()).monospace());
        });
    });
}

fn route_pill(ui: &mut Ui, route: Route) {
    let text = route.as_str();
    let color = route_color(text);
    egui::Frame::default()
        .fill(Color32::from_rgba_unmultiplied(
            color.r(),
            color.g(),
            color.b(),
            35,
        ))
        .stroke(Stroke::new(1.0, color))
        .corner_radius(CornerRadius::same(24))
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(text).monospace().strong().color(color));
        });
}

fn route_label(ui: &mut Ui, route: &str) {
    ui.label(
        RichText::new(route)
            .monospace()
            .strong()
            .color(route_color(route)),
    );
}

fn route_color(route: &str) -> Color32 {
    match route {
        "DIRECT" => good(),
        "REUSE" => accent(),
        "PATCH" => Color32::from_rgb(129, 140, 248),
        "IMPLEMENT" => Color32::from_rgb(96, 165, 250),
        "PARTIAL" => warn(),
        "DELEGATE" => Color32::from_rgb(244, 114, 182),
        "ASK" => Color32::from_rgb(251, 146, 60),
        r if r.starts_with("ESCALATE") => bad(),
        _ => text(),
    }
}

fn status_text(status: &str) -> RichText {
    let color = match status {
        "done" => good(),
        "failed" => bad(),
        "blocked" | "escalated" => warn(),
        _ => muted(),
    };
    RichText::new(status).monospace().color(color)
}

fn constraints_from_text(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn yes_no(v: bool) -> String {
    if v {
        "yes".to_string()
    } else {
        "no".to_string()
    }
}

fn usd(v: f64) -> String {
    if !v.is_finite() {
        "n/a".to_string()
    } else if v > 0.0 && v < 0.01 {
        format!("${v:.6}")
    } else {
        format!("${v:.4}")
    }
}

fn pct(v: f64) -> String {
    format!("{:.1}%", v * 100.0)
}

fn ms(v: f64) -> String {
    format!("{:.0}ms", v)
}

fn wrap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn error_box(ui: &mut Ui, err: &str) {
    egui::Frame::default()
        .fill(Color32::from_rgba_unmultiplied(248, 113, 113, 28))
        .stroke(Stroke::new(1.0, bad()))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.label(RichText::new(err).color(bad()));
        });
}

fn bg() -> Color32 {
    Color32::from_rgb(11, 14, 20)
}

fn panel_fill() -> Color32 {
    Color32::from_rgb(21, 26, 38)
}

fn panel_dark() -> Color32 {
    Color32::from_rgb(17, 21, 31)
}

fn text() -> Color32 {
    Color32::from_rgb(214, 219, 231)
}

fn muted() -> Color32 {
    Color32::from_rgb(124, 135, 160)
}

fn accent() -> Color32 {
    Color32::from_rgb(94, 234, 212)
}

fn good() -> Color32 {
    Color32::from_rgb(52, 211, 153)
}

fn warn() -> Color32 {
    Color32::from_rgb(251, 191, 36)
}

fn bad() -> Color32 {
    Color32::from_rgb(248, 113, 113)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraints_parser_ignores_empty_lines() {
        assert_eq!(
            constraints_from_text(" one\n\n two \n "),
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn usd_formats_infinite_as_not_available() {
        assert_eq!(usd(f64::INFINITY), "n/a");
    }

    #[test]
    fn eval_dataset_parser_accepts_yaml_and_json_aliases() {
        let yaml = parse_eval_items(
            r#"
- goal: " fix typo "
  constraints:
    - " no API break "
    - ""
  expected: "direct"
"#,
        )
        .unwrap();
        assert_eq!(yaml[0].task, "fix typo");
        assert_eq!(yaml[0].constraints, vec!["no API break".to_string()]);
        assert_eq!(yaml[0].expected_route, "DIRECT");

        let json =
            parse_eval_items(r#"[{"prompt":"maybe unclear request","expected_route":"ask"}]"#)
                .unwrap();
        assert_eq!(json[0].task, "maybe unclear request");
        assert_eq!(json[0].expected_route, "ASK");
    }

    #[test]
    fn batch_parser_strips_common_list_markers() {
        assert_eq!(
            parse_batch_tasks("1. fix typo\n- implement cache\n* maybe unclear\n").unwrap(),
            vec![
                "fix typo".to_string(),
                "implement cache".to_string(),
                "maybe unclear".to_string()
            ]
        );
    }

    #[test]
    fn apgr_matches_web_eval_formula() {
        assert_eq!(apgr(0.5, 0.5), 0.0);
        assert!((apgr(0.75, 0.5) - 50.0).abs() < f64::EPSILON);
        assert_eq!(apgr(0.9, 1.0), 100.0);
    }

    #[test]
    fn native_eval_runs_without_web_control_plane() {
        use std::path::Path;
        use std::sync::RwLock;

        use crate::config::Config;
        use crate::engine::Engine;
        use crate::pricing::{DriftWatchdog, Tracker, Ucb1Router};
        use crate::recorder::Recorder;
        use crate::store::Store;

        let mut cfg = Config::default();
        cfg.providers.get_mut("mock").unwrap().disabled = false;
        let arms: Vec<String> = cfg.providers.keys().cloned().collect();
        let engine = Engine {
            cfg,
            store: Store::open(Some(Path::new(":memory:"))).unwrap(),
            recorder: Recorder::new(Some(Path::new(&format!(
                "{}/tokenos-native-eval-test-{}",
                std::env::temp_dir().display(),
                std::process::id()
            ))))
            .unwrap(),
            tracker: Tracker::new(),
            bandit: Ucb1Router::new(&arms),
            drift: DriftWatchdog::new(),
            indexer: None,
            dry_run: true,
            adapters: RwLock::new(HashMap::new()),
        };
        let items = parse_eval_items(SAMPLE_EVAL_DATASET).unwrap();
        let report = evaluate_items(&engine, &items, true);
        let planned = route_batch(
            &engine,
            &parse_batch_tasks("fix typo\nmaybe unclear").unwrap(),
            &[],
        );
        assert_eq!(report.total, items.len());
        assert_eq!(report.sweep.as_ref().unwrap().len(), 11);
        assert!(report.total_strong_cost > 0.0);
        assert!(report.total_router_cost >= 0.0);
        assert_eq!(planned.len(), 2);
        assert!(planned.iter().all(|row| !row.task.is_empty()));
    }

    #[test]
    fn native_app_source_does_not_reference_web_control_plane() {
        let source = include_str!("nativeapp.rs");
        let old_ready_hook = ["serve", "_with", "_ready"].concat();
        let loopback_literal = ["127", ".0", ".0", ".1"].concat();
        let web_module = ["web", "ui", "::"].concat();
        assert!(!source.contains(&old_ready_hook));
        assert!(!source.contains(&loopback_literal));
        assert!(!source.contains(&web_module));
    }
}
