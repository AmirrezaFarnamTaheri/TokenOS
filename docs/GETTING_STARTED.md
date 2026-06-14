# Getting Started With TokenOS

This guide takes you from a fresh clone to an offline run, native desktop app,
and optional live provider execution.

## Prerequisites

- Rust 1.75 or newer (`rustup` recommended).
- No system SQLite install is required; SQLite is bundled.

## 1. Build

```sh
git clone https://github.com/AmirrezaFarnamTaheri/TokenOS.git
cd TokenOS
cargo build --release
```

Optional native desktop build:

```sh
cargo build --release --features native
```

## 2. First Run - Completely Offline

TokenOS ships with a fault-injectable mock provider, so the full pipeline can
run without an API key or network access.

```sh
./target/release/tokenos config init
./target/release/tokenos route "fix typo in README"
./target/release/tokenos run "say hello" --dry-run
```

`route` shows the selected route, reason, confidence, estimated tokens, and
provider chain. Nothing is spent until a live provider is enabled and `run` is
used without `--dry-run`.

## 3. Inspect What Happened

```sh
./target/release/tokenos tasks
./target/release/tokenos trace <task-id>
./target/release/tokenos attempts
./target/release/tokenos telemetry
./target/release/tokenos doctor
```

Every decision, prompt, response, rescue, verification event, and error is
recorded out-of-band so debugging does not consume context tokens.

## 4. Launch The Native Desktop App

```sh
cargo build --release --features native
./target/release/tokenos app --dry-run
```

The native app is an egui/eframe desktop application with direct engine and
SQLite integration. It includes dashboard telemetry, route preview and
execution, provider cost forecasts, bulk route planning, policy simulation,
route calibration, operational stats, task traces, executions, provider
attempts, and configuration views.

The retired browser dashboard and HTTP API are not part of the active product.
No loopback listener or browser tab is started by `tokenos app`.

## 5. Connect A Live Provider

1. Export the provider key:

   ```sh
   export ANTHROPIC_API_KEY=sk-ant-...
   ```

2. Enable the provider in `~/.config/tokenos/config.yaml`:

   ```yaml
   providers:
     anthropic:
       disabled: false
   ```

3. Verify model filters:

   ```sh
   ./target/release/tokenos providers
   ```

4. Run for real:

   ```sh
   ./target/release/tokenos run "summarize the routing module" --workspace .
   ```

`--workspace .` builds a structural symbol index so prompts carry minimum
viable context instead of whole files.

## 6. Good Habits

- Preview before paying with `tokenos route "<task>"`.
- Watch cost per successful task with `tokenos telemetry` or the native app.
- Use constraints; they feed the verifier and payload builder.
- Keep provider keys in environment variables managed by your shell or secret
  store.
- Protect `$TOKENOS_DB` and `$TOKENOS_TRACES` as application data.

## Next Steps

- [CLI.md](CLI.md) - every command and flag
- [CONFIGURATION.md](CONFIGURATION.md) - every config field
- [ARCHITECTURE.md](ARCHITECTURE.md) - how the kernel works inside
- [PRODUCTION_READINESS.md](PRODUCTION_READINESS.md) - release gates
- [SECURITY.md](SECURITY.md) - safe operation
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) - when something looks wrong
