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
use tokio::sync::Semaphore;

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLE_CSS: &str = include_str!("../static/style.css");

/// 5-minute ceiling for interactive /api/run executions.
const RUN_TIMEOUT: Duration = Duration::from_secs(300);

/// Hard per-process backpressure for paid work. Dashboard reads and route
/// preview stay available even when execution slots are saturated.
const MAX_CONCURRENT_RUNS: usize = 4;

/// Hard cap on inbound request bodies (finding 12.1: resource-exhaustion
/// hardening — a task description never legitimately approaches this).
const MAX_BODY_BYTES: usize = 256 * 1024;

#[derive(Clone)]
struct WebState {
    engine: Arc<Engine>,
    run_limiter: Arc<Semaphore>,
    cli_auth_token: Option<Arc<String>>,
}

/// Constant-time byte comparison so token checks don't leak length-prefix
/// timing information.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let max = a.len().max(b.len());
    for i in 0..max {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

async fn require_bearer(State(state): State<WebState>, req: Request, next: Next) -> Response {
    let cli_token = state.cli_auth_token.as_ref();
    let api_tokens = &state.engine.cfg.security.api_tokens;
    if cli_token.is_none() && api_tokens.is_empty() {
        return next.run(req).await; // loopback-only mode: no token configured
    }

    let Some(header_val) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid bearer token" })),
        )
            .into_response();
    };

    // Determine the required scope based on path
    let path = req.uri().path();
    let required_scope = if path == "/api/run" || path == "/api/route" {
        "run"
    } else if path.starts_with("/api/traces/") {
        "admin"
    } else {
        "read"
    };

    // 1. Check CLI token (admin scope, satisfies everything)
    if let Some(expected) = cli_token {
        if ct_eq(header_val.as_bytes(), expected.as_bytes()) {
            return next.run(req).await;
        }
    }

    // 2. Check config API tokens (constant-time check for each)
    let mut authenticated = false;
    for (cfg_token, scopes) in api_tokens {
        if ct_eq(header_val.as_bytes(), cfg_token.as_bytes())
            && scopes.iter().any(|s| s == required_scope || s == "admin")
        {
            authenticated = true;
        }
    }

    if authenticated {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "insufficient permissions or invalid token" })),
        )
            .into_response()
    }
}

async fn add_security_headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static("default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' https://fonts.gstatic.com; img-src 'self' data:; connect-src 'self' ws: wss:; frame-ancestors 'none';"),
    );
    headers.insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    response
}

pub fn router(engine: Arc<Engine>) -> Router {
    router_with_auth(engine, None)
}

pub fn router_with_auth(engine: Arc<Engine>, auth_token: Option<String>) -> Router {
    let state = WebState {
        engine,
        run_limiter: Arc::new(Semaphore::new(MAX_CONCURRENT_RUNS)),
        cli_auth_token: auth_token.map(Arc::new),
    };
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
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state);
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
        .layer(middleware::from_fn(add_security_headers))
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
async fn handle_meta(State(state): State<WebState>) -> Response {
    let eng = state.engine;
    let enabled = eng.cfg.providers.values().filter(|p| !p.disabled).count();
    ok_json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "dry_run": eng.dry_run,
        "providers_total": eng.cfg.providers.len(),
        "providers_enabled": enabled,
        "max_concurrent_runs": MAX_CONCURRENT_RUNS,
    }))
}

