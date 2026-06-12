# TokenOS Due-Diligence, Security, Reliability, and Engineering Audit

Audit date: 2026-06-12  
Finalization pass: 2026-06-12 after remediation and verification  
Repository root: `D:\GitHub\TokenOS`  
Branch observed: `main` at `02c157b` tracking `origin/genspark_ai_developer`  
Assessment type: executive-grade technical due diligence, source audit, security review, reliability review, operational readiness review  
Scope: tracked repository files, local build/test/lint execution, local CLI smoke tests, static dependency audit, documentation consistency review, remediation verification  
Out of scope: live provider API compatibility, cloud deployment posture, GitHub repository settings, issue tracker history, external penetration test, production telemetry

## 1. Executive Summary

TokenOS is a compact Rust codebase implementing a local "execution kernel" for LLM-driven agent tasks. It is not a distributed platform in its current form. The actual system is a single Rust library plus CLI binary, an embedded Axum web control plane, an optional native webview shell, embedded static frontend assets, local SQLite persistence, and local content-addressed trace files.

The project has several strong foundations and, after this refinement pass, several former P0/P1 gaps are remediated in code and documentation:

- The default release build succeeds on Windows after installing Rust 1.96.0.
- The optional `native` feature also builds successfully on Windows.
- `cargo test --locked` passes: 186 tests passed, 0 failed.
- `cargo fmt --all -- --check` passes.
- `cargo clippy --all-targets -- -D warnings` passes.
- The default configuration is offline-capable through the mock provider.
- API keys are read from environment variable names rather than config values.
- The web server refuses non-loopback binding unless `--public` and an auth token are supplied.
- The static dashboard now has a bearer-token dialog and injects `Authorization` headers for `/api/*`.
- Prompt masking exists and is tested for common secret classes.
- ASK now terminates locally with one deterministic clarifying question and zero provider/tokens.
- REUSE is now driven by an exact verified solution-cache hit, not by workspace context.
- Active CI now exists at `.github/workflows/ci.yml` with blocking fmt, clippy, audit, build, test, and native build jobs.
- The dependency audit found no known RustSec vulnerabilities in the lockfile at the time tested.

The most decision-relevant residual issues are now narrower:

- **Verification remains shallow:** the live engine still relies on static output checks, not semantic tests or an independent verifier.
- **Native supply-chain warnings remain:** `cargo audit` reports 11 unmaintained GTK3-family crates and 1 unsound `glib` advisory in the lockfile, primarily pulled through optional native GUI dependencies.
- **Trace/data-at-rest governance remains local-only:** traces and SQLite state are masked where designed but not encrypted, retained, pruned, or permission-hardened beyond default filesystem behavior.
- **Remote/multi-user serving remains limited:** `/api/run` has a process-local concurrency limit, timeout, body limit, and bearer auth, but no TLS, per-token scopes, per-user rate limits, or aggregate spend quotas.
- **Supply-chain warnings exist for optional native dependencies:** `cargo audit` reports 11 unmaintained GTK3-family crates and 1 unsound `glib` advisory in the lockfile, primarily pulled through optional native GUI dependencies.

Executive verdict: TokenOS is materially stronger after remediation. The former P0 routing defects, inactive CI, fmt/clippy failures, browser-auth gap, trace-index gap, and `/api/run` process backpressure gap are closed and covered by tests or smoke checks. It is now credible as a local-first, single-user execution kernel prototype with strong deterministic controls. It should still not be marketed as a fully production-grade multi-tenant agent platform until semantic verification, data-retention/encryption controls, native dependency governance, and remote deployment controls mature.

## 2. System Overview

### What It Is

TokenOS is a Rust-native local execution kernel for LLM agent work. It decides a route, builds a prompt, optionally masks secrets, chooses a provider, verifies output with static checks, records state, and exposes telemetry through CLI and web UI.

### What It Is Not

It is not currently:

- A cloud service.
- A multi-tenant service.
- A deployed SaaS platform.
- A full agent runtime that applies patches to a workspace.
- A complete authentication platform.
- A complete remote-access security boundary; use a reverse proxy for TLS and broader access controls.
- A system with durable production alerting or incident response automation.

### Runtime Boundary

The primary runtime boundary is local:

- CLI process invokes `Engine::run`.
- Web server invokes the same `Engine` through Axum handlers.
- Native app wraps the same web UI with a webview over loopback.
- Provider adapters make outbound HTTPS calls only when non-mock providers are enabled and selected.
- SQLite and recorder files are local process-owned artifacts.

### Evidence

- `Cargo.toml:8-14` defines one library and one binary.
- `src/lib.rs:10-26` publicly exports all modules.
- `src/main.rs:48-141` defines the CLI subcommands.
- `src/webui.rs:85-114` builds the embedded static web/API router.
- `src/nativeapp.rs:25-61` launches a native webview over an ephemeral loopback server.
- `src/store.rs:36-110` defines SQLite tables.
- `src/recorder.rs:48-94` writes local trace journals and blobs.

## 3. Architecture Analysis

### Logical Architecture

Logical flow:

1. `main.rs` parses CLI input.
2. `Engine::new` loads config, opens SQLite, creates recorder, tracker, bandit, drift watchdog.
3. `Engine::route_only` or `Engine::run` calculates context and deterministic signals.
4. `kernel::decide` chooses a route.
5. `payload::build` serializes the route, task, constraints, context, and failure memory.
6. `maskcodec::mask_prompt` redacts outbound prompt secrets.
7. `pricing::quote_all` scores candidate providers.
8. `provider::Adapter::execute` calls mock/OpenAI/Anthropic/Gemini/proxy.
9. `payload::extract_solution`, optional `jsonrescue`, and `verify::static_check` process output.
10. `store` records task state and execution telemetry.
11. `recorder` writes forensic events and blobs.
12. `webui` and CLI expose state, traces, telemetry, config, and run controls.

### Physical Architecture

All production code is in:

- `src/*.rs`: Rust implementation.
- `static/*`: embedded dashboard.
- `docs/*`: user and operator documentation.
- `.github/workflows/ci.yml`: active CI/release pipeline.
- `Cargo.toml` and `Cargo.lock`: build and dependency graph.

### Runtime Architecture

Runtime is process-local:

- State: SQLite connection guarded by `Mutex<Connection>`.
- Provider adapter cache: `RwLock<HashMap<String, Arc<Adapter>>>`.
- Provider health tracker: `Mutex<HashMap<String, Health>>`.
- Bandit: atomic counters and atomic f64 wrappers.
- Web handlers: shared `Arc<Engine>` plus process-local `/api/run` semaphore.

Concurrency is generally scoped well: no global web handler lock is held across network calls, and `/api/run` is now capped by a process-local semaphore. SQLite and filesystem operations remain synchronous inside async handlers, and there is still no per-user/per-token aggregate spend limiter.

### Deployment Architecture

Current deployment options:

- CLI binary from `cargo build --release`.
- Local web dashboard via `tokenos serve`.
- Optional native shell via `cargo build --release --features native` and `tokenos app`.

CI/release architecture is active under `.github/workflows/ci.yml`; local verification confirms the same fmt, clippy, test, audit, and build gates are currently clean except for known informational native dependency warnings from `cargo audit`.

