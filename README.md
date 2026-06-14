# TokenOS - Token-Optimal Agent Execution Kernel

TokenOS is a deterministic execution kernel for LLM-driven agents, written in
native Rust. Its governing rule:

> Never spend more resources deciding than the decision can save.

Routing, verification, loop detection, context selection, provider choice, and
cost forecasting are done locally in Rust. Provider tokens are spent only when
generation is actually required.

## Native-First Status

TokenOS now ships as:

- a native desktop app (`tokenos app`, feature `native`);
- a CLI binary for local automation;
- an embeddable Rust library crate.

The former browser dashboard, embedded static assets, Axum HTTP API, and
`tokenos serve` command are retired. There is no shipped loopback server,
browser control plane, webview shell, or `/api/*` surface in the active product.

## Quick Start

```sh
cargo build --release
./target/release/tokenos config init
./target/release/tokenos route "fix typo"
./target/release/tokenos run "say hello" --dry-run
```

Build and launch the native desktop app:

```sh
cargo build --release --features native
./target/release/tokenos app --dry-run
```

No API key is needed for the offline path. The mock provider exercises routing,
payload construction, verification, recording, and telemetry without network
access or provider spend.

## Documentation

| Document | Contents |
|---|---|
| [docs/GETTING_STARTED.md](docs/GETTING_STARTED.md) | Clone, offline run, native app, live providers |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Dataflow, module invariants, routing ladder, persistence |
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | YAML fields, model filters, provider policy |
| [docs/CLI.md](docs/CLI.md) | Command and flag reference |
| [docs/API.md](docs/API.md) | Retired HTTP API notice and native/CLI replacements |
| [docs/SECURITY.md](docs/SECURITY.md) | Threat model, masking, storage, operator controls |
| [docs/PRODUCTION_READINESS.md](docs/PRODUCTION_READINESS.md) | Release gates and production boundary |
| [docs/RISK_ACCEPTANCE.md](docs/RISK_ACCEPTANCE.md) | External controls and accepted operator-owned risks |
| [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) | Symptom, cause, fix |
| [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md) | Engineering rules and validation checklist |

## Architecture

```text
src/
  lib.rs        Library crate root for embedding TokenOS in other runtimes
  main.rs       CLI and native app dispatch
  kernel.rs     Deterministic router: route ladder, signals, policy, state
  config.rs     YAML config, provider chains, model filter matrix
  engine.rs     Orchestrator: route -> context -> payload -> failover -> verify -> record
  provider.rs   Mock, OpenAI, Anthropic, Gemini, and proxy-compatible adapters
  pricing.rs    Shadow pricing, quota pressure, drift, UCB1 provider bandit
  payload.rs    JIT cache-aligned prompt builder
  verify.rs     Free static verification before paid work is accepted
  tokenizer.rs  Conservative offline token estimator
  jsonrescue.rs Single-pass truncated-JSON rescuer
  maskcodec.rs  Edge secret masking and request-scoped unmasking
  loopdetect.rs Semantic loop detection with persisted windows
  contextidx.rs Structural symbol index for minimum viable context
  store.rs      SQLite state, telemetry, attempts, traces, and cache metadata
  recorder.rs   Content-addressed flight recorder
  nativeapp.rs  Feature-gated egui/eframe desktop UI
```

## Native Desktop App

`tokenos app` is a native egui/eframe application. It calls the Rust engine and
SQLite store directly. It does not start an HTTP server, bind a loopback port,
open a browser, or embed a webview.

The native app includes:

- dashboard KPIs: cost per successful task, estimated savings, success rate,
  total tokens, route effectiveness, provider health, spend, and store health;
- action center with prioritized operator actions synthesized from readiness,
  spend guards, credential state, provider failures, drift, cache settings, and
  recent attempts;
- run console with free route preview, extracted routing signals, provider
  chain diagnostics, provider cost forecast, budget sentinel status, and
  execution output;
- route planner for pasted task backlogs with route mix, provider demand,
  confidence, token estimates, forecast provider spend, budget blocks, and
  all-IMPLEMENT baseline savings;
