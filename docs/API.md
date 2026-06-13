# TokenOS HTTP API Reference

The web control panel (`tokenos serve`) exposes a JSON API under `/api/*`.
All endpoints are served by lock-free axum handlers sharing one
`Arc<Engine>` — long-running executions never block telemetry reads.

## Base URL and authentication

```text
http://127.0.0.1:8080            # default bind
```

If an auth token is configured (`--auth-token` or `$TOKENOS_AUTH_TOKEN`),
**every** `/api/*` request must carry it:

```text
Authorization: Bearer <token>
```

- Token comparison is **constant-time** (no timing side channel).
- Static assets (`/`, `/app.js`, `/style.css`) bypass auth so the login-less
  dashboard shell can load; the data it fetches still requires the token.
- Binding a non-loopback host is refused unless a token is set.

Errors are uniform:

```json
{ "error": "human-readable message" }
```

| Status | Meaning |
|---|---|
| `400` | Malformed request body |
| `401` | Missing/invalid bearer token |
| `429` | `/api/run` concurrency limit reached |
| `500` | Internal error (storage, recorder) |
| `504` | `/api/run` exceeded the server-side execution timeout |

---

## GET `/api/meta`

Runtime metadata used by the dashboard header and client-side capacity hints.

```json
{
  "version": "2.0.0",
  "dry_run": true,
  "providers_total": 4,
  "providers_enabled": 1,
  "max_concurrent_runs": 4
}
```

## GET `/api/health`

Local diagnostic snapshot. This endpoint performs no provider calls; it reads
config-derived mode/provider metadata plus SQLite integrity and table counts.

```json
{
  "version": "2.0.0",
  "dry_run": true,
  "traces_enabled": true,
  "providers_total": 4,
  "providers_enabled": 1,
  "workspace_index_enabled": false,
  "store": {
    "quick_check": "ok",
    "tasks": 12,
    "executions": 34,
    "execution_attempts": 40,
    "failure_memory": 0,
    "loop_history": 0,
    "traces": 120,
    "solution_cache": 3,
    "solution_cache_hits": 7,
    "api_request_stats": 9,
    "api_token_usage": 4,
    "drift_ratios": 2
  }
}
```

## GET `/api/summary`

Headline KPIs.

```json
{
  "tasks": 12,
  "executions": 34,
  "overall_success_pct": 0.91,
  "total_tokens": 45678,
  "total_cost_usd": 0.0123,
  "avg_latency_ms": 842.5,
  "cost_per_success": 0.0004
}
```

## GET `/api/stats/routes`

Per-route effectiveness, one object per route observed:

```json
[
  {
    "route": "IMPLEMENT",
    "runs": 20,
    "success_rate": 0.95,
    "avg_tokens_in": 1200,
    "avg_tokens_out": 450,
    "avg_latency_ms": 1100.0,
    "cost_per_success": 0.0006
  }
]
```

## GET `/api/stats/providers`

Per-provider health:

```json
[
  {
    "provider": "anthropic",
    "runs": 18,
    "success_rate": 0.94,
    "avg_latency_ms": 980.0,
    "total_tokens": 30000,
    "total_cost_usd": 0.011
  }
]
```

## GET `/api/stats/attempts`

Per-provider/per-route attempt health. This is the aggregate view of the
attempt ledger, so failed failover legs and verification failures are counted
instead of hidden behind the final execution row.

```json
[
  {
    "provider": "anthropic",
    "route": "IMPLEMENT",
    "attempts": 21,
    "success_rate": 0.9,
    "avg_latency_ms": 1040.0,
    "total_tokens": 35490,
    "total_cost_usd": 0.112
  }
]
```

## GET `/api/stats/api`

Durable HTTP control-plane aggregates. Request bodies, authorization headers,
query strings, and per-request rows are not stored. High-cardinality paths are
normalized, for example `/api/traces/:task_id`.

```json
[
  {
    "method": "GET",
    "path": "/api/summary",
    "status": 200,
    "count": 42,
    "avg_latency_ms": 0.8,
    "max_latency_ms": 5.1,
    "last_seen_at": "2026-06-13T12:00:00Z"
  }
]
```

## GET `/api/stats/bandit`

Live UCB1 bandit standings for the current process. Arms are
ranked by score; an unexplored arm's score is the string `"unexplored"`
(UCB1 assigns it +∞, which JSON cannot represent).

```json
{
  "exploration": 1.4142135623730951,
  "arms": [
    {
      "provider": "mock",
      "ucb1_score": "unexplored",
      "pulls": 0,
      "mean_reward": 0.0,
      "mean_latency_ms": 0.0
    },
    {
      "provider": "anthropic",
      "ucb1_score": 1.732,
      "pulls": 9,
      "mean_reward": 0.89,
      "mean_latency_ms": 950.0
    }
  ]
}
```

## GET `/api/stats/drift`

Estimator calibration watchdog plus solution-cache counters. `ratio_ewma`
is the EWMA of actual÷estimated input tokens (1.0 = perfectly calibrated);
`drifting` becomes true after ≥ 5 samples outside the trusted band
[0.75, 1.30].

```json
{
  "providers": [
    { "provider": "anthropic", "ratio_ewma": 1.04, "samples": 12, "drifting": false }
  ],
  "solution_cache": { "entries": 3, "zero_token_hits": 7 }
}
```

## GET `/api/stats/history`

Daily spend telemetry for the last 30 active days (excluding mock runs):

```json
[
  {
    "day": "2026-06-13",
    "cost_usd": 0.0125,
    "successes": 5,
    "runs": 6
  }
]
```

## GET `/api/executions`

Most recent 200 telemetry rows (newest first):