## 4. Component Inventory

| Component | Purpose | Owner | Dependencies | Risks | Maturity | Criticality |
|---|---|---|---|---|---|---|
| `src/main.rs` | CLI, command dispatch, public bind guard | Core maintainer inferred | `clap`, `Engine`, `webui` | CLI/docs drift remains possible; public bind guard depends on correct operator token handling | Medium | Critical |
| `src/lib.rs` | Public library root | Core maintainer inferred | All modules | Very broad public API surface | Medium | High |
| `src/kernel.rs` | Route enum, task state, signal extraction, route ladder | Core maintainer inferred | `regex`, `chrono`, `serde` | Heuristic route errors directly affect cost/correctness | Medium | Critical |
| `src/engine.rs` | End-to-end orchestration | Core maintainer inferred | Most modules | Highest blast radius; semantic verification remains shallow | Medium | Critical |
| `src/provider.rs` | Mock/OpenAI/Anthropic/Gemini/proxy adapters | Core maintainer inferred | `reqwest`, provider APIs | Live API compatibility unverified; provider manifests can drift | Medium | Critical |
| `src/pricing.rs` | Provider shadow pricing, health, cooldown, bandit, drift | Core maintainer inferred | atomics, mutex maps | Process-local learning; no durable provider health | Medium | High |
| `src/payload.rs` | Prompt contract and extraction | Core maintainer inferred | `serde_json` | Prompt contract overclaims output quality; extraction heuristic risk | Medium | High |
| `src/verify.rs` | Static verification | Core maintainer inferred | `regex` | "Verified" means shallow static checks only | Low-Medium | Critical |
| `src/tokenizer.rs` | Token estimation and truncation | Core maintainer inferred | embedded vocab | Estimator approximate; drift watchdog mitigates but does not eliminate model-tokenizer divergence | Medium | High |
| `src/jsonrescue.rs` | Lenient truncated JSON repair | Core maintainer inferred | `serde_json` | Can salvage partial data; semantic correctness not guaranteed | Medium | Medium |
| `src/maskcodec.rs` | Secret redaction and unmasking | Core maintainer inferred | `regex` | Pattern coverage incomplete by nature; generated new secrets remain residual risk | Medium | Critical |
| `src/loopdetect.rs` | Semantic loop detection | Core maintainer inferred | custom Levenshtein | False positives/negatives; scope keyed only by task text | Medium | High |
| `src/contextidx.rs` | Symbol extraction and SQLite FTS/LIKE search | Core maintainer inferred | `rusqlite`, `walkdir`, `regex` | Search quality affects prompt relevance; no semantic code understanding | Medium | Critical |
| `src/store.rs` | SQLite persistence | Core maintainer inferred | `rusqlite`, `serde_json` | No encryption/ACL hardening; trace/cache retention controls absent | Medium | Critical |
| `src/recorder.rs` | File-based trace journals and CAS blobs | Core maintainer inferred | filesystem, SHA-256 | Sensitive business content at rest; no retention controls | Medium | High |
| `src/webui.rs` | Axum API and embedded assets | Core maintainer inferred | `axum`, `tokio` | No TLS/CSP/security headers; rate limiting is process-local only | Medium | High |
| `src/nativeapp.rs` | Optional desktop shell | Core maintainer inferred | `tao`, `wry` | Optional dependency warnings; platform-specific runtime risk | Low-Medium | Medium |
| `static/app.js` | Dashboard behavior | Core maintainer inferred | Browser APIs only | In-memory/session token handling; XSS mostly mitigated via escaping | Medium | Medium |
| `static/index.html` | Dashboard shell | Core maintainer inferred | `app.js`, `style.css` | Help text can drift from engine semantics | Medium | Medium |
| `static/style.css` | Dashboard styling | Core maintainer inferred | none | Low technical risk | Medium | Low |
| `docs/*.md` | User/operator docs | Core maintainer inferred | Code behavior | Drift risk reduced but ongoing governance needed | Medium | High |
| `.github/workflows/ci.yml` | Active CI/release | Core maintainer inferred | GitHub Actions | Repository branch protection not verified from local audit | Medium | Critical |
| `Cargo.toml` | Build manifest | Core maintainer inferred | Rust toolchain | Optional GUI dependencies widen lockfile | Medium | Critical |
| `Cargo.lock` | Locked dependency graph | Core maintainer inferred | crates.io | 409 deps per cargo audit lockfile summary | Medium | Critical |
| `TokenOS Main Report.txt` | Untracked large report-like artifact | Unknown | Unknown | Not in git; may confuse evidence provenance | Unknown | Low-Medium |

## 5. Workflow Analysis

### Workflow: Route Preview

Steps:

1. User runs `tokenos route <task>` or POSTs `/api/route`.
2. Engine optionally indexes/query workspace context.
3. Signals are extracted.
4. Route is selected.
5. Provider chain is displayed.

Evidence:

- CLI: `src/main.rs:240-256`.
- Web: `src/webui.rs:264-291`.
- Engine route-only: `src/engine.rs:190-208`.

Observed command:

```text
.\target\release\tokenos.exe route "fix typo in README"
route DIRECT
confidence 0.90
est tokens 155
chain mock
```

Current status: remediated. Workspace context is prompt context only; `has_existing_solution` is now derived from a non-mutating exact solution-cache lookup. CLI smoke after remediation: `tokenos route "implement new webui auth token prompt" --workspace . --dry-run` returned `IMPLEMENT`, not `REUSE`.

### Workflow: Execute Task

Steps:

1. User runs `tokenos run` or POSTs `/api/run`.
2. State object is initialized and saved.
3. Route is selected.
4. ASK and escalations terminate locally.
5. Non-terminal routes check replayable solution cache, build prompt, mask secrets, quote providers, call provider, verify output, record result.

Evidence:

- `src/engine.rs:241-727`.
- `src/webui.rs:319-340`.

Observed command:

```text
.\target\release\tokenos.exe run "say hello" --dry-run --json
route IMPLEMENT
provider mock
success true
tokens_in 117
tokens_out 15
```

Current status: remediated. CLI smoke after remediation: `tokenos run "maybe somehow do something with the thing" --dry-run --json` returned route `ASK`, provider/model omitted, `tokens_in=0`, `tokens_out=0`, `cost_usd=0.0`, and exactly one verified local question.

### Workflow: Web Dashboard

Steps:

1. `tokenos serve` binds host/port.
2. Static assets are served without auth.
3. `/api/*` endpoints are protected only if `auth_token` is configured.
4. UI token dialog captures the bearer token and the single fetch wrapper injects `Authorization`.
5. `/api/run` is guarded by a process-local four-slot semaphore.

Evidence:

- Bind guard: `src/main.rs:440-475`.
- API auth middleware and semaphore state: `src/webui.rs:35-108`.
- Routes: `src/webui.rs:85-114`.
- Frontend fetch helper and token dialog: `static/app.js:7-72`.

Current status: browser auth UX remediated. Residual risk: the dashboard is still plain HTTP and should be placed behind TLS/reverse-proxy controls for non-local use.

### Workflow: Provider Selection and Execution

Steps:

1. `Config::provider_chain` builds route-specific provider sequence.
2. `Engine::quote` creates `Candidate` values.
3. `pricing::quote_all` filters by context and utility.
4. `ordered_providers_banditized` applies bandit exploitation weights.
5. Engine executes providers with failover.

