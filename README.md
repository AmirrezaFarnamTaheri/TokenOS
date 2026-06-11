# TokenOS — Token-Optimal Agent Execution Kernel

A deterministic execution kernel for LLM-driven agents, written in native Rust.
Its single governing rule:

> **Never spend more resources deciding than the decision can save.**

Routing, verification, loop detection, context selection, and provider choice are
all done **in code, with zero tokens** — the model is only invoked for work that
actually requires generation.

## Headline metric

**Effective Cost Per Successful Task** — surfaced in `tokenos telemetry` and on the
dashboard. Everything in the kernel exists to drive this number down.

## Quick start

```sh
cargo build --release                          # no system deps; SQLite is bundled
./target/release/tokenos config init           # write default config
./target/release/tokenos route "fix typo"      # FREE routing preview — zero tokens
./target/release/tokenos run "say hello" --dry-run   # full pipeline, fully offline
./target/release/tokenos serve --dry-run       # dashboard at http://127.0.0.1:8080
```

No API key is needed for any of the above — the fault-injectable mock provider
exercises the entire pipeline offline. See
[docs/GETTING_STARTED.md](docs/GETTING_STARTED.md) for the five-minute tour.

## Documentation

| Document | Contents |
|---|---|
| [docs/GETTING_STARTED.md](docs/GETTING_STARTED.md) | Clone → offline run → dashboard → live providers |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Dataflow, module invariants, routing ladder, bandit, persistence |
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | Every YAML field, the filter matrix, env overrides |
| [docs/CLI.md](docs/CLI.md) | Full command and flag reference with workflows |
| [docs/API.md](docs/API.md) | HTTP API endpoints, shapes, auth, curl cookbook |
| [docs/SECURITY.md](docs/SECURITY.md) | Threat model, masking, auth, parser safety, ops checklist |
| [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) | Symptom → cause → fix |
| [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md) | Ground rules, testing conventions, PR checklist |

## Architecture

```
src/
  lib.rs               Library crate root (kernel embeddable in other runtimes)
  main.rs              CLI (clap) + embedded web GUI entrypoint
  kernel.rs            Deterministic router: route ladder, signals, policy, state, delegation packet
  config.rs            YAML config, provider chains, two-tier model filter matrix
  engine.rs            Orchestrator: route → context → payload → failover → verify → record
  provider.rs          Adapters: mock (fault-injectable), OpenAI, Anthropic, Gemini, proxy-IDE
  pricing.rs           Shadow pricing U = confidence/(α·cost + β·latency) + EWMA + lock-free UCB1 bandit
  payload.rs           JIT cache-aligned prompt builder (static → semi-static → volatile)
  verify.rs            Tiered verification: free static checks before any LLM call
  tokenizer.rs         Offline token estimator + greedy BPE counter (conservative budgeting)
  jsonrescue.rs        Single-pass truncated-JSON rescuer (EOF as soft boundary)
  maskcodec.rs         Edge secret-masking codec (mask outbound, unmask echoes)
  loopdetect.rs        Semantic loop detection: Myers bit-parallel Levenshtein, 3% ceiling
  contextidx.rs        Surgical context: structural symbol index (FTS5, LIKE fallback)
  store.rs             SQLite state store: tasks, goal-keyed failure memory, loop history, telemetry
  recorder.rs          Out-of-band flight recorder (content-addressable blobs + NDJSON)
  webui.rs             Lock-free axum control panel (dashboard, run console, traces, bandit, config)
static/                Embedded dashboard assets (index.html, app.js, style.css)
```

### The routing ladder (execution priority)

| Priority | Route | Trigger |
|---|---|---|
| 1 | `DIRECT` | Trivial task, est. tokens ≤ 600 — answer immediately |
| 2 | `REUSE` | Existing indexed solution satisfies most requirements |
| 2.5 | `PATCH` | Localized change, no repeated failure — minimal diff |
| 3 | `IMPLEMENT` | Default productive path |
| 4 | `PARTIAL` | External blocker — deliver everything completed |
| 5 | `DELEGATE` | Repetitive + bounded + savings exceed delegation penalty |
| 6 | `ASK` | Missing critical info or confidence < 0.35 — exactly one question |
| 7 | `ESCALATE-CONFLICT/SAFETY/EXTERNAL` | Contradictions, safety violations, loops |

