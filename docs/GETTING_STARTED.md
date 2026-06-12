# Getting Started with TokenOS

This guide takes you from a fresh clone to a running dashboard in about five
minutes — entirely offline first, then with live providers.

## Prerequisites

- **Rust ≥ 1.75** (`rustup` recommended). That's it — SQLite is bundled, the
  dashboard has zero frontend dependencies, and there are no system
  libraries to install.

## 1. Build

```sh
git clone https://github.com/AmirrezaFarnamTaheri/TokenOS.git
cd TokenOS
cargo build --release
# binary: target/release/tokenos
```

Optionally run the test suite (a few seconds, fully offline):

```sh
cargo test
```

## 2. First run — completely offline

TokenOS ships with a fault-injectable **mock provider**, so you can exercise
the entire pipeline without an API key or network access:

```sh
# Write the default config (~/.config/tokenos/config.yaml)
./target/release/tokenos config init

# Preview a routing decision — deterministic, free, no LLM involved
./target/release/tokenos route "fix typo in README"

# Execute through the full pipeline against the mock
./target/release/tokenos run "say hello" --dry-run
```

`route` shows you the kernel's decision ladder output: the chosen route, the
reason, extracted signals, token estimates, and the provider failover chain.
Nothing is spent until you `run` without `--dry-run` against a live provider.

## 3. Inspect what happened

```sh
./target/release/tokenos tasks                 # persisted task states
./target/release/tokenos trace <task-id>       # flight-recorder timeline
./target/release/tokenos telemetry             # cost-per-success + bandit standings
```

Every decision, prompt, and response was recorded out-of-band — debugging
never consumes context tokens.

## 4. Launch the dashboard

```sh
./target/release/tokenos serve --port 8080 --dry-run
# open http://127.0.0.1:8080
```

The control panel gives you:

- **Dashboard** — cost-per-success KPI, route distribution, provider health,
  live UCB1 bandit standings
- **Run Console** — free route preview, then execute
  (keyboard: `Ctrl+Enter` to execute, `Ctrl+Shift+Enter` to preview,
  keys `1`–`5` switch views)
- **Tasks / Executions** — persisted state and the full telemetry ledger
- **Configuration** — read-only effective config

If you start the dashboard with `--auth-token` or expose it with `--public`,
click **API token** in the sidebar and enter the bearer token. The dashboard
then attaches `Authorization: Bearer ...` to every API request. Tokens are
kept in memory unless you explicitly remember them for the current browser tab.

## 5. Connect a live provider

1. Export the key (env vars only — keys never touch disk):

   ```sh
   export ANTHROPIC_API_KEY=sk-ant-...
   ```

2. Enable the provider in `~/.config/tokenos/config.yaml`:

   ```yaml
   providers:
     anthropic:
       disabled: false        # flip this
   ```

3. Verify the filter matrix admits the models you expect:

   ```sh
   ./target/release/tokenos providers
   ```

4. Run for real:

   ```sh
   ./target/release/tokenos run "summarize the routing module" --workspace .
   ```

`--workspace .` builds a structural symbol index over your codebase so the
prompt carries the *minimum viable context* (≤ 2000 tokens) instead of whole
files.

## 6. Good habits

- **Preview before paying.** `tokenos route "<task>"` is always free and
  tells you exactly what a run would do.
- **Watch cost-per-success.** `tokenos telemetry` surfaces the headline
  metric the whole kernel optimizes.
- **Use constraints.** `--constraints "a; b; c"` feeds the verifier and the
  payload builder; repeated failures on a goal automatically forbid the
  failed approach.
- **Keep the dashboard local.** If you must expose it, set
  `TOKENOS_AUTH_TOKEN`, enter that token in the dashboard's API-token dialog,
  and read [SECURITY.md](SECURITY.md) first.

## Next steps

- [CLI.md](CLI.md) — every command and flag
- [CONFIGURATION.md](CONFIGURATION.md) — every config field
- [ARCHITECTURE.md](ARCHITECTURE.md) — how the kernel works inside
- [PRODUCTION_READINESS.md](PRODUCTION_READINESS.md) — release gates and deployment boundary
- [SECURITY.md](SECURITY.md) — safe local and remote operation
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) — when something looks wrong