Evidence:

- Chain: `src/config.rs:347-395`.
- Quote: `src/engine.rs:653-683`.
- Pricing: `src/pricing.rs:177-236`.
- Banditized order: `src/engine.rs:761-795`.
- Execution loop: `src/engine.rs:389-637`.

Risk: failed failover attempts are recorded in recorder/failure memory, but not as first-class execution telemetry rows.

### Workflow: Persistence and Trace Replay

Steps:

1. Tasks/executions/cache/loop history are stored in SQLite.
2. Prompts/responses/decisions/errors are written to recorder NDJSON and object files.
3. Engine writes trace metadata rows through `Store::record_trace`.
4. CLI and web replay recorder events.

Evidence:

- SQLite schema: `src/store.rs:36-110`.
- Recorder write: `src/recorder.rs:75-94`.
- CLI trace: `src/main.rs:402-421`.
- Web trace: `src/webui.rs:275-281`.

Current status: trace indexing remediated. Regression test `flight_recorder_events_are_indexed_in_store` asserts SQLite trace count equals recorder event count for a run. Residual risk: failed failover attempts remain trace events rather than first-class execution-attempt telemetry rows.

## 6. Dependency Analysis

### Direct Dependencies

| Dependency | Purpose | Risk Notes |
|---|---|---|
| `anyhow`, `thiserror` | Error handling | Normal |
| `serde`, `serde_json`, `serde_yaml` | Serialization/config | `serde_yaml` is deprecated upstream; no RustSec vuln reported in audit output |
| `clap` | CLI | Normal |
| `rusqlite` with `bundled` | SQLite persistence | Bundled C build increases build complexity but reduces system skew |
| `reqwest` with `rustls-tls`, `http2` | Provider HTTP | Good TLS default; live compatibility unverified |
| `tokio` | Async runtime | Normal |
| `axum`, `tower-http` | Web API | `tower-http` timeout feature present but run timeout implemented via `tokio::time::timeout` |
| `sha2`, `hex` | Hashing IDs/blobs/cache keys | Normal |
| `regex`, `once_cell` | Heuristics and masking patterns | Rust regex avoids catastrophic backtracking |
| `rand` | Task IDs | 64-bit IDs; collision risk low for local usage |
| `chrono`, `dirs`, `walkdir` | Time, paths, indexing | Normal |
| `tao`, `wry` optional | Native desktop shell | Pulls GUI dependency warnings in lockfile |
| `tower` dev | Web tests | Normal |

### Lockfile and Audit Evidence

Commands run:

```text
cargo audit --json
```

Summary:

- `vulnerabilities.found`: false
- `vulnerabilities.count`: 0
- `lockfile.dependency-count`: 409
- advisory database last updated: 2026-06-11 17:21:36 -0700
- warnings:
  - 11 unmaintained packages: `atk`, `atk-sys`, `gdk`, `gdk-sys`, `gdkwayland-sys`, `gdkx11`, `gdkx11-sys`, `gtk`, `gtk-sys`, `gtk3-macros`, `proc-macro-error`
  - 1 unsound package warning: `glib`

Inference: default non-native builds are materially less exposed than native builds, but the lockfile and optional native artifact strategy still carry governance and future maintenance risk.

## 7. Security Assessment

### Authentication and Authorization

Strengths:

- Default bind is loopback: `src/main.rs:129`.
- Non-loopback requires `--public`: `src/main.rs:451-456`.
- Non-loopback also requires non-empty auth token: `src/main.rs:458-462`.
- `/api/*` can enforce bearer token: `src/webui.rs:53-77`.
- The dashboard can capture a bearer token and inject `Authorization` for all API calls: `static/app.js:7-72`.
- Constant-time comparison now folds length mismatch into the accumulator rather than returning early.
- Tests cover missing, wrong, and correct token: `src/webui.rs:379-416`.

Weaknesses:

- No TLS is served by TokenOS itself; docs require reverse proxy for remote.
- No CSP or common security headers are set for static assets.
- No server-side RBAC: token equals full control.
- Token storage is browser memory by default with opt-in sessionStorage; no cookie/session invalidation model exists.

### Secrets

Strengths:

- Provider API keys are env-var based in config defaults: `src/config.rs:180`, `src/config.rs:200`, `src/config.rs:220`.
- `/api/config` returns config, not env var values: `src/webui.rs:244-247`.
- Secret masking patterns cover many common formats: `src/maskcodec.rs:32-80`.
- Engine masks prompt before provider call: `src/engine.rs:333-334`.
- Tests assert prompt blobs and durable sinks do not contain known prompt secrets: `src/engine.rs:1127-1205`.
- Live provider adapters fail early if required API key environment values are missing.
- Placeholder-bearing outputs are not admitted to the verified solution cache.

Weaknesses:

- Generated inbound secrets not present in the original prompt are not generally re-masked before recorder storage.
- SQLite and trace files are not encrypted by implementation.
- No explicit Windows ACL or Unix chmod hardening is applied.

### Attack Surface

Primary surfaces:

- CLI arguments and environment variables.
- Local web API.
- Static browser app.
- Provider HTTP adapters.
- SQLite database files.
- Recorder trace files.
- Config YAML.
- Native webview shell.

Most credible attacks:

- Local process sends unauthenticated loopback requests in default serve mode.
- Misconfigured public bind with weak token exposes paid execution endpoint.
- Provider output causes trace/business-content exposure.
- Optional native dependencies increase supply-chain exposure.

## 8. Reliability Assessment

Strengths:

- Unit test count is meaningful for codebase size: 186 tests passed locally.
- Release build succeeds.
- Native release build succeeds on Windows.
- SQLite uses WAL and busy timeout: `src/store.rs:132-134`.
- Provider 429 cooldown exists: `src/pricing.rs:125-167`.
- Engine failover records failures and tries next provider: `src/engine.rs:462-482`.
- Run endpoint has 300-second timeout: `src/webui.rs:28-33`, `src/webui.rs:303-308`.
- Run endpoint has a process-local concurrency limiter.
- ASK and REUSE regressions are covered by tests and CLI smoke checks.

Weaknesses:

- Static verification is shallow but feeds "verified solution cache".
- No automatic recovery/repair for corrupted SQLite rows; listing functions silently drop deserialization failures.
- Bandit and drift data are process-local, not durable.
- Recorder writes do not use explicit per-task journal locking.

## 9. Scalability Assessment

TokenOS is appropriate for single-user local workflows and modest local server usage. It is not designed for high-concurrency multi-user serving.

Scaling positives:

- No npm/frontend dependency chain.
- Prompt context is capped to 2000 estimated tokens after indexing: `src/engine.rs:193-195`.
- Indexer skips `target`, `node_modules`, `vendor`, `dist`, `build`, `.git`, etc.: `src/contextidx.rs:232-234`.
- Request body cap is 256 KiB: `src/webui.rs:31-33`.
- `/api/run` concurrency is capped at four process-local execution slots.
- Loop comparison caps input at 20,000 chars: `src/loopdetect.rs:12-15`.

Scaling limits:

