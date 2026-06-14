# TokenOS CLI Reference

```text
tokenos <COMMAND> [OPTIONS]
```

Build the default CLI with `cargo build --release`. Build the native desktop
surface with `cargo build --release --features native`.

## Global Engine Flags

Most engine-backed commands accept these flags:

| Flag | Default | Meaning |
|---|---|---|
| `--config <path>` | `~/.config/tokenos/config.yaml` | Config file, also `$TOKENOS_CONFIG` |
| `--db <path>` | `~/.local/share/tokenos/tokenos.db` | State database, also `$TOKENOS_DB` |
| `--traces <path>` | `~/.local/state/tokenos/traces` | Flight-recorder directory, also `$TOKENOS_TRACES` |
| `--workspace <path>` | none | Workspace to index for surgical context |
| `--dry-run` | off | Force the offline mock adapter |

## `tokenos run` - Execute A Task

```sh
tokenos run "fix the auth timeout bug" --constraints "no public API changes; keep tests green"
tokenos run "produce a json summary of the config" --dry-run --json
```

| Option | Meaning |
|---|---|
| `<task>...` | Task description |
| `--constraints <s>` | Semicolon-separated constraint list |
| `--json` | Emit the full result object as JSON |
| `[engine flags]` | Shared flags above |

Runs deterministic routing, surgical context, payload construction, secret
masking, provider failover, verification, recording, and telemetry. Exit code
is non-zero on failure.

## `tokenos route` - Free Routing Preview

```sh
tokenos route "rename parse_config to load_config"
```

Prints route, reason, confidence, token estimate, and provider chain without
calling a provider.

## `tokenos index` - Build The Symbol Index

```sh
tokenos index .
tokenos index . --out idx.db
tokenos index . --query "auth token"
```

Parses Go, Python, JS/TS, Rust, Java, C, and Ruby sources into structural
symbols for minimum-viable-context selection.

## `tokenos providers` - Provider And Model Verdicts

```sh
tokenos providers
tokenos providers --config ./custom.yaml
```

Lists provider enablement, adapter, model, and two-tier include/exclude filter
results.

## `tokenos telemetry` - Cost Effectiveness

```sh
tokenos telemetry
```

Prints cost per successful task, per-route effectiveness, provider health,
provider-attempt aggregates, UCB1 bandit standings, solution-cache counters,
and estimator drift.

## `tokenos doctor` - Local Health Diagnostics

```sh
tokenos doctor
tokenos doctor --json
```

Reads local configuration and SQLite health without provider calls. It reports
database integrity, table counts, provider enablement, trace policy, cache
counters, and workspace-index status.

## `tokenos attempts` - Provider Attempt Ledger

```sh
tokenos attempts --limit 50
```

Lists provider legs, including failed failover attempts, verification failures,
loop-escalation attempts, final successful legs, latency, token counts, cost,
and error reason.

## `tokenos tasks` - Persisted Task States

```sh
tokenos tasks --limit 50
```

Lists compressed task states: ID, goal, status, blockers, and update time.

## `tokenos trace` - Flight-Recorder Replay

```sh
tokenos trace <task-id>
tokenos trace <task-id> --blobs
```

Replays decision, prompt, response, rescue, verify, and error events for one
task. `--blobs` prints full recorded payload blobs.

## `tokenos config` - Show Or Initialize Config

```sh
tokenos config
tokenos config init
tokenos config init --config ./tokenos.yaml
```

`init` refuses to overwrite an existing file.

## `tokenos eval` - Routing Accuracy Evaluation

```sh
tokenos eval --dataset ./dataset.json
tokenos eval --dataset ./dataset.yaml --sweep
```

Runs deterministic route decisions against a labeled YAML/JSON corpus and
reports accuracy, weak baseline, APGR, estimated cost, savings, and mismatch
details.

## `tokenos app` - Native Desktop UI

```sh
cargo build --release --features native
tokenos app --dry-run
```

Starts the egui/eframe desktop application. The native app calls the Rust
engine directly for route previews and executions and reads SQLite telemetry
directly for dashboard, task, execution, provider, attempt, spend, health, and
configuration views. It does not start an HTTP server, bind a loopback port,
open a browser, or embed a webview.

Native panels include:

- Dashboard: cost per successful task, route/provider health, spend, store
  health, UCB1 bandit standings, estimator drift, cache counters, and attempts.
- Action Center: prioritized operator actions generated from readiness, spend,
  credentials, provider health, drift, cache, and recent attempt signals.
- Run Console: route preview, routing signals, provider chain, provider cost
  forecast, budget-sentinel status, execution controls, and final output.
- Route Planner: backlog preview with route mix, provider demand, token
  estimates, forecast provider cost, budget blocks, and baseline savings.
- Policy Lab: zero-token simulation of routing policy changes.
- Calibration: route dataset evaluation with APGR, savings, mismatches, and
  ASK-threshold sweep.
- Operations: circuit breakers, request aggregate history retained in older
  databases, estimator drift, GenAI rollups, and raw attempts.
- Tasks / Executions: persisted task state, traces, final execution rows, and
  provider attempts.

## Retired Commands

`tokenos serve` has been removed with the browser dashboard and HTTP API. Use
`tokenos app`, CLI commands, or library embedding.

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Execution failed, invalid arguments, refused operation, or local health failure |

## Common Workflows

```sh
# First-time setup
tokenos config init
export ANTHROPIC_API_KEY=sk-ant-...

# Preview before paying
tokenos route "implement rate limiting middleware"

# Execute with workspace context
tokenos run "implement rate limiting middleware" --workspace ./myproject

# Inspect what happened
tokenos tasks
tokenos trace <task-id> --blobs
tokenos attempts
tokenos telemetry

# Native offline smoke test
cargo build --release --features native
tokenos app --dry-run
```
