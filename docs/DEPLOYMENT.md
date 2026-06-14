# TokenOS Deployment Guide

TokenOS deploys as a local-first Rust kernel, CLI, native desktop app, and
embeddable library. The retired browser dashboard and HTTP API are not shipped
in the active product, so there is no built-in web service to reverse proxy or
bind publicly.

## 1. Local CLI Install

```sh
cargo build --release --locked
install -m 0755 target/release/tokenos /usr/local/bin/tokenos
tokenos config init
tokenos doctor
```

Set provider keys in the launch environment, not in config files:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
export OPENAI_API_KEY=sk-proj-...
export GEMINI_API_KEY=...
```

## 2. Native Desktop Install

```sh
cargo build --release --locked --features native
./target/release/tokenos app --dry-run
```

The native app uses direct engine and SQLite calls. It does not start a web
server, bind a loopback port, open a browser, or embed a webview.

Use the native Readiness panel before live use. It checks local SQLite health,
provider key presence, spend ceilings, trace policy, verified-cache policy, and
the retired HTTP boundary.

## 3. State And Trace Paths

Defaults:

| Artifact | Default |
|---|---|
| Config | `~/.config/tokenos/config.yaml` |
| State DB | `~/.local/share/tokenos/tokenos.db` |
| Traces | `~/.local/state/tokenos/traces` |

Override paths when running under a service account or controlled workspace:

```sh
export TOKENOS_CONFIG=/etc/tokenos/config.yaml
export TOKENOS_DB=/var/lib/tokenos/tokenos.db
export TOKENOS_TRACES=/var/lib/tokenos/traces
```

Protect state and traces with owner-only permissions. Trace blobs are masked on
the outbound side but may contain sensitive business context.

## 4. Scheduled Or Headless CLI Use

For batch jobs, invoke CLI commands directly from the scheduler. Example
systemd unit for a one-shot dry-run health check:

```ini
[Unit]
Description=TokenOS local health check

[Service]
Type=oneshot
User=tokenos
Group=tokenos
EnvironmentFile=/etc/tokenos/tokenos.env
ExecStart=/usr/local/bin/tokenos doctor --db /var/lib/tokenos/tokenos.db --traces /var/lib/tokenos/traces
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/tokenos
PrivateTmp=yes
```

For scheduled task execution, prefer explicit command lines and constrained
working directories:

```sh
tokenos run "summarize yesterday's local build logs" \
  --workspace /srv/project \
  --db /var/lib/tokenos/tokenos.db \
  --traces /var/lib/tokenos/traces
```

## 5. Live Provider Staging

Before production spend:

1. Run `tokenos providers` to verify provider enablement and model filters.
2. Set `policy.max_cost_per_task_usd` or global daily/monthly spend limits.
3. Run `tokenos route "<task>"` before paid execution.
4. Execute a small live task in staging.
5. Inspect `tokenos attempts`, `tokenos telemetry`, and `tokenos trace <task-id>`.

## 6. Library Embedding

Hosts that need a remote API should embed the Rust library and implement their
own transport and authorization layer. TokenOS intentionally does not ship a
general-purpose public control plane in the active app.

Embedding owners are responsible for:

- authentication and authorization;
- TLS and certificate operations;
- request rate limits and abuse controls;
- multi-tenant isolation;
- monitoring, access logs, backups, restore tests, and incident response.

## 7. Backup And Retention

Back up the SQLite database if task state, attempt history, telemetry, or
verified-cache entries matter operationally. Back up traces when forensic
diagnostics matter, and prune according to `security.retention_days`.

Use OS disk encryption or an encrypted volume for deployments requiring
encryption at rest.