- SQLite connection is serialized through one mutex.
- Web `/api/run` has no per-token, per-user, daily, monthly, or organization-level budget governor.
- Indexing is synchronous and in-memory by default for CLI workspace usage.
- Provider health and bandit learning do not survive process restarts.
- No queue, admission control, or cancellation API exists.
- Trace retention is unbounded.

## 10. Operational Assessment

Operational strengths:

- CLI is comprehensive for a prototype: run, route, index, providers, telemetry, tasks, trace, config, serve, app.
- Dry-run mode supports offline demos and tests.
- Recorder gives concrete forensic evidence.
- Active CI runs fmt, clippy, audit, build, tests, and native builds.
- Docs are broad and useful.

Operational risks:

- No deployment guide for a hardened remote service beyond high-level reverse proxy guidance.
- No log retention or trace retention controls.
- No structured incident response workflow.
- Release provenance depends on GitHub Actions/tag discipline; branch protection was not verified locally.
- No documented ownership or codeowners.
- No active monitoring/alerting beyond dashboard/telemetry views.

## 11. Documentation Assessment

Documentation is extensive and has been updated to reflect the remediated implementation. The main remaining documentation risk is governance drift: docs are still manually maintained and should be protected by review discipline.

Documented correctly:

- Default loopback bind.
- Env-var API key names.
- Local SQLite and trace locations.
- Active CI location is documented.
- Static assets are embedded.
- Dry-run mock provider exists.

Former drift or overclaims corrected in this pass:

- ASK zero-token behavior now matches code and docs.
- README test count now reflects 186 tests.
- README build section documents fmt/clippy gates explicitly.
- API/CLI/security/troubleshooting docs describe dashboard bearer-token entry.
- REUSE is documented as exact verified solution-cache replay, while workspace context is documented as prompt context only.

## 12. Findings Catalog

### F-01: ASK Routes Are Documented as Zero-Token Local Terminations but Call Providers

**Description**  
Baseline: the project claimed ASK terminated locally at zero LLM cost, but `Engine::run` sent ASK through prompt construction and provider execution. Current status: **Remediated**. ASK now emits one deterministic local clarifying question, marks the task blocked, records verification, and returns with no provider/model/tokens.

**Evidence**  
Baseline evidence: docs claimed local ASK termination while old `Engine::run` only short-circuited escalations; observed baseline command returned route `ASK`, provider `mock`, `tokens_in=123`, `tokens_out=29`.  
Remediation evidence: `ask_resolves_locally_without_provider_or_tokens` passes in the test suite. CLI smoke after remediation returned route `ASK`, no provider/model fields, `tokens_in=0`, `tokens_out=0`, `cost_usd=0.0`, and a passing static verification result.

**Root Cause**  
The route abstraction has `is_terminal_local`, but `Engine::run` uses `is_escalation` instead of `is_terminal_local`.

**Technical Impact**  
Before remediation, ASK consumed provider calls, prompt tokens, latency, quota, and failure surface. After remediation, ASK is a local deterministic branch.

**Operational Impact**  
Unexpected live-provider spend for ambiguous tasks is removed. Operators still need to answer the persisted question before execution can continue.

**Security Impact**  
Ambiguous prompts no longer leave the process merely because a clarifying question is needed.

**Reliability Impact**  
ASK no longer depends on provider availability, authentication, or rate limits.

**Scalability Impact**  
Ambiguous tasks do not create upstream traffic.

**Business Impact**  
Directly weakens the headline cost-control proposition.

**Likelihood**  
Baseline: High. Current residual likelihood: Low.

**Severity**  
Baseline: Critical. Current residual severity: Low.

**Confidence**  
High.

**Remediation**  
Implemented: `Engine::run` now handles `Route::Ask` before cache/provider execution; `clarifying_question` synthesizes one local question; the engine persists blocked state and records an `ask` trace event.

**Effort**  
M.

**Priority**  
P0 baseline; closed.

**Validation Method**  
Completed: `cargo test --locked`; CLI smoke `tokenos run "maybe somehow do something with the thing" --dry-run --json`.

### F-02: Workspace Context Hits Are Misclassified as Existing Solutions, Causing Incorrect REUSE Routes

**Description**  
Baseline: when a workspace index returned any minimum viable context, the engine treated that as `has_existing_solution`, causing false `REUSE`. Current status: **Remediated**. `has_existing_solution` is now derived from an exact verified solution-cache hit; workspace context only informs prompt construction.

**Evidence**  
Baseline evidence:

```text
tokenos index . --query "tokenizer truncate" -> indexed 250 symbols; found src\tokenizer.rs truncate
tokenos route "fix tokenizer truncate bug" --workspace . -> REUSE
tokenos route "rename function route_only to preview_route" --workspace . -> REUSE
tokenos route "implement new webui auth token prompt" --workspace . -> REUSE
```

Remediation evidence: `workspace_context_hit_does_not_imply_reuse` passes in the test suite. CLI smoke after remediation: `tokenos route "implement new webui auth token prompt" --workspace . --dry-run` indexed 265 symbols and returned `IMPLEMENT`.

**Root Cause**  
The code conflates "relevant context exists" with "solution exists."

**Technical Impact**  
Before remediation, normal edit/implementation tasks could receive the wrong route contract and provider chain. After remediation, context hits and cache hits are separate signals.

**Operational Impact**  
Workspace users now get route previews that distinguish "relevant source exists" from "verified answer already exists."

**Security Impact**  
The specific route-confusion path is closed. Residual risk remains that heuristic routing can still be imperfect.

**Reliability Impact**  
False REUSE from workspace context is covered by regression tests.

**Scalability Impact**  
REUSE remains zero-token replay only when an exact verified cache entry exists.

**Business Impact**  
Damages trust in the core routing engine.

**Likelihood**  
Baseline: High for workspace users. Current residual likelihood: Low.

**Severity**  
Baseline: Critical. Current residual severity: Low-Medium.

**Confidence**  
High.

**Remediation**  
Implemented: `route_only_with_constraints` and execution routing use a non-mutating solution-cache lookup; `Store::peek_cached_solution` prevents preview side effects; docs clarify that context does not imply REUSE.

**Effort**  
M.

**Priority**  
P0 baseline; closed.

**Validation Method**  
Completed: `workspace_context_hit_does_not_imply_reuse`, `exact_solution_cache_hit_routes_reuse_without_mutating_hits`, and CLI smoke with `--workspace .`.

### F-03: CI and Release Pipeline Are Not Active (Remediated)

**Description**  
Baseline: the CI file was stored under `.github/workflows-staged/ci.yml`, not `.github/workflows/ci.yml`. Current status: **Remediated in repository files**. Active CI now exists at `.github/workflows/ci.yml`; the stale staged workflow files were removed.

**Evidence**  
Baseline evidence: `.github/workflows-staged/README.md` documented the staged workaround and `git ls-files` contained `.github/workflows-staged/ci.yml`.  
Remediation evidence: `.github/workflows/ci.yml` now contains blocking fmt, clippy, audit, build, test, native build, artifact, and release jobs.

**Root Cause**  
Repository automation permission limitation led to staged workflow workaround.

**Technical Impact**  
No guaranteed automated build/test/lint gate on pushes or PRs.

**Operational Impact**  
Broken changes can merge unnoticed.