```json
[
  {
    "id": 34,
    "task_id": "t-9f2c",
    "route": "IMPLEMENT",
    "provider": "anthropic",
    "model": "claude-sonnet-4-20250514",
    "tokens_in": 1180,
    "tokens_out": 510,
    "latency_ms": 1042,
    "retries": 0,
    "est_cost_usd": 0.0112,
    "success": true,
    "created_at": "2026-06-11T10:00:00Z"
  }
]
```

## GET `/api/attempts`

Most recent 300 provider attempts (newest first). Unlike `/api/executions`,
this includes failed failover legs, verification failures, loop-escalation
attempts, and the final successful provider leg when one exists.

```json
[
  {
    "id": 52,
    "task_id": "t-9f2c",
    "provider": "anthropic",
    "model": "claude-sonnet-4-20250514",
    "route": "IMPLEMENT",
    "tokens_in": 1180,
    "tokens_out": 510,
    "latency_ms": 1042,
    "success": true,
    "error_message": "",
    "cost_usd": 0.0112,
    "created_at": "2026-06-13T12:00:00Z"
  }
]
```

## GET `/api/tasks`

Most recent 100 compressed task states:

```json
[
  {
    "task_id": "t-9f2c",
    "goal": "implement rate limiting middleware",
    "status": "done",
    "blocked": false,
    "updated_at": "2026-06-11T10:00:02Z"
  }
]
```

## GET `/api/traces/:task_id`

Flight-recorder timeline for one task. Event kinds include `decision`,
`prompt`, `response`, `rescue`, `verify`, and `error`.

```json
[
  { "ts": "2026-06-11T10:00:00Z", "kind": "decision", "summary": "IMPLEMENT: default productive path" },
  { "ts": "2026-06-11T10:00:00Z", "kind": "prompt",   "summary": "blob sha256:ab12…" },
  { "ts": "2026-06-11T10:00:01Z", "kind": "response", "summary": "blob sha256:cd34…" }
]
```

## GET `/api/config`

The effective configuration, **read-only**. API keys never appear here —
only the names of the environment variables that hold them.

## POST `/api/route`

Free, deterministic routing preview. Pure CPU work executed on a blocking
thread (the async reactor stays free). No provider is called.

Request:

```json
{ "task": "fix the auth timeout bug" }
```

Response:

```json
{
  "decision": {
    "route": "PATCH",
    "reason": "localized change, no repeated failure",
    "signals": { "confidence": 0.8, "is_trivial": false, "...": "..." }
  },
  "provider_chain": ["anthropic", "openai"],
  "prompt_tokens": 412,
  "context_tokens": 120
}
```

`400` if the body is not `{"task": "..."}` with a non-empty task.

## POST `/api/run`

Execute a task through the full pipeline.

Request:

```json
{
  "task": "implement rate limiting middleware",
  "constraints": ["no public API changes"]
}
```

Response (success):

```json
{
  "result": {
    "task_id": "t-9f2c",
    "route": "IMPLEMENT",
    "success": true,
    "provider": "anthropic",
    "model": "claude-sonnet-4-20250514",
    "tokens_in": 1180,
    "tokens_out": 510,
    "latency_ms": 1042,
    "retries": 0,
    "cost_usd": 0.0112,
    "output": "..."
  }
}
```

Response (engine-level failure — still HTTP 200):

```json
{ "result": null, "error": "all providers failed: ..." }
```

`429` if all in-process execution slots are occupied. The request is rejected
before entering provider code. `504` if the run exceeds the server-side timeout.

## GET `/metrics`

Exposes plaintext Prometheus exposition format metrics (unauthenticated, to allow scraping by Prometheus monitoring agents).

```text
# HELP tokenos_tasks_total Total tasks submitted
# TYPE tokenos_tasks_total counter
tokenos_tasks_total 12

# HELP tokenos_cost_usd_total Total cost in USD
# TYPE tokenos_cost_usd_total counter
tokenos_cost_usd_total 0.0123
...
```

## POST `/v1/chat/completions`

An OpenAI-compatible chat completions proxy endpoint (requires `run` or `admin` bearer authentication when configured).

Request:

```json
{
  "messages": [
    { "role": "user", "content": "say hello" }
  ]
}
```

Response:

```json
{
  "id": "chatcmpl-t-9f2c",
  "object": "chat.completion",
  "created": 1718280000,
  "model": "claude-sonnet-4-20250514",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "Hello! How can I help you today?"
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 10,
    "completion_tokens": 8,
    "total_tokens": 18
  }
}
```

---

## Static assets

| Path | Content |
|---|---|
| `GET /` | Dashboard HTML (embedded at compile time via `include_str!`) |
| `GET /app.js` | Dashboard JavaScript |
| `GET /style.css` | Dashboard stylesheet |

Assets are compiled into the binary — the server has no filesystem
dependency at runtime.

## curl cookbook

```sh
TOK=s3cret
B="http://127.0.0.1:8080"

curl -s $B/api/summary -H "Authorization: Bearer $TOK" | jq
curl -s $B/api/stats/api -H "Authorization: Bearer $TOK" | jq
curl -s $B/api/stats/bandit -H "Authorization: Bearer $TOK" | jq
curl -s $B/api/stats/drift -H "Authorization: Bearer $TOK" | jq
curl -s $B/api/route -H "Authorization: Bearer $TOK" \
     -H 'Content-Type: application/json' \
     -d '{"task":"fix typo in README"}' | jq
curl -s $B/api/run -H "Authorization: Bearer $TOK" \
     -H 'Content-Type: application/json' \
     -d '{"task":"say hello","constraints":[]}' | jq
```
