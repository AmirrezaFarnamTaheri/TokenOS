# TokenOS Production Readiness

This document is the maintained release boundary for TokenOS. It replaces
retired audit and engineering-report artifacts with a concise operational
record: what the local source tree closes, what operators must still provide,
and which gates define a releasable build.

## Finalized Artifact Scope

TokenOS is finalized as a local-first Rust execution kernel:

- CLI binary and embeddable library crate.
- Embedded Axum dashboard and JSON API.
- Optional native launcher that opens the loopback dashboard in the system
  browser without a webview/GTK dependency chain.
- Local SQLite state store and content-addressed flight recorder.
- Provider adapters for mock, OpenAI, Anthropic, Gemini, and proxy endpoints.

TokenOS is not a multi-tenant SaaS platform, a complete identity provider, a
patch-application engine, a screen-control agent, or a fleet governance plane.
Those boundaries are deliberate. Remote, multi-user, and regulated deployments
must add the controls listed in [RISK_ACCEPTANCE.md](RISK_ACCEPTANCE.md).

## Closed Local Controls

| Area | Local closure |
|---|---|
| Routing correctness | `ASK` terminates locally with one question and zero provider cost. `REUSE` requires an exact verified solution-cache hit, not merely workspace context. |
| Build governance | Active CI runs formatting, clippy, audit, release build, tests, native builds, and a documentation drift guard. |
| Provider safety | Live adapters fail if required key environment variables are missing. Gemini keys travel in `X-Goog-Api-Key`, never in query strings. |
| Cost control | Conservative token budgeting, shadow pricing, per-task budget sentinel, daily/monthly spend limits, and process-local `/api/run` backpressure are implemented. |
| API protection | Non-loopback bind requires `--public` and a non-empty bearer token. API token comparison is constant-time; scoped tokens and a shared SQLite per-token request ledger are available. |
| Transport | Native HTTPS is available with `--tls-cert` and `--tls-key`; reverse proxies remain supported for managed deployments. |
| Traceability | Recorder events are indexed in SQLite, provider attempts are first-class rows and aggregates exposed by CLI/API/dashboard, startup provider health replays attempt rows, corrupt telemetry reads fail visibly, and API request telemetry is aggregate-only. |
| Data minimization | Prompts are masked before provider calls; unmasked output is returned only at the caller boundary. Placeholder-bearing outputs are not replayed from the solution cache. |
| Storage hygiene | Trace disablement, retention pruning, and Unix owner-only permissions are implemented for state and recorder artifacts. |
| Supply chain | The native launcher avoids the prior GTK/webview dependency path; `cargo audit` is part of the required gate. |
| Licensing | The repository uses `AGPL-3.0-only` and ships the full AGPLv3 license text in `LICENSE`. |

## Release Gates

Run these from the repository root before cutting or distributing a build:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo audit
cargo build --release --locked
cargo build --release --locked --features native
```

Expected current local result: all gates pass; the unit test suite contains
234 tests. If the count changes, update the README and this document in the
same change as the tests.

## Smoke Gates

These quick checks cover the highest-value end-to-end paths:

```sh
tokenos route "fix typo in README" --dry-run
tokenos run "maybe somehow do something with the thing" --dry-run --json
tokenos run "say hello" --dry-run --json
tokenos providers
tokenos doctor
```

Expected behavior:

- Route preview performs no provider call.
- Ambiguous tasks route to `ASK`, return one local question, and report zero
  tokens and zero cost.
- Dry-run execution succeeds against the mock provider.
- Live providers are disabled unless explicitly configured with valid env-var
  names and keys.
- Doctor reports SQLite `quick_check=ok` and does not call providers.

## Component Readiness Matrix

| Component | Readiness | Notes |
|---|---|---|
| `kernel` | Production-local | Pure deterministic route ladder; no I/O or provider calls. |
| `engine` | Production-local | Highest blast radius; all route, cache, failover, verification, and persistence changes require focused tests. |
| `provider` | Production-local with staging requirement | Mock path is fully offline; live provider contracts require credentialed staging checks before production spend. |
| `pricing` | Production-local | Bandit and latency/failure learning are process-local; drift ratios persist for observability. |
| `payload` | Production-local | Static-first prompt contract, context distillation, and solution extraction are covered by tests. |
| `verify` | Production-local | Static checks always run; configured verification commands define semantic strength for code tasks. |
| `store` | Production-local | SQLite is transactional and bundled; application-level encryption is deployment-specific. |
| `recorder` | Production-local | Content-addressed traces are useful diagnostics and should be protected as application logs. |
| `webui` | Production-local with deployment controls | Loopback default is safe for local use; remote access requires TLS, bearer auth, and scoped tokens. |
| `static` | Production-local | No frontend build step, CDN, or third-party script dependency. |
| `.github/workflows/ci.yml` | Source-ready | Hosted branch protection must still be enforced by repository administration. |

## Documentation Set

The maintained production documentation lives in `README.md` and `docs/`.
Root-level audit reports and engineering bundles are retired so there is one
coherent source of truth:

- Architecture and invariants: [ARCHITECTURE.md](ARCHITECTURE.md)
- Operator deployment: [DEPLOYMENT.md](DEPLOYMENT.md)
- Security model: [SECURITY.md](SECURITY.md)
- External controls and accepted risks: [RISK_ACCEPTANCE.md](RISK_ACCEPTANCE.md)
- API contract: [API.md](API.md)
- CLI contract: [CLI.md](CLI.md)
- Configuration: [CONFIGURATION.md](CONFIGURATION.md)
- Troubleshooting: [TROUBLESHOOTING.md](TROUBLESHOOTING.md)
- Contribution rules: [CONTRIBUTING.md](CONTRIBUTING.md)

## Operator-Owned Controls

These controls cannot be proven from a local checkout and must be verified by
the deployment owner:

- GitHub branch protection or repository rulesets requiring CI.
- Live OpenAI, Anthropic, Gemini, and proxy adapter compatibility under real
  credentials and spend limits.
- TLS certificate issuance, rotation, HSTS policy, and access logging.
- OS disk encryption or application-level database encryption if required by
  the environment.
- Fleet-wide quota governance for independent hosts, databases, or regions.
- Monitoring, alerting, backup, restore, and incident-response procedures.

TokenOS can be deployed without these only as a local, single-user execution
kernel. Treat any broader deployment as an operations project with explicit
acceptance of the external controls above.