**Security Impact**  
Supply-chain and release checks are not automatically enforced.

**Reliability Impact**  
Regression prevention depends on humans.

**Scalability Impact**  
Team growth increases change-control risk.

**Business Impact**  
Weak due-diligence signal for investors/acquirers.

**Likelihood**  
Baseline: High. Current residual likelihood: Medium only if hosted repository settings do not require the workflow.

**Severity**  
Baseline: High. Current residual severity: Medium until branch protection is verified externally.

**Confidence**  
High.

**Remediation**  
Implemented: active workflow added at `.github/workflows/ci.yml`; staged workflow files removed. Still recommended: configure branch protection in GitHub to require these checks.

**Effort**  
S.

**Priority**  
P1 baseline; repository-file portion closed.

**Validation Method**  
Open a PR and verify required GitHub checks run and block failures. Local source audit cannot verify hosted branch protection.

### F-04: Current Quality Gates Fail Under Rust 1.96.0 (Remediated)

**Description**  
Baseline: local test/build passed, but `cargo fmt --check` and clippy failed. Current status: **Remediated**.

**Evidence**  
Installed toolchain: `rustc 1.96.0`, `cargo 1.96.0`.  
Baseline evidence: `cargo fmt --all -- --check` exited 1 and `cargo clippy --all-targets -- -D warnings` exited 101.  
Remediation evidence: `cargo fmt --all -- --check` exits 0; `cargo clippy --all-targets -- -D warnings` exits 0; `cargo test --locked` exits 0 with 186 passed; release and native release builds exit 0.

**Root Cause**  
Formatting drift accumulated because CI did not enforce it; current active CI makes these gates blocking.

**Technical Impact**  
Strict CI cannot be enabled without fixes.

**Operational Impact**  
Developers receive noisy or ignored quality signals.

**Security Impact**  
Weak gates reduce chance of catching risky patterns.

**Reliability Impact**  
Low direct runtime impact; high governance impact.

**Scalability Impact**  
Code quality consistency degrades as contributors increase.

**Business Impact**  
Red flag in audit review despite passing tests.

**Likelihood**  
Baseline: Current. Current residual likelihood: Low if CI is enforced.

**Severity**  
Baseline: Medium. Current residual severity: Low.

**Confidence**  
High.

**Remediation**  
Implemented: ran `cargo fmt --all`; changed tokenizer midpoint calculation to `.div_ceil(2)`; fixed an additional clippy `print_literal` warning in `src/main.rs`.

**Effort**  
S.

**Priority**  
P1 baseline; closed.

**Validation Method**  
Run `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`; both must exit 0.

### F-05: Authenticated Browser Dashboard Is Not Usable Because Frontend Sends No Bearer Token (Remediated)

**Description**  
Baseline: the backend supported bearer auth for `/api/*`, and public bind required auth, but the embedded frontend never attached an `Authorization` header. Current status: **Remediated**.

**Evidence**  
Baseline evidence: auth middleware expected `Authorization: Bearer <token>`, while the frontend helper called `fetch(path, opts)` without adding headers.  
Remediation evidence: `static/index.html` contains an API token modal; `static/app.js` stores the token in memory by default, optionally in `sessionStorage`, and injects `Authorization` in the shared `api` wrapper for every `/api/*` call. CLI/API/security/troubleshooting docs now describe this flow.

**Root Cause**  
Backend auth was added before a corresponding UI token-entry/session design. The design now exists.

**Technical Impact**  
Secured API works for curl/scripts and the dashboard UX.

**Operational Impact**  
Auth friction is reduced. Residual operational risk remains if users expose plain HTTP without TLS.

**Security Impact**  
Residual risk is weak token selection or lack of TLS/RBAC, not missing frontend auth support.

**Reliability Impact**  
Recommended secure mode is usable through the browser.

**Scalability Impact**  
Blocks team/shared remote usage.

**Business Impact**  
Weakens product usability.

**Likelihood**  
Baseline: High for remote dashboard users. Current residual likelihood: Low-Medium.

**Severity**  
Baseline: High. Current residual severity: Medium because remote deployment still needs TLS/RBAC outside TokenOS.

**Confidence**  
High.

**Remediation**  
Implemented: browser-side token modal, in-memory default, optional sessionStorage, shared fetch-wrapper header injection, and documentation updates.

**Effort**  
M.

**Priority**  
P1 baseline; closed.

**Validation Method**  
Manual browser validation is still recommended for hosted deployments; source-level implementation and API tests cover server token behavior.

### F-06: Constant-Time Auth Claim Is Overstated (Remediated)

**Description**  
Baseline: the code returned false immediately when token lengths differed, despite comments/docs claiming constant-time comparison. Current status: **Remediated**.

**Evidence**  
Baseline evidence: old `ct_eq` returned early on length mismatch.  
Remediation evidence: current `ct_eq` folds length difference into the accumulator and iterates across the maximum length; `constant_time_eq_basics` passes.

**Root Cause**  
Hand-rolled comparison originally handled equal-length tokens only. Current implementation avoids the early-return length leak.

**Technical Impact**  
Baseline: minor timing side channel. Current residual: implementation still hand-rolled; a vetted crate could further improve audit posture.

**Operational Impact**  
Mostly low unless remote token guessing is possible and latency noise is low.

**Security Impact**  
Docs overstate side-channel resistance.

**Reliability Impact**  
None.

**Scalability Impact**  
None.

**Business Impact**  
Audit credibility issue.

**Likelihood**  
Baseline: Medium. Current residual likelihood: Low.

**Severity**  
Baseline: Low-Medium. Current residual severity: Low.

**Confidence**  
High.

**Remediation**  
Implemented: length difference is folded into the accumulator and comparison iterates over the maximum length. Future optional improvement: use a vetted crate such as `subtle`.

**Effort**  
S.

**Priority**  
P2 baseline; closed.

**Validation Method**  
Unit test behavior and review implementation. Optionally benchmark mismatch paths for no early-return behavior.

### F-07: Config Validation Does Not Fail Fast on Missing Live Provider Secrets or Invalid Economic Values (Mostly Remediated)

**Description**  
Baseline: enabled API-key providers could be constructed with empty API keys, and config validation checked only basic provider/routing references. Current status: **Mostly remediated**.

**Evidence**  
Baseline evidence: old provider construction used `unwrap_or_default()` for missing env vars; validation checked provider references and adapter presence only.  
Remediation evidence: `Config::validate` now rejects unknown adapters, enabled live providers without `api_key_env`, proxy providers without endpoints, non-positive live context windows, and negative costs. `Adapter::new` rejects live adapters when the resolved environment variable is empty. Tests cover unknown adapter, missing env key name, and live adapter credential failure.

**Root Cause**  
Config validation was structural, not semantic. Current validation covers the most important semantic live-provider invariants.

**Technical Impact**  
Most missing-key and invalid-provider cases now fail before or during adapter construction with specific messages.

**Operational Impact**  
Operators diagnose auth failures later than necessary.

**Security Impact**  
Weak validation can conceal unintended provider enablement or zero-price live providers.

**Reliability Impact**  
Provider failover sees fewer avoidable auth failures. Residual risk remains for live API contract drift and provider-side auth semantics.

**Scalability Impact**  
Misconfigured fleets degrade routing and telemetry.