Escalations and ASK terminate locally at **zero LLM cost**.

### Key mechanisms

- **Two-tier model filter matrix** — per-provider `include`/`exclude` wildcard lists;
  exclusion always wins, then non-empty include acts as whitelist, else allow.
- **Shadow pricing** — every candidate provider is quoted
  `U = confidence / (α·cost·1000 + β·latency)`, discounted by failure-EWMA and live
  quota pressure; hard context-window constraint; deterministic tie-breaking.
- **Failure memory** — max 5 entries per task; a repeated similar failure forbids the
  same approach and biases routing away from `PATCH`.
- **JIT cache alignment** — payloads are serialized static-first with a byte-stable
  kernel contract so provider prompt caches hit on every call.
- **Loop detection** — normalized Levenshtein distance over a sliding window of 5
  outputs; distance < 3% ⇒ semantic loop ⇒ escalate. The window is **persisted in
  SQLite**, so loops are detected across cold CLI process invocations.
- **Surgical context** — workspace parsed into structural symbols (Go, Python,
  JS/TS, Rust, Java, C, Ruby) and queried for the minimum viable context
  (≤ 2000 tokens) instead of shipping whole files.
- **Flight recorder** — every decision, prompt, and response is content-addressed
  (SHA-256) outside the conversation, so debugging never consumes context tokens.
- **Tiered verification** — free static checks (diff shape for PATCH, single-question
  contract for ASK, brace balance, truncation detection) run before anything costs.
- **UCB1 bandit failover (S19)** — a lock-free multi-armed bandit over the provider
  fleet scales each shadow-priced utility by live observed evidence
  (`0.5 + mean_reward` for explored arms; neutral `1.0` for unexplored arms so
  shadow pricing alone decides and every arm is still explored). Verified
  successes earn latency-discounted reward; transport failures and
  verification failures earn zero. Standings surface in `tokenos telemetry`,
  `/api/stats/bandit`, and the dashboard.
- **Truncated-JSON rescue (S20)** — when the goal demands JSON, a generation cut
  mid-stream (timeout, token limit) is repaired by a single-pass lenient parser
  instead of being discarded: strings cut at EOF keep their partial contents,
  dangling keys are dropped, open containers are closed. A truncation guard
  refuses to "repair" prose that merely starts with a bracket. Every rescue is
  logged to the flight recorder at zero token cost.
- **Conservative token budgeting (S23)** — routing estimates take the max of the
  calibrated chars/token heuristic and a greedy longest-match BPE segmenter, so
  a route is never selected on an underestimate.
- **Delegation packets** — `DELEGATE` routes transmit a minimal JSON contract
  (task, scope, constraints, acceptance, next step) — conclusions only, no
  history, no reasoning.
- **Edge secret masking (S24)** — outbound prompts are scanned for API keys,
  tokens, private-key blocks, passwords, connection strings, emails and IPs;
  secrets are replaced with stable placeholders before any network byte leaves
  the process, and echoes are restored on the response leg. The reverse vault
  lives only in the request's stack frame.
- **Verified solution cache (S25)** — an exact goal+constraints re-request is
  served from a durable SQLite cache at **zero tokens**. Only verified
  successes are admitted; a later failure of the same goal evicts the entry.
  Toggle with `policy.reuse_cache`.
- **Rate-limit circuit breaker (S26)** — a 429 opens a per-provider breaker
  with exponential backoff (5s → 120s cap); failover skips the provider while
  the breaker is open. Retrying a provider that just said "stop" is
  guaranteed waste.
- **Route-scoped output budgets (S27)** — each route caps the output tokens it
  may request: an ASK is one question (256), a PATCH is a minimal diff (2048),
  only full builds get the wide ceiling (4096). Paying for headroom a route's
  contract cannot use is pure waste.
- **Context distillation (S28)** — the context block is distilled before
  transmission: trailing whitespace stripped, blank-line runs collapsed,
  duplicate index headers dropped (code lines are never deduplicated).
  Deterministic and idempotent, so prompt-cache alignment is preserved.
- **Budget sentinel (S29)** — `policy.max_cost_per_task_usd` sets a hard
  per-task ceiling. Over-budget providers are pruned from the chain; if every
  candidate exceeds the ceiling the run terminates locally at zero token cost.
