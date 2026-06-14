# TokenOS Troubleshooting Guide

Symptoms, causes, and fixes.

## Build Issues

### `error: package ... requires rustc 1.75 or newer`

Update Rust:

```sh
rustup update stable
```

### Linker errors mentioning `sqlite3`

SQLite is bundled through `rusqlite`. If this appears, clean stale artifacts:

```sh
cargo clean
cargo build
```

### `tokenos app` is unavailable

The native app is feature-gated:

```sh
cargo build --release --features native
./target/release/tokenos app --dry-run
```

Default builds still include the CLI and library.

## Configuration Issues

### Config not found or defaults are unexpected

Resolution order is `--config` flag, `$TOKENOS_CONFIG`, then
`~/.config/tokenos/config.yaml`.

```sh
tokenos config
```

### Provider never gets selected

Check:

1. `disabled: false` in the provider profile.
2. The env var named by `api_key_env` is set in the launching shell.
3. `tokenos providers` admits the configured model.
4. The route appears in `routing` or survives shadow pricing.
5. The provider context window can fit the payload.

### `tokenos config init` says the file exists

`init` never overwrites. Move the old file or pass a different `--config`.

## Routing Surprises

### Everything routes to ASK

The confidence signal is below `policy.ask_threshold`, or the task lacks
critical information. ASK is local: one question, no provider, no tokens.

### DIRECT routes to IMPLEMENT

`DIRECT` requires the conservative estimate to fit
`policy.direct_max_tokens`. The estimator deliberately avoids undercounting.

### PATCH stopped being offered

Failure memory exists for that goal. Inspect the trace and attempts:

```sh
tokenos trace <task-id>
tokenos attempts --limit 20
```

### ESCALATE-EXTERNAL: semantic loop

Recent outputs for the same goal were too similar. Change the approach or
constraints rather than retrying the same prompt.

## Execution Issues

### `all providers failed`

Start with:

```sh
tokenos attempts --limit 20
tokenos telemetry
tokenos trace <task-id>
```

Common causes:

- missing or expired key;
- provider quota exhaustion;
- model filter excludes the configured model;
- provider context window too small;
- network egress blocked.

Use `--dry-run` to verify the local pipeline independently of provider access.

### Output looks like repaired JSON

The JSON rescuer only activates for JSON-intent tasks or constraints and only
accepts full-input repairs. Rescues are recorded as trace events.

### First provider call is slow

Initial connection setup and cold prompt caches are expected. Later calls use
the payload builder's stable static prefix and bandit latency learning.

## Native App Issues

### App opens but data is empty

Run:

```sh
tokenos doctor
tokenos telemetry
```

The app reads the same SQLite database as the CLI. If you pass `--db` or
`TOKENOS_DB` to one surface, pass the same path to the other.

### Readiness says live keys are missing

The provider is enabled but the env var named by `api_key_env` is not set in
the process that launched the app. Start the app from a shell with the key set,
or use your OS launch mechanism to inject environment variables.

### Readiness warns about spend ceiling

For live mode, configure at least one spend guard:

```yaml
policy:
  max_cost_per_task_usd: 0.05
security:
  daily_spend_limit_usd: 5.0
  monthly_spend_limit_usd: 50.0
```

### Native app does not expose an HTTP endpoint

Correct. The browser dashboard and HTTP API are retired. Use CLI commands,
native views, or library embedding.

## State And Storage

### Clean slate

Remove or redirect the state paths:

```sh
rm ~/.local/share/tokenos/tokenos.db
rm -r ~/.local/state/tokenos/traces
```

On Windows, delete the equivalent paths or set `$TOKENOS_DB` and
`$TOKENOS_TRACES` to fresh locations.

### Trace blobs are missing

Run:

```sh
tokenos trace <task-id> --blobs
```

If traces are disabled in config, future payload blobs will not be recorded.

## Still Stuck

Open an issue with:

1. `tokenos --version` and `rustc --version`;
2. `tokenos doctor --json`;
3. `tokenos route "<task>"`;
4. relevant `tokenos trace <task-id>` output with business content redacted.