**Business Impact**  
Cost and provider reliability metrics become unreliable.

**Likelihood**  
Baseline: Medium-High. Current residual likelihood: Medium.

**Severity**  
Baseline: Medium. Current residual severity: Low-Medium.

**Confidence**  
High.

**Remediation**  
Implemented: required live env var names, non-empty resolved live keys at adapter construction, non-negative costs, positive context windows for live providers, valid adapter enum, and proxy endpoint checks. Residual recommended work: validate route names and policy numeric ranges more exhaustively.

**Effort**  
M.

**Priority**  
P1 baseline; mostly closed.

**Validation Method**  
Completed for missing env key name, invalid adapter, and adapter-level missing credential. Add route-name/policy-range tests as future hardening.

### F-08: Verified Solution Cache Can Return Masked Placeholders Instead of User-Facing Secrets (Remediated)

**Description**  
Baseline: successful outputs were unmasked before returning to the caller, but the durable solution cache stored the masked form and could replay placeholders later. Current status: **Remediated**.

**Evidence**  
Baseline evidence: old cache admission stored masked output and cache hit returned `cached_out` directly.  
Remediation evidence: `maskcodec::contains_placeholder` detects opaque secret placeholders; engine skips cache admission for placeholder-bearing output and evicts old placeholder-bearing cache hits. `durable_sinks_never_hold_unmasked_secrets` now asserts placeholder-bearing output does not enter the solution cache.

**Root Cause**  
Security design stores masked durable output, but cache replay semantics are not adjusted for secret-bearing tasks.

**Technical Impact**  
Placeholder-bearing outputs remain safe in durable traces but are not reused as future user-facing cache answers.

**Operational Impact**  
Users see degraded or unusable cached answers for credential rotation/config tasks.

**Security Impact**  
Fail-safe for secrets, but can create confusion and possible unsafe manual substitution.

**Reliability Impact**  
Violates "cache returns verified output verbatim" for secret-bearing prompts.

**Scalability Impact**  
Cache effectiveness drops for sensitive tasks.

**Business Impact**  
Trust in zero-token cache is reduced.

**Likelihood**  
Baseline: Medium. Current residual likelihood: Low.

**Severity**  
Baseline: Medium. Current residual severity: Low.

**Confidence**  
High.

**Remediation**  
Implemented: outputs containing opaque secret placeholders are not admitted to durable cache; stale placeholder cache hits are evicted.

**Effort**  
S-M.

**Priority**  
P2 baseline; closed.

**Validation Method**  
Completed via durable-sink regression test and cache replay guard. Future test can add explicit second-run coverage for a stale manually inserted placeholder cache row.

### F-09: Telemetry and Trace Persistence Are Split in a Way That Weakens Auditability (Partially Remediated)

**Description**  
Baseline: executions table recorded final run outcomes, while failed failover attempts lived only in recorder/failure memory; the `traces` SQLite table existed but was unused. Current status: **Partially remediated**.

**Evidence**  
Baseline evidence: `Store::record_trace` existed with no source usage.  
Remediation evidence: engine recorder writes now flow through `record_event`, which writes both the recorder journal/blob and SQLite trace metadata; `flight_recorder_events_are_indexed_in_store` asserts recorder events and SQL trace rows stay in sync. Residual evidence: provider attempts are still not modeled as first-class `execution_attempts` rows.

**Root Cause**  
Two diagnostic systems evolved in parallel. Trace indexing is now unified, but execution-attempt telemetry remains separate from final execution rows.

**Technical Impact**  
Trace metadata is queryable, but provider failure rates and retry reasons still require recorder replay rather than SQL attempt aggregation.

**Operational Impact**  
Dashboards underreport failover pain.

**Security Impact**  
Incident review may miss events if filesystem traces are unavailable.

**Reliability Impact**  
Root-cause analysis is slower.

**Scalability Impact**  
Ad hoc trace files become harder to aggregate.

**Business Impact**  
Weak cost/reliability evidence for provider selection.

**Likelihood**  
Baseline: High. Current residual likelihood: Medium.

**Severity**  
Medium; partially reduced.

**Confidence**  
High.

**Remediation**  
Implemented: wired recorder events into the trace table. Remaining remediation: create a unified `execution_attempts` table with task_id, provider, model, route, attempt_index, status, error_class, latency, tokens, and cost.

**Effort**  
M.

**Priority**  
P2; partially closed.

**Validation Method**  
Completed for trace indexing. Remaining validation: inject provider failures and assert attempts appear in first-class SQL telemetry and web API once `execution_attempts` exists.

### F-10: Optional Native Dependency Chain Has RustSec Informational Warnings

**Description**  
`cargo audit` found no known vulnerabilities, but reported unmaintained GTK3-family crates and an unsound `glib` advisory in the lockfile.

**Evidence**  
`cargo audit --json` exit 0.  
Warnings: 11 unmaintained packages and 1 unsound package (`glib`).  
Optional native dependencies in `Cargo.toml:38-44` enable `tao` and `wry`.

**Root Cause**  
Native webview ecosystem pulls GTK3-era crates through optional dependency graph.

**Technical Impact**  
Default build risk appears lower, but native releases inherit maintenance warnings.

**Operational Impact**  
Native app support may become harder on Linux/GTK stacks.

**Security Impact**  
Unsound dependency advisory requires review even if code paths may not use affected functions.

**Reliability Impact**  
Platform-specific crashes or build drift possible.

**Scalability Impact**  
Cross-platform release burden grows.

**Business Impact**  
Native app promise carries hidden maintenance liability.

**Likelihood**  
Medium.

**Severity**  
Medium.

**Confidence**  
Medium-High.

**Remediation**  
Track native dependency advisories separately. Evaluate newer `wry`/GTK4-compatible paths when available. Consider making native release experimental until warnings are cleared or risk-accepted.

**Effort**  
M-L.

**Priority**  
P2.

**Validation Method**  
Run `cargo audit`, `cargo build --features native`, and smoke tests on Linux/macOS/Windows in active CI.

### F-11: Sensitive Local Artifacts Lack Implementation-Level Retention and Permission Controls

**Description**  
SQLite state and trace files are created under default user paths, but no explicit permissions, encryption, retention, or scrubbing controls are implemented.

**Evidence**  
Default DB path: `src/store.rs:24-34`.  
Default trace path: `src/recorder.rs:35-46`.  
Directories created with `create_dir_all`: `src/store.rs:119-129`, `src/recorder.rs:50-56`.  
Security docs warn traces contain business content: `docs/SECURITY.md:97-112`.

**Root Cause**  
Local-first prototype uses default filesystem security.

**Technical Impact**  
Data protection depends on OS/user profile defaults.

**Operational Impact**  
Shared machines and backups can retain sensitive prompts/responses indefinitely.

**Security Impact**  
Business-sensitive content can persist unencrypted.

**Reliability Impact**  
Trace growth can consume disk over time.

**Scalability Impact**  
No retention policy for long-running use.

**Business Impact**  
Compliance and customer-data concerns.

**Likelihood**  
Medium.

**Severity**  
Medium.

**Confidence**  
High.

**Remediation**  
Add retention settings, trace pruning, optional trace disablement, explicit permission hardening where portable, and documented encryption-at-rest guidance. Consider SQLCipher or OS keychain integration for sensitive deployments.