- policy lab for simulating ASK threshold, DIRECT token ceiling, delegation
  economics, budget sentinel, semantic cache threshold, cascade limits,
  re-ask limit, cache reuse, and learned-routing fallback;
- calibration workbench for YAML/JSON labeled route datasets, APGR, savings,
  mismatch analysis, and ASK-threshold sweeps;
- operations views for circuit breakers, estimator drift, solution-cache
  counters, OpenTelemetry GenAI rollups, provider attempts, tasks, executions,
  and flight-recorder traces.

## CLI

```sh
tokenos config init
tokenos route "fix typo in README"
tokenos run "task" --dry-run
tokenos run "task" --workspace .
tokenos providers
tokenos telemetry
tokenos doctor
tokenos attempts
tokenos tasks
tokenos trace <task-id> --blobs
tokenos index . --query "auth token"
tokenos eval --dataset ./routes.yaml
tokenos app --dry-run
```

API keys are read from environment variables only (`OPENAI_API_KEY`,
`ANTHROPIC_API_KEY`, `GEMINI_API_KEY` by default) and are never written to the
config file.

## Key Mechanisms

- **Routing ladder**: conflict, safety, external blockers, ASK, DIRECT, REUSE,
  PATCH, DELEGATE, PARTIAL, and IMPLEMENT are selected deterministically.
- **ASK is local**: missing critical information produces one local question,
  zero provider/model, zero tokens, and zero cost.
- **Verified solution cache**: exact goal+constraint replays return cached,
  verified outputs at zero tokens.
- **Surgical context**: workspace symbols are indexed and distilled to minimum
  viable context instead of sending whole files.
- **Secret masking**: outbound prompts are masked before network egress;
  unmasking happens only at the caller boundary.
- **Provider failover**: shadow pricing, quota pressure, failure EWMA, UCB1
  evidence, and budget ceilings determine provider order.
- **Provider attempt ledger**: every provider leg is recorded, including failed
  failover attempts and verification failures.
- **Estimator drift watchdog**: actual/estimated token ratios are tracked and
  surfaced in CLI and native operations views.
- **Flight recorder**: decision, prompt, response, rescue, verify, and error
  events are stored out-of-band as content-addressed diagnostics.

## Build And Validation

Requires Rust 1.75 or newer. SQLite is bundled through `rusqlite`.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
cargo build --release --locked --features native
cargo audit
```

## Configuration

`~/.config/tokenos/config.yaml` (override with `$TOKENOS_CONFIG`):

```yaml
current_profile: default
policy:
  ask_threshold: 0.35
  direct_max_tokens: 600
  delegation_penalty: 1500
  delegation_min_scale: 1.5
  max_cost_per_task_usd: 0
  reuse_cache: true
providers:
  anthropic:
    adapter: anthropic
    api_key_env: ANTHROPIC_API_KEY
    model: claude-sonnet-4-20250514
    priority: 1
    models:
      include: ["claude-*"]
      exclude: ["*-haiku-*"]
routing:
  - route: IMPLEMENT
    provider: anthropic
    fallback: [openai, mock]
```

Other environment overrides: `$TOKENOS_DB` for state and `$TOKENOS_TRACES` for
flight-recorder storage.

## Security Properties

- Provider secrets stay in environment variables and request headers, never
  URLs or config files.
- No inbound HTTP listener ships in the active app.
- SQLite access uses prepared statements with bound parameters.
- Regex heuristics use Rust's linear-time `regex` engine.
- Loop detection caps compared text before edit-distance work.
- Trace blobs contain masked prompts/responses but should still be protected as
  sensitive application logs.

## Design Principles

1. Decisions are made in code, not prompts.
2. State lives in SQLite, not conversation history.
3. Diagnostics live in the flight recorder, not the context window.
4. Every free check runs before every paid check.
5. Same inputs produce the same route, provider ordering baseline, and payload
   bytes.

## License

TokenOS is licensed under the GNU Affero General Public License v3.0 only
(`AGPL-3.0-only`). See [LICENSE](LICENSE).
