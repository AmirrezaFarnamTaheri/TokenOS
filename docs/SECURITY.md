# TokenOS Security Model

This document describes the threat model, the concrete hardening measures in
the codebase, and the operational guidance for running TokenOS safely.

## Threat model

TokenOS sits between your workspace (potentially containing secrets) and
third-party LLM providers (untrusted networks, logged requests). The kernel
treats three surfaces as hostile:

1. **The outbound network path** — request URLs, proxies, provider logs.
2. **The inbound web API** — anyone who can reach the dashboard port.
3. **Model output** — unbounded, adversarial-shaped text fed into parsers.

## 1. Secret protection

### Edge secret masking (`maskcodec`)

Every outbound prompt is scanned **before any byte leaves the process** for:

- API keys and bearer tokens (provider-specific and generic patterns)
- Private-key PEM blocks
- Passwords and connection strings
- Email addresses and IP addresses

Matches are replaced with stable placeholders; if the model echoes a
placeholder, the response leg restores the original value. Properties:

- The reverse vault (placeholder → secret) lives **only in the request's
  stack frame** — it is never persisted, logged, or shared across requests.
- Benign IP-like strings (e.g. version numbers) use a U+2024 sentinel
  scan-past technique; a guard disables the sentinel trick entirely when the
  input already contains U+2024, so pre-existing characters survive
  masking verbatim.

### API keys

- Keys are read **only** from environment variables (`api_key_env` names the
  variable; the value never enters the config file).
- `GET /api/config` returns the env-var *name*, never the value.
- **No API keys in URLs**: the Gemini adapter authenticates via the
  `X-Goog-Api-Key` request header, never the query string, so keys cannot
  leak into access logs, proxies, or tracing systems.

## 2. Web API hardening

### Bind policy

- Default bind is loopback (`127.0.0.1`).
- Binding a non-loopback interface requires the explicit `--public` flag
  **and** is refused unless a bearer token is configured. You cannot
  accidentally expose an unauthenticated dashboard.

### Bearer authentication

- When a token is set (`--auth-token` / `$TOKENOS_AUTH_TOKEN`), every
  `/api/*` request must present `Authorization: Bearer <token>`.
- Comparison is **constant-time** — equality is computed over all bytes
  regardless of where a mismatch occurs, closing the timing side channel.
- Static assets bypass auth (they contain no data); every data endpoint
  enforces it.

### Request handling

- Request bodies are strictly typed; malformed JSON returns `400` with a
  descriptive error, never a panic.
- `/api/run` is wrapped in a server-side timeout (`504` on expiry) so a hung
  provider cannot pin a connection forever.
- Handlers are lock-free (`Arc<Engine>`, no global mutex) — a slow execution
  cannot be used to starve health/telemetry endpoints.

## 3. Parser and algorithm safety

### ReDoS immunity

All routing and masking regexes use the Rust `regex` crate, which compiles
to finite automata with **linear-time matching** — catastrophic backtracking
is impossible by construction, no matter what the model emits.

### Bounded Levenshtein

Loop-detection comparisons cap input at 20k characters, keeping the
quadratic-worst-case pass CPU-bounded even on enormous generations. The
inner loop is Myers' bit-parallel algorithm (Hyyrö 2003 multiword), ~64×
faster than the naive DP.

### JSON rescuer guard

The truncated-JSON rescuer is a single-pass lenient parser with an
EOF-consumption guard: it only accepts a repair if parsing consumed the
entire input. Prose that merely *starts* with a bracket is returned
untouched — adversarial output cannot trick the rescuer into fabricating
structured data from non-JSON.

### SQL

All SQLite access goes through `rusqlite` prepared statements with bound
parameters — no string-built SQL anywhere in the codebase.

## 4. Data at rest

| Artifact | Location | Contents |
|---|---|---|
| State DB | `~/.local/share/tokenos/tokenos.db` | Task states, telemetry, failure memory, loop windows |
| Flight recorder | `~/.local/state/tokenos/traces` | NDJSON event journals + SHA-256 content-addressed payload blobs |
| Config | `~/.config/tokenos/config.yaml` | Profiles and policy — **never keys** |

Note that flight-recorder blobs contain full prompts/responses (post-masking
on the outbound side). Treat the traces directory with the same sensitivity
as application logs: secrets are masked, but business content is present.
Set `$TOKENOS_TRACES` to a suitably protected path in shared environments.

## 5. Supply chain

- SQLite is **bundled** (`rusqlite` with the `bundled` feature) — no system
  library version skew.
- The dashboard has **zero frontend dependencies** — no CDN, no npm, no
  third-party scripts. All assets are embedded in the binary at compile time
  (`include_str!`), so the served UI is exactly what was reviewed at build
  time.

## 6. Operational checklist

- [ ] Run with the default loopback bind unless remote access is required.
- [ ] If exposing remotely: set a strong `$TOKENOS_AUTH_TOKEN`, use
      `--public` deliberately, and terminate TLS in front (reverse proxy) —
      TokenOS itself serves plain HTTP.
- [ ] Keep provider keys in env vars managed by your secret store; never
      commit them.
- [ ] Point `$TOKENOS_TRACES` and `$TOKENOS_DB` at appropriately
      permissioned directories in multi-user environments.
- [ ] Review `tokenos providers` after config changes to confirm the filter
      matrix admits only the models you intend to pay for.

## Reporting

If you discover a security issue, please open a private security advisory on
the GitHub repository rather than a public issue.
