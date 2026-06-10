# TokenOS — Token-Optimal Agent Execution Kernel

A deterministic execution kernel for LLM-driven agents, written in Go. Its single
governing rule:

> **Never spend more resources deciding than the decision can save.**

Routing, verification, loop detection, context selection, and provider choice are
all done **in code, with zero tokens** — the model is only invoked for work that
actually requires generation.

## Headline metric

**Effective Cost Per Successful Task** — surfaced in `tokenos telemetry` and on the
dashboard. Everything in the kernel exists to drive this number down.

## Architecture

```
cmd/tokenos            CLI + embedded web GUI entrypoint
internal/
  kernel/              Deterministic router: route ladder, signals, policy, state
  config/              YAML config, provider chains, two-tier model filter matrix
  engine/              Orchestrator: route → context → payload → failover → verify → record
  provider/            Adapters: mock (fault-injectable), OpenAI, Anthropic, Gemini, proxy-IDE
  pricing/             Shadow pricing  U = confidence / (α·cost + β·latency)  + EWMA trackers
  payload/             JIT cache-aligned prompt builder (static → semi-static → volatile)
  verify/              Tiered verification: free static checks before any LLM call
  tokenizer/           Offline token estimator (no network, no model)
  loopdetect/          Semantic loop detection via Levenshtein distance ceiling (3%)
  contextidx/          Surgical context: structural symbol index (FTS5, LIKE fallback)
  store/               SQLite state store: tasks, failure memory (max 5), telemetry
  recorder/            Out-of-band flight recorder (content-addressable blobs + NDJSON)
  webui/               Embedded control panel (dashboard, run console, traces, config)
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
  outputs; distance < 3% ⇒ semantic loop ⇒ escalate.
- **Surgical context** — workspace parsed into structural symbols (Go, Python,
  JS/TS, Rust, Java, C, Ruby) and queried for the minimum viable context
  (≤ 2000 tokens) instead of shipping whole files.
- **Flight recorder** — every decision, prompt, and response is content-addressed
  (SHA-256) outside the conversation, so debugging never consumes context tokens.
- **Tiered verification** — free static checks (diff shape for PATCH, single-question
  contract for ASK, brace balance, truncation detection) run before anything costs.

## Build

Requires Go ≥ 1.23 and a C compiler (CGO, for SQLite).

```sh
go build -tags sqlite_fts5 -o tokenos ./cmd/tokenos   # full FTS5 index
go build -o tokenos ./cmd/tokenos                     # LIKE-ranking fallback
go test ./...
```

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

## Web control panel

`tokenos serve` embeds a zero-dependency GUI:

- **Dashboard** — cost-per-success KPI, route distribution, per-provider stats
- **Run console** — free route preview (signals + provider chain + token estimates)
  before committing to a paid execution
- **Tasks** — persisted state with flight-recorder trace timeline per task
- **Executions** — full telemetry ledger
- **Configuration** — read-only view (keys stay in env)

## Design principles

1. Decisions are made in code, not in prompts.
2. State lives in SQLite, not in conversation history.
3. Diagnostics live in the flight recorder, not in the context window.
4. Every free check runs before every paid check.
5. Determinism everywhere: same inputs ⇒ same route, same provider order, same payload bytes.