async fn handle_summary(State(state): State<WebState>) -> Response {
    let eng = state.engine;
    match eng.store.get_summary() {
        Ok(s) => ok_json(s),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn handle_route_stats(State(state): State<WebState>) -> Response {
    let eng = state.engine;
    match eng.store.stats_by_route() {
        Ok(s) => ok_json(s),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn handle_provider_stats(State(state): State<WebState>) -> Response {
    let eng = state.engine;
    match eng.store.stats_by_provider() {
        Ok(s) => ok_json(s),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Live UCB1 bandit standings (evolution S19): per-arm pulls, mean reward,
/// mean latency and current UCB1 score. Lock-free read path.
async fn handle_bandit_stats(State(state): State<WebState>) -> Response {
    let eng = state.engine;
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
async fn handle_drift_stats(State(state): State<WebState>) -> Response {
    let eng = state.engine;
    let (cache_entries, test_verified, cache_hits) =
        eng.store.solution_cache_stats().unwrap_or((0, 0, 0));
    ok_json(json!({
        "providers": eng.drift.all(),
        "solution_cache": {
            "entries": cache_entries,
            "test_verified": test_verified,
            "static_checked": cache_entries - test_verified,
            "zero_token_hits": cache_hits
        },
    }))
}

async fn handle_executions(State(state): State<WebState>) -> Response {
    let eng = state.engine;
    match eng.store.list_executions(200) {
        Ok(s) => ok_json(s),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn handle_tasks(State(state): State<WebState>) -> Response {
    let eng = state.engine;
    match eng.store.list_tasks(100) {
        Ok(s) => ok_json(s),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Config is exposed read-only; API keys live in env vars, never here.
async fn handle_config(State(state): State<WebState>) -> Response {
    let eng = state.engine;
    ok_json(&eng.cfg)
}

async fn handle_traces(State(state): State<WebState>, Path(task_id): Path<String>) -> Response {
    let eng = state.engine;
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
    State(state): State<WebState>,
    body: Result<Json<TaskRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let req = match body {
        Ok(Json(r)) if !r.task.is_empty() => r,
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "body must be {\"task\": \"...\"}".into(),
            )
        }
    };
    // Routing is pure CPU work — run it on a blocking thread so the async
    // reactor stays free (no lock involved at all).
    let result = tokio::task::spawn_blocking(move || {
        let eng = state.engine;
        let (dec, ctx_block) = eng.route_only_with_constraints(&req.task, &req.constraints);
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
    State(state): State<WebState>,
    body: Result<Json<TaskRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let req = match body {
        Ok(Json(r)) if !r.task.is_empty() => r,
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "body must be {\"task\": \"...\"}".into(),
            )
        }
    };
    let Ok(permit) = state.run_limiter.clone().try_acquire_owned() else {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many concurrent executions; limit is {MAX_CONCURRENT_RUNS}"),
        );
    };
    // No global lock: concurrent runs are safe (engine state is internally
    // synchronized) and dashboard reads stay responsive during execution.
    let run =
        tokio::time::timeout(RUN_TIMEOUT, state.engine.run(&req.task, &req.constraints)).await;
    drop(permit);
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
    use crate::pricing::{DriftWatchdog, Tracker, Ucb1Router};
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
        assert_eq!(
            resp.headers().get("content-security-policy").unwrap(),
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' https://fonts.gstatic.com; img-src 'self' data:; connect-src 'self' ws: wss:; frame-ancestors 'none';"
        );
        assert_eq!(resp.headers().get("x-frame-options").unwrap(), "DENY");
        assert_eq!(
            resp.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
        assert_eq!(
            resp.headers().get("referrer-policy").unwrap(),
            "no-referrer"
        );
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
            .oneshot(
                Request::get("/api/stats/bandit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn api_run_concurrency_limiter_blocks_excess_requests() {
        let engine = test_engine();
        let state = WebState {
            engine,
            run_limiter: Arc::new(Semaphore::new(MAX_CONCURRENT_RUNS)),
            cli_auth_token: None,
        };

        // Acquire 4 permits to fully saturate the semaphore
        let p1 = state.run_limiter.clone().acquire_owned().await.unwrap();
        let p2 = state.run_limiter.clone().acquire_owned().await.unwrap();
        let p3 = state.run_limiter.clone().acquire_owned().await.unwrap();
        let p4 = state.run_limiter.clone().acquire_owned().await.unwrap();

        // 5th request should fail with 429 TOO_MANY_REQUESTS
        let body = Ok(axum::Json(TaskRequest {
            task: "test concurrency limit".to_string(),
            constraints: vec![],
        }));
        let resp = handle_run(axum::extract::State(state.clone()), body).await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

        // Release one permit
        drop(p1);

        // 6th request should now pass and return StatusCode::OK
        let body2 = Ok(axum::Json(TaskRequest {
            task: "test concurrency limit".to_string(),
            constraints: vec![],
        }));
        let resp2 = handle_run(axum::extract::State(state), body2).await;
        assert_eq!(resp2.status(), StatusCode::OK);

        // Suppress unused warning on permits
        drop(p2);
        drop(p3);
        drop(p4);
    }

    #[tokio::test]
    async fn api_scopes_governance() {
        let mut cfg = Config::default();
        cfg.security
            .api_tokens
            .insert("read_token".into(), vec!["read".into()]);
        cfg.security
            .api_tokens
            .insert("run_token".into(), vec!["run".into()]);
        cfg.security
            .api_tokens
            .insert("admin_token".into(), vec!["admin".into()]);
        let arms: Vec<String> = cfg.providers.keys().cloned().collect();

        let engine = Arc::new(Engine {
            cfg,
            store: Store::open(Some(std::path::Path::new(":memory:"))).unwrap(),
            recorder: Recorder::new(Some(std::path::Path::new(&format!(
                "{}/tokenos-web-scope-test-{}",
                std::env::temp_dir().display(),
                std::process::id()
            ))))
            .unwrap(),
            tracker: Tracker::new(),
            bandit: Ucb1Router::new(&arms),
            drift: DriftWatchdog::new(),
            indexer: None,
            dry_run: true,
            adapters: std::sync::RwLock::new(std::collections::HashMap::new()),
        });

        let app = router_with_auth(engine, None);

        // 1. read_token can access read endpoint, but not run
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/summary")
                    .header("authorization", "Bearer read_token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/run")
                    .header("authorization", "Bearer read_token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"task":"rename variable"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 2. run_token can access run endpoint, but not read
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/summary")
                    .header("authorization", "Bearer run_token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/run")
                    .header("authorization", "Bearer run_token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"task":"rename variable"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 3. admin_token can access both read and run
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/summary")
                    .header("authorization", "Bearer admin_token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/run")
                    .header("authorization", "Bearer admin_token")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"task":"rename variable"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