- **Estimator drift watchdog (S30)** — an EWMA of actual÷estimated token
  ratios per provider flags calibration drift outside the trusted band
  [0.75, 1.30]. Surfaced in `tokenos telemetry`, `/api/stats/drift`, and the
  dashboard's Estimator Calibration panel.

## Build

Requires Rust ≥ 1.75 (SQLite is bundled — no system dependencies).

```sh
cargo build --release        # binary at target/release/tokenos — zero warnings
cargo test                   # 170 unit tests across all subsystems, fully offline
```

The crate ships as a library (`src/lib.rs`) plus a thin CLI binary, so the
kernel can be embedded inside other agent runtimes.

## Usage

```sh
tokenos config init                       # write default config (~/.config/tokenos/config.yaml)
tokenos route "fix typo in README"        # FREE routing preview (no LLM call)
tokenos run "task" --dry-run              # full pipeline against the offline mock
tokenos run "task" --workspace .          # surgical context from your codebase
tokenos providers                         # filter-matrix verdicts per provider
tokenos telemetry                         # cost-per-success + per-route stats
tokenos tasks                             # persisted task states
tokenos trace <task-id> --blobs           # flight-recorder timeline + payloads
tokenos index . --query "auth token"      # build & probe the symbol index
tokenos serve --port 8080 --dry-run       # web control panel
```

API keys are read from environment variables only (`OPENAI_API_KEY`,
`ANTHROPIC_API_KEY`, `GEMINI_API_KEY` by default) and are **never** written to disk
or exposed over the web API.

### Configuration

`~/.config/tokenos/config.yaml` (override with `$TOKENOS_CONFIG`):

```yaml
current_profile: default
policy:
  ask_threshold: 0.35
  direct_max_tokens: 600
  delegation_penalty: 1500
  delegation_min_scale: 1.5
  max_cost_per_task_usd: 0   # budget sentinel; 0 = disabled
  reuse_cache: true          # verified solution cache
providers:
  anthropic:
    adapter: anthropic
    api_key_env: ANTHROPIC_API_KEY
    model: claude-sonnet-4-20250514
    priority: 1
    models:
      include: ["claude-*"]
      exclude: ["*-haiku-*"]     # exclusion always wins
routing:
  - route: IMPLEMENT
    provider: anthropic
    fallback: [openai, mock]
```

Other env overrides: `$TOKENOS_DB` (state database), `$TOKENOS_TRACES` (flight
recorder directory).

## Security & concurrency properties

- **No API keys in URLs** — the Gemini adapter authenticates via the
  `X-Goog-Api-Key` request header, never the query string, so secrets cannot
  leak into access logs, proxies, or tracing systems.
- **Lock-free web handlers** — the dashboard shares an `Arc<Engine>` with no
  global mutex; long-running `/api/run` executions never block telemetry reads.
- **ReDoS-immune heuristics** — all routing regexes compile to finite automata
  (Rust `regex` crate): linear-time matching, no catastrophic backtracking.
- **Bounded Levenshtein** — loop comparisons cap input at 20k chars, keeping the
  quadratic pass CPU-bounded on huge generations.

## Web control panel

`tokenos serve` embeds a zero-dependency GUI:

- **Dashboard** — cost-per-success KPI, route distribution, per-provider stats,
  live UCB1 bandit standings (`/api/stats/bandit`)
- **Run console** — free route preview (signals + provider chain + token estimates)
  before committing to a paid execution
- **Tasks** — persisted state with flight-recorder trace timeline per task
- **Executions** — full telemetry ledger
- **Configuration** — read-only view (keys stay in env)

Keyboard-first: views on keys `1`–`5`, `Ctrl+Enter` executes,
`Ctrl+Shift+Enter` previews the route for free. Zero frontend dependencies —
all assets are embedded in the binary at compile time.

Full endpoint reference: [docs/API.md](docs/API.md).

## Design principles

1. Decisions are made in code, not in prompts.
2. State lives in SQLite, not in conversation history.
3. Diagnostics live in the flight recorder, not in the context window.
4. Every free check runs before every paid check.
5. Determinism everywhere: same inputs ⇒ same route, same provider order, same payload bytes.
