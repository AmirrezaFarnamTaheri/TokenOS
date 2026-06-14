//! Native desktop application for TokenOS (feature `native`).
//!
//! `tokenos app` is a real native desktop surface built with egui/eframe. It
//! talks directly to the Rust engine and SQLite store; it does not start the
//! Axum web dashboard, does not bind a loopback port, and does not open a
//! browser.

use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use eframe::egui::{
    self, Align, Color32, Context, CornerRadius, FontFamily, FontId, Grid, Layout, RichText,
    ScrollArea, Sense, Stroke, StrokeKind, TextEdit, Ui, Vec2, ViewportBuilder,
};
use eframe::{App, Frame, NativeOptions};
use tokio::runtime::Runtime;

use crate::engine::{Engine, RunResult};
use crate::kernel::{Decision, Route, State};
use crate::store::{
    AttemptStats, DailySpend, Execution, ProviderStats, RouteStats, StoreHealth, Summary,
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const INITIAL_TASK: &str = "Fix the typo in the README header";

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
    Tasks,
    Executions,
    Config,
}

impl View {
    fn label(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Console => "Run Console",
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
    history: Vec<DailySpend>,
    tasks: Vec<State>,
    executions: Vec<Execution>,
    health: Option<StoreHealth>,
    solution_cache: Option<(i64, i64, i64)>,
    error: Option<String>,
}

struct RunMessage {
    task: String,
    result: Result<RunResult, String>,
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
    status: String,
}

impl TokenOsNativeApp {
    fn new(engine: Arc<Engine>, rt: Runtime) -> Self {
        let (run_tx, run_rx) = mpsc::channel();
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
                            ui.label(RichText::new(&task.task_id).monospace().small());
                            ui.label(status_text(task.status.as_str()));
                            ui.label(if task.blocked { "yes" } else { "no" });
                            ui.label(wrap(&task.goal, 72));
                            ui.label(RichText::new(wrap(&task.next_action, 56)).color(muted()));
                            ui.end_row();
                        }
                    });
            });
        });
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
        history: engine.store.get_daily_spend()?,
        tasks: engine.store.list_tasks(100)?,
        executions: engine.store.list_executions(200)?,
        health: Some(engine.store.health_snapshot()?),
        solution_cache: Some(engine.store.solution_cache_stats()?),
        error: None,
    })
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
