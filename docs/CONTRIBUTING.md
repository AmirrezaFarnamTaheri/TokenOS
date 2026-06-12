# Contributing to TokenOS

Thanks for considering a contribution. TokenOS is small on purpose; the bar
for new code is that it observably reduces **cost per successful task** or
hardens an existing guarantee.

## Ground rules

1. **Determinism is sacred.** Same inputs ⇒ same route, same provider order,
   same payload bytes. Anything that introduces nondeterminism into the
   decision path (random tie-breaks, time-dependent routing, hash-map
   iteration order) will be rejected. The UCB1 bandit is the one sanctioned
   source of run-time adaptivity, and even it defaults to neutral.
2. **Zero tokens for decisions.** Routing, verification, loop detection,
   context selection, and provider ordering must never call a model. If your
   feature needs an LLM to decide something, it belongs in the payload, not
   the kernel.
3. **Free checks before paid checks.** Any verification you add must run
   before money is spent, or demonstrably pay for itself.
4. **No new runtime dependencies without discussion.** The dashboard is
   dependency-free by design (embedded assets, no CDN); SQLite is bundled.
   Open an issue before adding a crate.

## Development setup

```sh
git clone https://github.com/AmirrezaFarnamTaheri/TokenOS.git
cd TokenOS
cargo build        # must finish with zero warnings
cargo test         # must be green — the suite is fully offline
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

The crate is a **library + thin binary**: put logic in the library modules
(`src/lib.rs` exports them all) and keep `src/main.rs` a pure CLI consumer.

## Testing conventions

- Every module owns its unit tests in a `#[cfg(test)] mod tests` block.
- Engine-level behavior is tested through the **mock adapter** —
  fault-injectable (`fail_every_n`), latency-shapeable, and able to return
  canned outputs. Use it rather than network mocks:

  ```rust
  e.adapters.write().unwrap().insert("__dryrun__", Adapter::Mock { canned: Some("...".into()), ..Default::default() });
  ```

- Web endpoints are tested with `tower::ServiceExt::oneshot` against the
  axum router — no port binding in tests.
- If you change ordering/pricing logic, keep or extend the property-style
  tests that compare against the previous oracle (see
  `unexplored_bandit_preserves_shadow_priced_order`).
- Use in-memory SQLite (`:memory:`) and per-process temp dirs for recorder
  state so tests are parallel-safe.

## Code style

- `rustfmt` defaults. No warnings — the build must be silent.
- Public items get doc comments that state the *invariant*, not just the
  behavior.
- Error handling: no `unwrap()` on fallible paths outside tests; classify
  provider errors (retryable vs terminal) rather than stringly matching.
- Lock-free where the hot path demands it (`AtomicF64` CAS patterns in
  `pricing.rs` are the reference); otherwise prefer the simplest correct
  synchronization.

## Frontend (static/)

- Plain HTML/CSS/JS — no frameworks, no build step, no external requests.
- Assets are embedded at compile time (`include_str!` in `webui.rs`), so a
  `cargo build` is required for frontend changes to take effect.
- All dynamic text must go through the `esc()` helper (XSS).
- Keep it keyboard-accessible: views are reachable via keys `1`–`5`,
  console actions via `Ctrl+Enter` / `Ctrl+Shift+Enter`.

## Pull request checklist

- [ ] `cargo build` — zero warnings
- [ ] `cargo test --locked` — green, no network access required
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo audit` — no known vulnerability findings
- [ ] New behavior covered by tests (including at least one failure-path test)
- [ ] Docs updated (`README.md` + relevant file in `docs/`)
- [ ] Commit messages follow `type(scope): description`
      (e.g. `feat(pricing): quota-pressure decay`, `fix(jsonrescue): EOF guard`)
- [ ] One squashed, comprehensive commit per logical change

## Reporting bugs

Best bug reports include the free, deterministic reproduction:

```sh
tokenos route "<task>"          # the decision, signals, and chain
tokenos trace <task-id>         # what actually happened
```

Security issues: please use a private GitHub security advisory instead of a
public issue — see [SECURITY.md](SECURITY.md).