**Effort**  
M-L.

**Priority**  
P2.

**Validation Method**  
Run integration tests confirming file mode/ACL where applicable, retention pruning, and trace disablement.

### F-12: "Verified Success" Is Based on Shallow Static Checks Only

**Description**  
The verification module describes a tiered budget including targeted tests and LLM verifier, but live engine execution calls only `verify::static_check`.

**Evidence**  
Verification doc comment: `src/verify.rs:1-8`.  
Engine call: `src/engine.rs:521-526`.  
Static check implementation: `src/verify.rs:28-71`.  
`Status::Verifying` exists but was not found in live usage beyond enum definition.

**Root Cause**  
Verification roadmap is documented in comments, but only first tier is implemented.

**Technical Impact**  
Outputs can be cached after passing format checks, not semantic correctness checks.

**Operational Impact**  
Telemetry "success" can overstate actual task completion.

**Security Impact**  
Malicious or unsafe model output can pass if structurally plausible.

**Reliability Impact**  
False positives in success/cache admission.

**Scalability Impact**  
Bad cached answers can propagate cheaply.

**Business Impact**  
Due-diligence concern: "verified" is not equivalent to tested.

**Likelihood**  
High.

**Severity**  
High.

**Confidence**  
High.

**Remediation**  
Rename current status to "static_verified" or implement route-specific verification tiers. For code tasks, run configured tests or diff validation before cache admission. Store verification tier in telemetry and cache metadata.

**Effort**  
L.

**Priority**  
P1.

**Validation Method**  
Add failing semantic-output fixtures that pass static shape and verify they are not cached without stronger validation.

### F-13: `/api/run` Has No Server-Wide Concurrency, Spend, or Rate Limit (Partially Remediated)

**Description**  
Baseline: the endpoint had a per-request timeout and request body limit, but no run concurrency limit, per-token spend enforcement at server level, or per-token auth scopes. Current status: **Partially remediated**: a process-local concurrency limiter now caps `/api/run` at four simultaneous executions.

**Evidence**  
Baseline evidence: run timeout and body limit existed; no semaphore/rate limiter was present.  
Remediation evidence: `MAX_CONCURRENT_RUNS` and `WebState.run_limiter` are implemented in `src/webui.rs`; saturated requests return HTTP `429` before provider execution.

**Root Cause**  
Single-user local assumption. The first layer of backpressure is now implemented, but aggregate cost/rate governance remains future work.

**Technical Impact**  
Concurrent callers are capped per process. They can still consume paid provider executions up to that limit and across restarted/multiple processes.

**Operational Impact**  
A shared/public deployment still needs external rate limiting, TLS, and aggregate spend controls.

**Security Impact**  
Token compromise grants execution up to process-local limits until revoked; no per-token scopes or quotas exist.

**Reliability Impact**  
Provider quota exhaustion and local resource pressure.

**Scalability Impact**  
Process-local backpressure exists; distributed or per-user backpressure does not.

**Business Impact**  
Cost exposure.

**Likelihood**  
Baseline: Medium in remote/shared usage. Current residual likelihood: Medium-Low for single-process serving; Medium for public/shared deployments.

**Severity**  
Baseline: Medium-High. Current residual severity: Medium.

**Confidence**  
High.

**Remediation**  
Implemented: per-process semaphore and `429` saturation response. Remaining: per-token rate limits, daily/monthly cost counters, aggregate budgets, and distributed deployment limits.

**Effort**  
M-L.

**Priority**  
P2; partially closed.

**Validation Method**  
Add a concurrent web integration test or load test asserting excess `/api/run` requests return `429` without provider calls.

### F-14: Documentation and Evidence Provenance Drift (Mostly Remediated)

**Description**  
Baseline: docs were extensive but contained multiple drift points, and a large untracked report-like file existed in the worktree. Current status: **Mostly remediated for tracked documentation**; the unrelated untracked `TokenOS Main Report.txt` remains untouched because ownership is unknown.

**Evidence**  
Baseline evidence: README said 177 tests while local test count was 180; README implied zero-warning gates while fmt/clippy failed; `git status --short` reported `?? "TokenOS Main Report.txt"`.  
Remediation evidence: README, architecture, security, API, CLI, configuration, getting-started, troubleshooting, contributing, and this report were updated to reflect 186 tests, active CI, ASK/REUSE semantics, browser auth, run backpressure, trace indexing, and cache replay guards. `TokenOS Main Report.txt` remains untracked.

**Root Cause**  
Documentation drift came from manual updates without active CI. CI is now active for code quality; docs still need deeper automated snippet/provenance checks.

**Technical Impact**  
Reviewers may rely on stale claims.

**Operational Impact**  
Confusing artifacts in worktree.

**Security Impact**  
Untracked reports can contain sensitive or unsupported claims.

**Reliability Impact**  
Low direct runtime impact.

**Scalability Impact**  
Knowledge management degrades with team size.

**Business Impact**  
Due-diligence credibility risk.

**Likelihood**  
Baseline: High. Current residual likelihood: Medium.

**Severity**  
Baseline: Medium. Current residual severity: Low-Medium for tracked docs; unknown for untracked external report.

**Confidence**  
High.

**Remediation**  
Implemented tracked-doc update. Remaining: add docs smoke checks for command snippets/counts and decide whether `TokenOS Main Report.txt` is to be committed, ignored, archived, or deleted by the owner.

**Effort**  
S-M.

**Priority**  
P2.

**Validation Method**  
Run docs smoke tests in CI and enforce clean worktree before release.

## 13. Risk Register

| ID | Risk | Severity | Likelihood | Priority | Owner | Status |
|---|---|---:|---:|---:|---|---|
| R-01 | ASK route consumes provider calls despite zero-token claim | Critical baseline / Low current | High baseline / Low current | P0 | Core | Closed |
| R-02 | Workspace context hit causes false REUSE | Critical baseline / Low-Medium current | High baseline / Low current | P0 | Core | Closed |
| R-03 | CI inactive | High baseline / Medium current | High baseline / Medium current | P1 | Maintainer | Source remediated; hosted branch protection unverified |
| R-04 | Clippy/fmt fail | Medium baseline / Low current | Current baseline / Low current | P1 | Core | Closed |
| R-05 | Authenticated dashboard unusable | High baseline / Medium current | High baseline / Low-Medium current | P1 | Frontend/API | Closed for browser UX; remote TLS/RBAC residual |
| R-06 | Static verification overstates success | High | High | P1 | Core | Open |
| R-07 | Missing live key validation | Medium baseline / Low-Medium current | Medium-High baseline / Medium current | P1 | Config/provider | Mostly closed |
| R-08 | Native dependency advisory warnings | Medium | Medium | P2 | Platform | Open |
| R-09 | Trace/state at-rest controls incomplete | Medium | Medium | P2 | Security/ops | Open |
| R-10 | Telemetry misses failed attempts as first-class rows | Medium | Medium | P2 | Observability | Partially closed: trace index wired |
| R-11 | No `/api/run` concurrency/spend limiter | Medium-High baseline / Medium current | Medium | P2 | Web/API | Partially closed: process semaphore only |
| R-12 | Documentation drift | Medium baseline / Low-Medium current | High baseline / Medium current | P2 | Docs | Mostly closed; untracked artifact unresolved |

