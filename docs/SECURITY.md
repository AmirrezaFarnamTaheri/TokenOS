# TokenOS Security Model

This document describes the active threat model, local hardening measures, and
operator responsibilities for TokenOS.

## Threat Model

TokenOS sits between a local workspace and third-party LLM providers. The
kernel treats these surfaces as hostile:

1. outbound network paths, provider logs, and proxies;
2. model output, including malformed or adversarial-shaped text;
3. local files and traces that may contain sensitive business content.

The retired browser dashboard and HTTP API are not part of the active product.
TokenOS does not ship an inbound listener, public web endpoint, browser
extension, screen-control layer, or transparent raw-chat proxy.

## 1. Secret Protection

### Edge Secret Masking

Every outbound prompt is scanned before network egress for:

- API keys and bearer tokens;
- private-key PEM blocks;
- passwords and connection strings;
- email addresses and IP addresses.

Matches are replaced with stable placeholders. If the model echoes a
placeholder, the response leg restores the original value at the caller
boundary. The reverse vault lives only in the request stack frame and is never
persisted or shared.

### API Keys

- Keys are read only from environment variables named by config.
- Config files store env-var names, not secret values.
- Gemini authenticates with `X-Goog-Api-Key`, never a query-string key.
- Provider requests leave the process only through the adapter layer after
  routing, payload construction, and masking.

## 2. Native App Boundary

`tokenos app` is a native egui/eframe application:

- direct `Arc<Engine>` calls for preview and execution;
- direct SQLite reads for telemetry and health;
- no HTTP server;
- no loopback bind;
- no browser launch;
- no webview shell.

This removes the former inbound web API threat class from the shipped UI.
Programmatic integrations should embed the library crate and provide their own
transport, identity, authorization, and audit boundary.

## 3. Parser And Algorithm Safety

- Routing and masking regexes use Rust `regex`, which is linear-time and does
  not support catastrophic backtracking.
- Loop detection caps inputs before edit-distance comparison and uses Myers'
  bit-parallel algorithm.
- The JSON rescuer accepts repairs only when parsing consumes the full input;
  prose that merely starts with JSON punctuation is returned untouched.
- SQLite access uses prepared statements with bound parameters.

## 4. Data At Rest

| Artifact | Default location | Contents |
|---|---|---|
| State DB | `~/.local/share/tokenos/tokenos.db` | Task states, execution telemetry, provider attempts, aggregates, failure memory, loop windows, trace metadata, solution cache |
| Flight recorder | `~/.local/state/tokenos/traces` | NDJSON journals and SHA-256 content-addressed payload blobs |
| Config | `~/.config/tokenos/config.yaml` | Profiles and policy, never key values |

Flight-recorder blobs contain masked prompts/responses but may still include
sensitive business context. Protect traces and the state database as
application data. Use OS disk encryption or a deployment-specific encrypted
storage layer when required.

The verified solution cache admits only replayable verified outputs. Outputs
that still contain opaque secret placeholders are not cached.

## 5. Supply Chain

- SQLite is bundled through `rusqlite`.
- The native UI uses egui/eframe and avoids webview/browser embedding.
- The retired Axum browser-control module and explicit web-control
  dependencies have been removed from the active manifest. Provider adapters
  still use `reqwest` for outbound network calls.
- `cargo audit` is part of the release gate.

## 6. Operational Checklist

- [ ] Keep provider keys in environment variables managed by a secret store or
      secured shell environment.
- [ ] Protect `$TOKENOS_DB` and `$TOKENOS_TRACES` with owner-only permissions
      in shared environments.
- [ ] Run `tokenos providers` after config changes to confirm model filters.
- [ ] Run `tokenos doctor` before live use to verify local store health.
- [ ] Use `tokenos route` or the native Run Console preview before paid runs.
- [ ] Stage live provider compatibility and spend limits before production use.
- [ ] Add monitoring, backups, restore testing, and incident procedures for any
      operational deployment.

## Reporting

Report security issues through a private GitHub security advisory rather than a
public issue.
