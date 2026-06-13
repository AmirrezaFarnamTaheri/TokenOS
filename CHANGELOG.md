# Changelog

All notable changes to TokenOS are documented here.
This project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased] — Correctness & operability hardening

This release closes a set of correctness, security, and operability gaps
surfaced by a full-codebase technical review. No public API changed.

### Fixed — correctness

- **Mock fallback can no longer poison durable state.** When the offline
  `mock` adapter serves a *live* (non-dry-run) execution — which only happens
  when every real provider is exhausted or filtered — its synthetic output is
  no longer admitted to the verified solution cache, and a clear warning is
  emitted to stderr and the flight recorder. Dry-run previews are unaffected.
  (`src/engine.rs`, `src/provider.rs::Adapter::is_mock`)
- **`cost_per_success` is now undefined (not `0.0`) when a route/instance has
  zero successful tasks.** A route that only ever failed used to report
  `$0.00` and rank as the *cheapest*, inverting the headline metric. It now
  returns a non-finite value that serializes to JSON `null` and renders as
  "—" in the dashboard. (`src/store.rs`)

### Added — safety & validation

- **Config validation rejects unknown adapter names** (e.g. `openi`) and
  **all-disabled provider sets** at load time instead of failing lazily inside
  the failover loop. (`src/config.rs`)
- **Flight-recorder blob lookups validate the SHA is hex** before building a
  filesystem path, preventing path traversal from a tampered journal.
  (`src/recorder.rs`)
- **Anthropic adapter omits the `x-api-key` header when no key is set**
  (mirroring the OpenAI adapter), yielding a clean upstream 401 instead of a
  malformed request. (`src/provider.rs`)
- Documented the single `unsafe` block in the JSON rescuer with an explicit
  `// SAFETY:` invariant. (`src/jsonrescue.rs`)

### Changed — CI / supply chain

- **Clippy is now a blocking gate** (`-D warnings`) instead of advisory, and a
  new **`security-audit` job runs `cargo audit` + `cargo deny`** on every push
  and PR, backed by a new `deny.toml` (permissive-license allowlist, advisory
  DB, source/ban policy). `cargo fmt --check` remains advisory pending a
  one-time `cargo fmt --all`. CI still lives in `.github/workflows-staged/`
  and is activated by a maintainer with `workflows` permission (see the
  staged README). (`.github/workflows-staged/ci.yml`, `deny.toml`)

### Tests

- Added regression coverage: live-mock output is not cached; `cost_per_success`
  is non-finite without a success and serializes to `null`; unknown-adapter and
  all-disabled configs are rejected; `Adapter::is_mock` discriminates adapters;
  recorder blob lookups reject non-hex / traversal SHAs.
