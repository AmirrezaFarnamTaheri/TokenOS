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
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
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

pub fn router(engine: Arc<Engine>) -> Router {
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
        .route("/api/summary", get(handle_summary))
        .route("/api/stats/routes", get(handle_route_stats))
        .route("/api/stats/providers", get(handle_provider_stats))
        .route("/api/executions", get(handle_executions))
        .route("/api/tasks", get(handle_tasks))
        .route("/api/config", get(handle_config))
        .route("/api/traces/:task_id", get(handle_traces))
        .route("/api/route", post(handle_route_preview))
        .route("/api/run", post(handle_run))
        .with_state(engine)
}

/// Serves the dashboard on addr until the process exits.
pub async fn serve(engine: Arc<Engine>, host: &str, port: u16) -> anyhow::Result<()> {
    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("TokenOS dashboard listening on http://{}", addr);
    axum::serve(listener, router(engine)).await?;
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
        Arc::new(Engine {
            cfg: Config::default(),
            store: Store::open(Some(std::path::Path::new(":memory:"))).unwrap(),
            recorder: Recorder::new(Some(std::path::Path::new(&format!(
                "{}/tokenos-web-test-{}",
                std::env::temp_dir().display(),
                std::process::id()
            ))))
            .unwrap(),
            tracker: Tracker::new(),
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