## 14. Remediation Matrix

| Priority | Action | Findings | Effort | Validation |
|---|---|---|---|---|
| Done | Make ASK terminal-local in engine | F-01 | M | `cargo test`; ASK CLI smoke has no provider/tokens |
| Done | Separate context hits from solution hits | F-02 | M | Workspace implementation task routes IMPLEMENT |
| Done | Activate GitHub Actions in `.github/workflows` | F-03 | S | Source workflow exists; hosted branch protection still external |
| Done | Enforce fmt/clippy and fix current failures | F-04 | S | fmt/clippy exit 0 |
| Done | Add dashboard bearer token UX | F-05 | M | Frontend token modal + shared auth header injection |
| P1 | Strengthen verification semantics/cache admission | F-12 | L | Bad semantic outputs are not cached |
| Mostly done | Add semantic config validation | F-07 | M | Invalid live configs fail before run; route/policy validation remains |
| Done | Fix constant-time comparison implementation or docs | F-06 | S | No length early return |
| P2 | Add execution_attempts telemetry | F-09 | M | Failed attempts visible in SQL/API |
| P2 | Review native dependency strategy | F-10 | M-L | Audit warnings accepted or removed |
| P2 | Add trace retention and permission hardening | F-11 | M-L | Retention/permissions tested |
| Partly done | Add run concurrency/spend guardrails | F-13 | M-L | Process semaphore added; aggregate budgets/rate limits remain |
| Mostly done | Clean docs and untracked artifact governance | F-14 | S-M | Tracked docs updated; untracked `TokenOS Main Report.txt` unresolved |

## 15. Strategic Recommendations

### Short Term: 0-2 Weeks

1. Verify hosted GitHub branch protection requires `.github/workflows/ci.yml` checks.
2. Decide the disposition of `TokenOS Main Report.txt` because it remains untracked and provenance is unknown.
3. Add route-name and policy-range config validation tests to complete F-07.
4. Add an explicit concurrent `/api/run` integration test proving saturation returns `429`.
5. Rename or qualify user-facing "verified" language where static checks are not semantic proof.

### Medium Term: 2-6 Weeks

1. Add execution-attempt telemetry with provider/model/route/status/error/latency/cost fields.
2. Add trace retention controls, optional trace disablement, and documented file-permission guidance.
3. Implement semantic verification for at least route-specific invariants that static shape checks cannot cover.
4. Add CSP/security headers and a reverse-proxy/TLS deployment guide for remote dashboard use.
5. Formally risk-accept or remove native GTK3-family dependency warnings.

### Long Term: 6-12 Weeks

1. Build a real verification framework with route-specific validators and optional local test commands.
2. Introduce durable provider health and drift storage if routing decisions must survive process restarts.
3. Add budget governance: aggregate cost caps, per-token scopes, per-user rate limits, and token quotas.
4. Harden remote deployment story beyond single-process local serving.
5. Re-evaluate native app dependency chain and platform support strategy.

## 16. Evidence Appendix

### Repository Evidence

- `git status --short --branch`: `## main...origin/genspark_ai_developer`
- `git log -n 1`: `02c157b Merge pull request #4 from ...`
- Tracked files: Rust source, static assets, docs, active workflow, Cargo files.
- Untracked: `TokenOS Main Report.txt`.

### Toolchain and Build Evidence

Installed after user approval:

```text
rustc 1.96.0 (ac68faa20 2026-05-25)
cargo 1.96.0 (30a34c682 2026-05-25)
```

Checks:

```text
cargo test --locked
EXIT=0
186 passed; 0 failed

cargo build --release --locked
EXIT=0
Finished release profile in 1m 01s

cargo build --release --locked --features native
EXIT=0
Finished release profile in 1m 10s

cargo fmt --all -- --check
EXIT=0

cargo clippy --all-targets -- -D warnings
EXIT=0
```

### Supply-Chain Evidence

```text
cargo audit --json
EXIT=0
vulnerabilities_found=False
vulnerability_count=0
dependency_count=409
unmaintained_count=11
unsound_count=1
```

### CLI Smoke Evidence

```text
tokenos route "fix typo in README"
route DIRECT
chain mock

tokenos run "say hello" --dry-run --json
route IMPLEMENT
provider mock
success true
cost_usd 0.0

tokenos providers
mock enabled; openai/anthropic/gemini disabled by default
```

### Baseline Defect Reproduction and Remediation Evidence

Baseline ASK provider call:

```text
tokenos route "maybe somehow do something with the thing"
route ASK

tokenos run "maybe somehow do something with the thing" --dry-run --json
route ASK
provider mock
tokens_in 123
tokens_out 29
```

Remediated ASK smoke:

```text
tokenos run "maybe somehow do something with the thing" --dry-run --json
route ASK
provider/model omitted
tokens_in 0
tokens_out 0
cost_usd 0.0
verified.pass true
```

Baseline workspace REUSE misroute:

```text
tokenos index . --query "tokenizer truncate"
indexed 250 symbols
found src\tokenizer.rs truncate

tokenos route "fix tokenizer truncate bug" --workspace .
route REUSE

tokenos route "implement new webui auth token prompt" --workspace .
route REUSE
```

Remediated workspace route smoke:

```text
tokenos route "implement new webui auth token prompt" --workspace . --dry-run
indexed 265 symbols from .
route IMPLEMENT
chain mock
```

## 17. Assumptions and Unknowns

### Assumptions

- "Owner" in inventories is inferred as core maintainer because no CODEOWNERS or ownership metadata was found.
- Local smoke tests are representative of default Windows behavior after Rust installation.
- RustSec warnings in optional native dependencies are relevant to native release risk even if default non-native builds do not exercise those code paths.
- Provider live compatibility was not asserted because no API keys or external live calls were used.

### Unknowns

- Whether GitHub branch protections exist.
- Whether releases are published manually outside the active workflow.
- Whether provider model IDs and prices are current as of 2026-06-12.
- Whether users deploy TokenOS remotely today.
- Whether `TokenOS Main Report.txt` is user-authored, generated, obsolete, or intended for commit.
- Whether production data has ever been stored in local DB/trace paths.
- Whether native app behavior is acceptable on macOS/Linux; only Windows build was verified.

## 18. Final Verdict

TokenOS is technically coherent as a local Rust execution-kernel prototype and is materially stronger after this remediation pass. The two most serious route-contract defects are closed: ASK now terminates locally at zero provider cost, and workspace context no longer implies REUSE. CI is active in the repository, fmt/clippy/tests/builds are green, browser bearer-token UX exists, trace events are indexed, placeholder-bearing cache replay is blocked, and `/api/run` has process-local backpressure.

The remaining risks are now governance and productionization risks rather than basic correctness contradictions: semantic verification remains shallow, native dependencies carry RustSec informational warnings, local traces and SQLite state lack retention/encryption/permission controls, and remote serving still needs TLS, RBAC/scopes, aggregate budgets, and hosted branch-protection verification. TokenOS can credibly be positioned as a local-first, single-user execution kernel with strong deterministic controls. It should not yet be positioned as a production-grade, multi-tenant token-optimization platform without the remaining controls.
