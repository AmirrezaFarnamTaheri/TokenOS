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
| `500` | Internal error (storage, recorder) |
| `504` | `/api/run` exceeded the server-side execution timeout |

---

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

## GET `/api/stats/bandit`

Live UCB1 bandit standings for the current process (evolution S19). Arms are
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

`504` if the run exceeds the server-side timeout.

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
curl -s $B/api/stats/bandit -H "Authorization: Bearer $TOK" | jq
curl -s $B/api/stats/drift -H "Authorization: Bearer $TOK" | jq
curl -s $B/api/route -H "Authorization: Bearer $TOK" \
     -H 'Content-Type: application/json' \
     -d '{"task":"fix typo in README"}' | jq
curl -s $B/api/run -H "Authorization: Bearer $TOK" \
     -H 'Content-Type: application/json' \
     -d '{"task":"say hello","constraints":[]}' | jq
```
