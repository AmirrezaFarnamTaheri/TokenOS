# TokenOS Production Readiness

This document defines the maintained release boundary for TokenOS.

## Finalized Artifact Scope

TokenOS is finalized as a local-first Rust execution kernel:

- CLI binary and embeddable library crate.
- Native desktop app backed by egui/eframe and direct engine/store calls.
- Local SQLite state store and content-addressed flight recorder.
- Provider adapters for mock, OpenAI, Anthropic, Gemini, and proxy endpoints.

The browser dashboard, static web assets, Axum HTTP API, and `tokenos serve`
command are retired. TokenOS is not a multi-tenant SaaS platform, identity
provider, patch-application engine, screen-control agent, or fleet governance
plane.

## Closed Local Controls

| Area | Local closure |
|---|---|
| Routing correctness | `ASK` terminates locally with one question and zero provider cost. `REUSE` requires an exact verified solution-cache hit. |
| Native UI | `tokenos app` provides dashboard, action center, command deck, console, planner, policy lab, calibration, operations, readiness, tasks, executions, and configuration views without webview/browser dependencies. |
| Build governance | CI covers formatting, clippy, audit, release build, tests, native builds, and documentation drift checks. |
| Provider safety | Live adapters fail when required key environment variables are missing. Gemini keys travel in headers, not URLs. |
| Cost control | Conservative token budgeting, shadow pricing, per-task budget sentinel, daily/monthly spend limits, and provider cost forecasts are implemented. |
| Traceability | Recorder events are indexed in SQLite; provider attempts are first-class rows and aggregates exposed by CLI and native views. |
| Data minimization | Prompts are masked before provider calls; unmasked output is returned only at the caller boundary. |
| Storage hygiene | Trace disablement, retention pruning, and owner-only permissions are implemented for local artifacts. |
| Web retirement | The active manifest no longer contains the Axum web-control module, `src/webui.rs`, embedded static assets, or `tokenos serve`. Provider networking still uses `reqwest` and its transitive HTTP stack. |
| Licensing | The repository uses `AGPL-3.0-only` and ships the full license text. |

## Release Gates

Run these from the repository root before distributing a build:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo test --locked --features native
cargo audit
cargo build --release --locked
cargo build --release --locked --features native
```

The test count is intentionally not pinned in docs; `cargo test` is the source
of truth.

## Smoke Gates

```sh
tokenos route "fix typo in README"
tokenos run "maybe somehow do something with the thing" --dry-run --json
tokenos run "say hello" --dry-run --json
tokenos providers
tokenos doctor
tokenos app --dry-run
```

Expected behavior:

- Route preview performs no provider call.
- Ambiguous tasks route to `ASK` with zero tokens and zero cost.
- Dry-run execution succeeds against the mock provider.
- Live providers are disabled unless explicitly configured.
- Doctor reports SQLite `quick_check=ok`.
- Native app starts without opening a browser or binding a listener.

## Component Readiness Matrix

| Component | Readiness | Notes |
|---|---|---|
| `kernel` | Production-local | Pure deterministic route ladder; no I/O or provider calls. |
| `engine` | Production-local | Highest blast radius; route, cache, failover, verification, and persistence changes require focused tests. |
| `provider` | Production-local with staging requirement | Mock path is offline; live provider contracts require credentialed staging. |
| `pricing` | Production-local | Bandit and latency/failure learning are process-local; drift ratios persist. |
| `payload` | Production-local | Static-first prompt contract and context distillation are covered by tests. |
| `verify` | Production-local | Static checks always run; configured commands define semantic strength. |
| `store` | Production-local | SQLite is transactional and bundled; encryption is deployment-specific. |
| `recorder` | Production-local | Content-addressed traces should be protected as application logs. |
| `nativeapp` | Production-local | Native egui/eframe UI with direct engine/store integration, readiness checks, and prioritized action synthesis. |
| `.github/workflows/ci.yml` | Source-ready | Hosted branch protection must be enforced by repository administration. |

## Documentation Set

- [ARCHITECTURE.md](ARCHITECTURE.md)
- [DEPLOYMENT.md](DEPLOYMENT.md)
- [SECURITY.md](SECURITY.md)
- [RISK_ACCEPTANCE.md](RISK_ACCEPTANCE.md)
- [API.md](API.md)
- [CLI.md](CLI.md)
- [CONFIGURATION.md](CONFIGURATION.md)
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md)
- [CONTRIBUTING.md](CONTRIBUTING.md)

## Operator-Owned Controls

These cannot be proven from a local checkout:

- GitHub branch protection or repository rulesets requiring CI.
- Live provider compatibility under real credentials and spend limits.
- OS disk encryption or application-level database encryption.
- Fleet-wide quota governance across independent hosts or databases.
- Monitoring, alerting, backup, restore, and incident-response procedures.

TokenOS can be deployed without these only as a local, single-user execution
kernel. Broader deployments require explicit operator acceptance.
