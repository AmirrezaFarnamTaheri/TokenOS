//! Local observability dashboard + REST control plane.
//!
//! Concurrency (audit finding 12.1 remediation): unlike the original Go
//! server — which held one coarse `sync.Mutex` across every handler,
//! including 5-minute /api/run network calls — this implementation shares an
//! `Arc<Engine>` across handlers with NO global lock. Telemetry reads hit
//! SQLite through the store's fine-grained connection mutex (microseconds),
//! and long-running /api/run executions never block dashboard reads.

use crate::engine::Engine;
use crate::payload;
use crate::tokenizer;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLE_CSS: &str = include_str!("../static/style.css");

/// 5-minute ceiling for interactive /api/run executions.
const RUN_TIMEOUT: Duration = Duration::from_secs(300);

/// Hard cap on inbound request bodies (finding 12.1: resource-exhaustion
/// hardening — a task description never legitimately approaches this).
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Bearer-token gate for /api/* (finding 12.1, CWE-306). `None` means the
/// server is loopback-only and auth is not enforced.
#[derive(Clone)]
struct AuthToken(Option<Arc<String>>);

/// Constant-time byte comparison so token checks don't leak length-prefix
/// timing information.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn require_bearer(
    State(auth): State<AuthToken>,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = auth.0.as_ref() else {
        return next.run(req).await; // loopback-only mode: no token configured
    };
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| ct_eq(t.as_bytes(), expected.as_bytes()))
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid bearer token" })),
        )
            .into_response()
    }
}

pub fn router(engine: Arc<Engine>) -> Router {
    router_with_auth(engine, None)
}

/// Builds the full app router. When `auth_token` is Some, every /api/* route
/// requires `Authorization: Bearer <token>` (finding 12.1).
pub fn router_with_auth(engine: Arc<Engine>, auth_token: Option<String>) -> Router {
    let auth = AuthToken(auth_token.map(Arc::new));
    let api = Router::new()
        .route("/api/meta", get(handle_meta))
        .route("/api/summary", get(handle_summary))
        .route("/api/stats/routes", get(handle_route_stats))
        .route("/api/stats/providers", get(handle_provider_stats))
        .route("/api/stats/bandit", get(handle_bandit_stats))
        .route("/api/stats/drift", get(handle_drift_stats))
        .route("/api/executions", get(handle_executions))
        .route("/api/tasks", get(handle_tasks))
        .route("/api/config", get(handle_config))
        .route("/api/traces/:task_id", get(handle_traces))
        .route("/api/route", post(handle_route_preview))
        .route("/api/run", post(handle_run))
        .layer(middleware::from_fn_with_state(auth, require_bearer))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(engine);
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route(
            "/app.js",
            get(|| async { asset(APP_JS, "application/javascript; charset=utf-8") }),
        )
        .route(
            "/style.css",
            get(|| async { asset(STYLE_CSS, "text/css; charset=utf-8") }),
        )
        .merge(api)
}

/// Serves the dashboard on addr until the process exits.
pub async fn serve(
    engine: Arc<Engine>,
    host: &str,
    port: u16,
    auth_token: Option<String>,
) -> anyhow::Result<()> {
    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("TokenOS dashboard listening on http://{}", addr);
    axum::serve(listener, router_with_auth(engine, auth_token)).await?;
    Ok(())
}

/// Serves the dashboard and reports the actual bound address through
/// `ready` before accepting traffic. Used by the native desktop shell,
/// which binds port 0 (ephemeral) and must learn the real port to point
/// the webview at. Loopback-only by convention of its single caller.
pub async fn serve_with_ready(
    engine: Arc<Engine>,
    host: &str,
    port: u16,
    auth_token: Option<String>,
    ready: tokio::sync::oneshot::Sender<std::net::SocketAddr>,
) -> anyhow::Result<()> {
    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let local = listener.local_addr()?;
    // A dropped receiver means the shell aborted — serving is pointless.
    if ready.send(local).is_err() {
        return Ok(());
    }
    axum::serve(listener, router_with_auth(engine, auth_token)).await?;
    Ok(())
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

fn err(status: StatusCode, msg: String) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn ok_json<T: serde::Serialize>(v: T) -> Response {
    Json(v).into_response()
}

/// Instance metadata for the dashboard header: kernel version, dry-run flag,
/// and provider fleet size. Lets the UI tell newcomers at a glance whether
/// they are exercising the offline mock provider or spending real money.
async fn handle_meta(State(eng): State<Arc<Engine>>) -> Response {
    let enabled = eng.cfg.providers.values().filter(|p| !p.disabled).count();
    ok_json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "dry_run": eng.dry_run,
        "providers_total": eng.cfg.providers.len(),
        "providers_enabled": enabled,
    }))
}

async fn handle_summary(State(eng): State<Arc<Engine>>) -> Response {
    match eng.store.get_summary() {
        Ok(s) => ok_json(s),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn handle_route_stats(State(eng): State<Arc<Engine>>) -> Response {
    match eng.store.stats_by_route() {
        Ok(s) => ok_json(s),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn handle_provider_stats(State(eng): State<Arc<Engine>>) -> Response {
    match eng.store.stats_by_provider() {
        Ok(s) => ok_json(s),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Live UCB1 bandit standings (evolution S19): per-arm pulls, mean reward,
/// mean latency and current UCB1 score. Lock-free read path.
async fn handle_bandit_stats(State(eng): State<Arc<Engine>>) -> Response {
    let arms: Vec<serde_json::Value> = eng
        .bandit
        .ranked()
        .into_iter()
        .map(|(provider, score)| {
            let (pulls, mean_reward, mean_latency_ms) = eng.bandit.arm_stats(&provider);
            json!({
                "provider": provider,
                "ucb1_score": if score.is_finite() { json!(score) } else { json!("unexplored") },
                "pulls": pulls,
                "mean_reward": mean_reward,
                "mean_latency_ms": mean_latency_ms,
            })
        })
        .collect();
    ok_json(json!({ "exploration": eng.bandit.exploration, "arms": arms }))
}

/// Estimator drift watchdog (evolution S30): per-provider EWMA of
/// actual/estimated input-token ratios plus the solution-cache counters
/// (evolution S25).
async fn handle_drift_stats(State(eng): State<Arc<Engine>>) -> Response {
    let (cache_entries, cache_hits) = eng.store.solution_cache_stats().unwrap_or((0, 0));
    ok_json(json!({
        "providers": eng.drift.all(),
        "solution_cache": { "entries": cache_entries, "zero_token_hits": cache_hits },
    }))
}

async fn handle_executions(State(eng): State<Arc<Engine>>) -> Response {
    match eng.store.list_executions(200) {
        Ok(s) => ok_json(s),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn handle_tasks(State(eng): State<Arc<Engine>>) -> Response {
    match eng.store.list_tasks(100) {
        Ok(s) => ok_json(s),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Config is exposed read-only; API keys live in env vars, never here.
async fn handle_config(State(eng): State<Arc<Engine>>) -> Response {
    ok_json(&eng.cfg)
}

async fn handle_traces(State(eng): State<Arc<Engine>>, Path(task_id): Path<String>) -> Response {
    match eng.recorder.events(&task_id) {
        Ok(evs) => ok_json(evs),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct TaskRequest {
    #[serde(default)]
    task: String,
    #[serde(default)]
    constraints: Vec<String>,
}

async fn handle_route_preview(
    State(eng): State<Arc<Engine>>,
    body: Result<Json<TaskRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let req = match body {
        Ok(Json(r)) if !r.task.is_empty() => r,
        _ => return err(StatusCode::BAD_REQUEST, "body must be {\"task\": \"...\"}".into()),
    };
    // Routing is pure CPU work — run it on a blocking thread so the async
    // reactor stays free (no lock involved at all).
    let result = tokio::task::spawn_blocking(move || {
        let (dec, ctx_block) = eng.route_only(&req.task);
        let chain = eng.cfg.provider_chain(dec.route.as_str());
        json!({
            "decision": dec,
            "provider_chain": chain,
            "context_tokens": tokenizer::estimate(&ctx_block),
            "prompt_tokens": tokenizer::estimate(payload::KERNEL_CONTRACT)
                + tokenizer::estimate(&req.task)
                + tokenizer::estimate(&ctx_block),
        })
    })
    .await;
    match result {
        Ok(v) => ok_json(v),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn handle_run(
    State(eng): State<Arc<Engine>>,
    body: Result<Json<TaskRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let req = match body {
        Ok(Json(r)) if !r.task.is_empty() => r,
        _ => return err(StatusCode::BAD_REQUEST, "body must be {\"task\": \"...\"}".into()),
    };
    // No global lock: concurrent runs are safe (engine state is internally
    // synchronized) and dashboard reads stay responsive during execution.
    let run = tokio::time::timeout(RUN_TIMEOUT, eng.run(&req.task, &req.constraints)).await;
    match run {
        Err(_) => err(StatusCode::GATEWAY_TIMEOUT, "execution timed out".into()),
        Ok(Ok(res)) => ok_json(json!({ "result": res })),
        Ok(Err(e)) => ok_json(json!({ "result": null, "error": e.to_string() })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::pricing::Tracker;
    use crate::recorder::Recorder;
    use crate::store::Store;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for `oneshot`

    fn test_engine() -> Arc<Engine> {
        let cfg = Config::default();
        let arms: Vec<String> = cfg.providers.keys().cloned().collect();
        Arc::new(Engine {
            cfg,
            store: Store::open(Some(std::path::Path::new(":memory:"))).unwrap(),
            recorder: Recorder::new(Some(std::path::Path::new(&format!(
                "{}/tokenos-web-test-{}",
                std::env::temp_dir().display(),
                std::process::id()
            ))))
            .unwrap(),
            tracker: Tracker::new(),
            bandit: crate::pricing::Ucb1Router::new(&arms),
            drift: crate::pricing::DriftWatchdog::new(),
            indexer: None,
            dry_run: true,
            adapters: Default::default(),
        })
    }

    #[tokio::test]
    async fn summary_endpoint_returns_json() {
        let app = router(test_engine());
        let resp = app
            .oneshot(Request::get("/api/summary").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn index_serves_html() {
        let app = router(test_engine());
        let resp = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn route_preview_requires_task() {
        let app = router(test_engine());
        let resp = app
            .oneshot(
                Request::post("/api/route")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn api_rejects_missing_token_when_auth_enabled() {
        let app = router_with_auth(test_engine(), Some("sekrit".into()));
        let resp = app
            .oneshot(Request::get("/api/summary").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_rejects_wrong_token() {
        let app = router_with_auth(test_engine(), Some("sekrit".into()));
        let resp = app
            .oneshot(
                Request::get("/api/summary")
                    .header("authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_accepts_correct_token() {
        let app = router_with_auth(test_engine(), Some("sekrit".into()));
        let resp = app
            .oneshot(
                Request::get("/api/summary")
                    .header("authorization", "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn static_assets_bypass_auth() {
        let app = router_with_auth(test_engine(), Some("sekrit".into()));
        let resp = app
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let app = router(test_engine());
        let huge = format!(r#"{{"task":"{}"}}"#, "x".repeat(MAX_BODY_BYTES + 1024));
        let resp = app
            .oneshot(
                Request::post("/api/route")
                    .header("content-type", "application/json")
                    .body(Body::from(huge))
                    .unwrap(),
            )
            .await
            .unwrap();
        // The limit either rejects at the layer (413) or surfaces through
        // the Json extractor rejection (400) — both refuse the payload.
        assert!(
            resp.status() == StatusCode::PAYLOAD_TOO_LARGE
                || resp.status() == StatusCode::BAD_REQUEST,
            "oversized body must be refused, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn constant_time_eq_basics() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[tokio::test]
    async fn bandit_endpoint_reports_arms() {
        let eng = test_engine();
        eng.bandit.record("mock", true, 42.0);
        let app = router(eng);
        let resp = app
            .oneshot(Request::get("/api/stats/bandit").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arms = v["arms"].as_array().unwrap();
        assert!(!arms.is_empty());
        let mock_arm = arms.iter().find(|a| a["provider"] == "mock").unwrap();
        assert_eq!(mock_arm["pulls"], 1);
        // Unexplored arms must serialize as the "unexplored" sentinel, not
        // as an invalid JSON infinity.
        let unexplored = arms.iter().find(|a| a["provider"] != "mock").unwrap();
        assert_eq!(unexplored["ucb1_score"], "unexplored");
    }

    #[tokio::test]
    async fn run_endpoint_executes_dry_run() {
        let app = router(test_engine());
        let resp = app
            .oneshot(
                Request::post("/api/run")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"task":"rename variable a to b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
