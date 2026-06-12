# TokenOS Troubleshooting Guide

Symptoms, causes, and fixes — roughly in the order you're likely to hit them.

## Build issues

### `error: package ... requires rustc 1.75 or newer`

Update your toolchain: `rustup update stable`.

### Linker errors mentioning `sqlite3`

You shouldn't see these — SQLite is bundled (`rusqlite` `bundled` feature).
If you do, run `cargo clean && cargo build`; a stale build cache from a
different feature set is the usual culprit.

## Configuration issues

### "config not found" or defaults being used unexpectedly

Resolution order is: `--config` flag → `$TOKENOS_CONFIG` → 
`~/.config/tokenos/config.yaml`. Check which one is winning:

```sh
tokenos config        # prints the effective merged configuration
```

### My provider never gets selected

Work through this checklist:

1. **Is it enabled?** `disabled: false` in the profile.
2. **Is the key present?** The env var named in `api_key_env` must be set in
   the shell that launches tokenos.
3. **Does the filter matrix admit the model?** Run `tokenos providers` —
   remember **exclusion always wins**, and a non-empty `include` list is a
   strict whitelist.
4. **Is there a routing rule for the route?** Check `execution_routing` —
   the route printed by `tokenos route "<task>"` must appear in some rule's
   `route_types`, or the provider must survive the default shadow-priced
   ordering.
5. **Does the context fit?** Providers whose `max_context_tokens` can't hold
   the payload are excluded from the chain entirely.

### `tokenos config init` says the file already exists

By design — `init` never overwrites. Move or delete the old file first.

## Routing surprises

### Everything routes to ASK

The confidence signal is below `ask_threshold` (default 0.35). Either the
task genuinely lacks critical information (the kernel asks exactly one
question and stops — at zero cost), or your tasks are very terse. Add
specifics, or lower `policy.ask_threshold`.

### A task I expect to be DIRECT routes to IMPLEMENT

`DIRECT` requires the **conservative** token estimate (max of the calibrated
heuristic and a greedy BPE count) to be ≤ `policy.direct_max_tokens`
(default 600). The conservative counter deliberately never under-estimates,
so borderline tasks fall through to `IMPLEMENT`. Raise the policy value if
your workload skews trivial.

### PATCH suddenly stopped being offered for a goal

That goal has failure memory: a previous similar failure was recorded, the
approach is now forbidden, and routing is biased away from `PATCH`. This is
intentional — see the failure entries with `tokenos trace <task-id>`.

### ESCALATE-EXTERNAL: "semantic loop"

The last outputs for this goal were ≥ 97% similar (normalized Levenshtein
< 3%). The window is **persisted in SQLite**, so this fires even across
separate CLI invocations. It means the system is genuinely stuck — change
the approach or constraints rather than retrying the same prompt.

## Execution issues

### `all providers failed`

Check the trace: `tokenos trace <task-id>` shows each attempt's classified
error (timeout, auth, quota, server). Common causes:

- Expired/missing API key (auth errors are terminal, not retried)
- Quota exhaustion — if `quota_limit_per_min` is set, pressure discounts the
  provider before hard failure
- Network egress blocked — verify with the mock: `--dry-run` should succeed

### Output looks like repaired JSON I didn't ask for

The JSON rescuer only activates when the task or constraints mention
"json" (case-insensitive) **and** the output parses as truncated JSON
consuming the whole input. Rescues are logged as `rescue` events in the
trace. If you see one, the model's output really was cut mid-stream.

### Latency spikes on first call per provider

Connection establishment plus a cold prompt cache. The payload builder's
byte-stable static prefix means subsequent calls hit the provider's prompt
cache; the bandit also learns latency and reorders failover accordingly.

## Dashboard issues

### "connecting…" never turns green

The frontend polls `/api/summary`. Check:

- Is the server actually up? (`tokenos serve` prints the bind address.)
- Auth: if a token is configured, click **API token** in the sidebar and enter
  the same bearer token used to start the server. The dashboard then attaches
  it to all `/api/*` requests.

### `--host 0.0.0.0` is refused

Binding non-loopback requires both `--public` **and** an auth token
(`--auth-token` or `$TOKENOS_AUTH_TOKEN`). This is a guardrail, not a bug.

### Dashboard loads but every panel says 401

The static shell can load without auth, but every `/api/*` data call still
requires the bearer token when auth is configured. Click **API token** in the
sidebar, enter the same token used to start the server, and retry the panel. If
it still fails, clear the token and re-enter it; wrong tokens are rejected with
the same `401` shape as missing tokens.

### Bandit panel says "unexplored" everywhere

Bandit state is **per-process** and learns from live executions in the
serving process. Run some tasks from the Run Console and the standings fill
in. CLI runs in a different process won't appear in the server's panel.

### Stats look stale

The dashboard auto-refreshes every 5 seconds while the tab is visible (it
deliberately pauses when hidden to save resources). Switch views or refocus
the tab to force an immediate refresh.

## State and storage

### I want a clean slate

```sh
rm ~/.local/share/tokenos/tokenos.db        # tasks, telemetry, failure memory, loop windows
rm -r ~/.local/state/tokenos/traces         # flight recorder
```

(Or point `$TOKENOS_DB` / `$TOKENOS_TRACES` at fresh paths.)

### Where did my trace blobs go?

`tokenos trace <id>` shows the event timeline; add `--blobs` to print full
payload contents. Raw blobs live under the traces directory in `objects/`,
keyed by SHA-256.

## Still stuck?

Open an issue with:

1. `tokenos --version` / `rustc --version`
2. The `tokenos route "<task>"` output (free, deterministic, shareable)
3. The relevant `tokenos trace <task-id>` timeline (redact business content)
