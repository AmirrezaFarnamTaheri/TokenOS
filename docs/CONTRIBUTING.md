# Contributing to TokenOS

TokenOS is small on purpose. New code should reduce cost per successful task,
strengthen determinism, improve native operations, or harden an existing
guarantee.

## Ground Rules

1. **Determinism is sacred.** Same inputs must produce the same route, baseline
   provider order, and payload bytes. The UCB1 bandit is the sanctioned runtime
   adaptation path.
2. **Zero tokens for decisions.** Routing, verification, loop detection,
   context selection, and provider ordering must not call a model.
3. **Free checks before paid checks.** Verification should run before spend, or
   clearly pay for itself.
4. **Native-first UI.** The active UI is `src/nativeapp.rs` using egui/eframe.
   Do not reintroduce a web dashboard, webview shell, or HTTP control plane.
5. **No new runtime dependencies without justification.** SQLite is bundled;
   dependency additions must buy clear reliability, security, or product value.

## Development Setup

```sh
git clone https://github.com/AmirrezaFarnamTaheri/TokenOS.git
cd TokenOS
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

Build the native app when touching native UI:

```sh
cargo build --features native
cargo test --features native
```

## Testing Conventions

- Keep module tests in `#[cfg(test)] mod tests`.
- Engine behavior should use the mock adapter rather than live network mocks.
- Use in-memory SQLite (`:memory:`) and per-process temp dirs for recorder
  state.
- Ordering/pricing changes should preserve or extend property-style tests such
  as `unexplored_bandit_preserves_shadow_priced_order`.
- Native UI helpers should have focused pure-function tests where possible; app
  launch remains a manual smoke gate.

## Code Style

- `rustfmt` defaults.
- No warnings.
- Public items document invariants, not obvious mechanics.
- No `unwrap()` on fallible production paths.
- Classify provider errors instead of stringly matching.
- Use lock-free primitives only where the hot path justifies them.

## Native UI Standards

- Preserve direct engine/store integration.
- Keep panels operationally useful: preview before spend, explain provider
  choice, expose budgets, show failures, and make readiness visible.
- Avoid cosmetic-only changes unless they support clarity, density, or
  operator confidence.
- Do not add browser dependencies, external assets, CDNs, webviews, or hidden
  local servers.

## Pull Request Checklist

- [ ] `cargo build` passes with zero warnings.
- [ ] `cargo test --locked` passes.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --all-targets -- -D warnings` passes.
- [ ] `cargo audit` has no unresolved vulnerability findings.
- [ ] `cargo build --release --locked --features native` passes when native UI changes.
- [ ] New behavior is covered by tests or documented as a manual native smoke gate.
- [ ] README and relevant docs are updated.
- [ ] One logical change per commit.

## Reporting Bugs

Good reports include:

```sh
tokenos route "<task>"
tokenos trace <task-id>
tokenos attempts --limit 20
tokenos doctor
```

Report security issues through a private GitHub security advisory.
