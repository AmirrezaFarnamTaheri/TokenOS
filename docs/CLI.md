# TokenOS CLI Reference

```
tokenos <COMMAND> [OPTIONS]
```

Build the binary with `cargo build --release`; it lands at
`target/release/tokenos`.

## Global engine flags

Most commands accept these flags (shown as `[engine flags]` below):

| Flag | Default | Meaning |
|---|---|---|
| `--config <path>` | `~/.config/tokenos/config.yaml` | Config file (also `$TOKENOS_CONFIG`) |
| `--db <path>` | `~/.local/share/tokenos/tokenos.db` | State database (also `$TOKENOS_DB`) |
| `--traces <path>` | `~/.local/state/tokenos/traces` | Flight-recorder directory (also `$TOKENOS_TRACES`) |
| `--workspace <path>` | — | Workspace to index for surgical context |
| `--dry-run` | off | Force the offline mock adapter — zero live tokens |

---

## `tokenos run` — execute a task

```sh
tokenos run "fix the auth timeout bug" --constraints "no public API changes; keep tests green"
tokenos run "produce a json summary of the config" --dry-run --json
```

| Option | Meaning |
|---|---|
| `<task>...` | Task description (free text; multiple words allowed unquoted) |
| `--constraints <s>` | Semicolon-separated constraint list |
| `--json` | Emit the full result object as JSON (for scripting) |
| `[engine flags]` | See above |

Runs the full pipeline: deterministic routing → surgical context → payload →
secret masking → shadow-priced failover → verification → recording. Exit
code is non-zero on failure.

## `tokenos route` — free routing preview

```sh
tokenos route "rename the function parse_config to load_config"
```

Prints the routing decision (route, reason, signals, confidence, token
estimates, provider chain) **without calling any provider**. Deterministic
and always free.

## `tokenos index` — build the symbol index

```sh
tokenos index .                            # index current directory (in-memory)
tokenos index . --out idx.db               # persist the index
tokenos index . --query "auth token"       # probe the fresh index
```

| Option | Meaning |
|---|---|
| `[root]` | Workspace root (default `.`) |
| `--out <path>` | Persist the index database to a file |
| `--query <q>` | Run a test query against the fresh index |

Parses Go, Python, JS/TS, Rust, Java, C, and Ruby sources into structural
symbols for minimum-viable-context selection.

## `tokenos providers` — filter-matrix verdicts

```sh
tokenos providers
tokenos providers --config ./custom.yaml
```

Lists every provider profile with enablement status, adapter, priority,
costs, and the per-model verdicts of the two-tier include/exclude matrix.

## `tokenos telemetry` — cost effectiveness

```sh
tokenos telemetry
```

Prints:

- **Cost per successful task** (the headline metric)
- Per-route effectiveness (runs, success rate, tokens, latency, cost/success)
- Per-provider health
- Per-provider/per-route attempt health, including failed failover legs
- **UCB1 bandit standings** (arm, pulls, mean reward, mean latency, score)

## `tokenos doctor` — local health diagnostics

```sh
tokenos doctor
tokenos doctor --json
```

Validates the local configuration and reads the SQLite health snapshot without
calling any provider. It reports database integrity (`PRAGMA quick_check`),
table counts, provider enablement, trace policy, solution-cache counters, and
workspace-index status. Non-`ok` SQLite integrity exits non-zero.

## `tokenos attempts` — provider attempt ledger

```sh
tokenos attempts --limit 50
```

Lists recent provider attempts, including failed failover legs, verification
failures, loop-escalation attempts, and final successful provider legs. Use it
when `telemetry` shows retries or failures and you need the exact provider,
model, route, latency, cost, and error reason for each leg.

## `tokenos tasks` — persisted task states

```sh
tokenos tasks --limit 50
```

Lists compressed state objects (ID, goal, status, blockers, updated-at) —
never transcripts.

## `tokenos trace` — flight-recorder replay

```sh
tokenos trace <task-id>             # event timeline
tokenos trace <task-id> --blobs     # include full payload blobs
```

Replays every decision, prompt, response, rescue, and error event for a task
from the out-of-band flight recorder.

## `tokenos config` — show or initialize config

```sh
tokenos config              # print effective configuration
tokenos config init         # write a default config file
tokenos config init --config ./tokenos.yaml
```

`init` refuses to overwrite an existing file.

## `tokenos eval` — run routing-accuracy evaluation

```sh
tokenos eval --dataset ./dataset.json
tokenos eval --dataset ./dataset.yaml
```

| Option | Meaning |
|---|---|
| `--dataset <path>` | Path to the labeled dataset (JSON or YAML format) |
| `[engine flags]` | See above |

Runs the deterministic routing signal extraction and route decision against a labeled corpus. It outputs a comparison table, correct/incorrect statistics, and a mismatch breakdown showing expected vs. predicted routes alongside intent reasons.

## `tokenos serve` — web control panel

```sh
tokenos serve                                  # http://127.0.0.1:8080, local only
tokenos serve --port 3000 --dry-run            # offline demo mode
TOKENOS_AUTH_TOKEN=s3cret tokenos serve --host 0.0.0.0 --public
TOKENOS_AUTH_TOKEN=s3cret tokenos serve --host 0.0.0.0 --public \
  --tls-cert ./fullchain.pem --tls-key ./privkey.pem
```

| Option | Default | Meaning |
|---|---|---|
| `--port <n>` | `8080` | Listen port |
| `--host <h>` | `127.0.0.1` | Listen address |
| `--public` | off | Required to bind a non-loopback interface; **refused without an auth token** |
| `--auth-token <t>` | `$TOKENOS_AUTH_TOKEN` | Bearer token enforced on every `/api/*` request |
| `--tls-cert <path>` | none | PEM certificate file for native HTTPS serving; requires `--tls-key` |
| `--tls-key <path>` | none | PEM private key file for native HTTPS serving; requires `--tls-cert` |
| `[engine flags]` | | See above |

Static assets (`/`, `/app.js`, `/style.css`) are served without auth; all
API endpoints require the bearer token when one is configured. See
[API.md](API.md) for the endpoint reference.

The browser dashboard has an **API token** button in the sidebar. Enter the same
token passed via `--auth-token` or `$TOKENOS_AUTH_TOKEN`; the frontend keeps it
in memory by default, or in `sessionStorage` for the current tab if you opt in.
Interactive executions are capped at four concurrent `/api/run` calls per
server process; saturated requests return `429` while telemetry remains live.
If `security.api_token_rate_limit_per_min` is configured, each bearer token
also has a SQLite-backed per-minute request ledger shared by every TokenOS
process using the same database.

---

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Execution failed, invalid arguments, or refused operation (e.g. `--public` without a token) |

## Common workflows

```sh
# First-time setup
tokenos config init
export ANTHROPIC_API_KEY=sk-ant-...
# edit ~/.config/tokenos/config.yaml: set anthropic disabled: false

# Preview before paying
tokenos route "implement rate limiting middleware"

# Execute with workspace context
tokenos run "implement rate limiting middleware" --workspace ./myproject

# Inspect what happened
tokenos tasks
tokenos trace <task-id> --blobs
tokenos telemetry

# Fully offline smoke test
tokenos run "say hello" --dry-run
```
