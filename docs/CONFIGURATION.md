# TokenOS Configuration Reference

TokenOS reads a single YAML document. Every field has a sensible default —
`tokenos config init` writes a complete, offline-capable starting point.

## File locations and overrides

| Setting | Default | Override |
|---|---|---|
| Config file | `~/.config/tokenos/config.yaml` | `$TOKENOS_CONFIG` env var or `--config <path>` flag |
| State database | `~/.local/share/tokenos/tokenos.db` | `$TOKENOS_DB` env var or `--db <path>` flag |
| Flight recorder dir | `~/.local/state/tokenos/traces` | `$TOKENOS_TRACES` env var or `--traces <path>` flag |
| Web auth token | — | `$TOKENOS_AUTH_TOKEN` env var or `--auth-token <tok>` flag |

API keys are read **only** from environment variables (named per provider in
`api_key_env`). They are never written to disk and never returned by the
config API.

## Top-level document

```yaml
current_profile: default      # free-form profile label
policy: { ... }               # router policy (section below)
providers: { ... }            # provider profiles (section below)
execution_routing: [ ... ]    # route -> provider bindings (section below)
pricing: { ... }              # shadow-pricing weights (section below)
```

---

## `policy` — router policy

Tunes the deterministic routing ladder (see [ARCHITECTURE.md](ARCHITECTURE.md) §3).

```yaml
policy:
  ask_threshold: 0.35         # confidence below this => ASK (one question, zero tokens)
  direct_max_tokens: 600      # estimated tokens at or below this can take DIRECT
  delegation_penalty: 1500    # fixed token-equivalent cost charged to DELEGATE
  delegation_min_scale: 1.5   # savings must exceed penalty * this scale
  max_cost_per_task_usd: 0    # budget sentinel; 0 = disabled
  reuse_cache: true           # verified solution cache
  verification_command: ""    # global local verification command
  verification_commands:      # route-specific local verification commands
    PATCH: "cargo check"
    IMPLEMENT: "cargo test"
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `ask_threshold` | float | `0.35` | If extracted confidence falls below this, route is `ASK`. Raise to ask less often; lower to ask more often. |
| `direct_max_tokens` | int | `600` | Ceiling (conservative token estimate) for the `DIRECT` fast path. |
| `delegation_penalty` | float | `1500` | Fixed overhead (in token-equivalents) any delegation must amortize. |
| `delegation_min_scale` | float | `1.5` | Estimated savings must exceed `delegation_penalty × delegation_min_scale` before `DELEGATE` is chosen. |
| `max_cost_per_task_usd` | float | `0` | Budget sentinel: hard per-task USD ceiling. Over-budget providers are pruned from the failover chain; if **every** candidate exceeds it, the run terminates locally at zero token cost. `0` disables. |
| `reuse_cache` | bool | `true` | Verified solution cache: identical goal+constraints re-requests are served from SQLite at zero tokens. Only verified successes are cached; a later failure of the goal evicts the entry. |
| `verification_command` | string | `""` | Optional global verification shell command. Runs on successful execution of tasks. |
| `verification_commands` | map | `{}` | Optional route-specific verification shell commands overrides (keys are route types, e.g. `PATCH` or `IMPLEMENT`). |

## `providers` — provider profiles

A map from provider name to profile. The default config ships `mock`
(enabled) plus `openai`, `anthropic`, `gemini` (disabled until you add keys).

```yaml
providers:
  anthropic:
    adapter: anthropic                  # mock | openai | anthropic | gemini | proxy
    auth_type: api_key                  # api_key | oauth2 | none
    api_key_env: ANTHROPIC_API_KEY      # env var holding the key
    endpoint: https://api.anthropic.com/v1
    model: claude-sonnet-4-20250514     # default model for this provider
    priority: 1                         # lower = preferred (tie-break input)
    quota_limit_per_min: 0              # 0 = unlimited; >0 enables quota pressure
    max_context_tokens: 200000          # hard window constraint for shadow pricing
    cost_per_mtok_in: 3.0               # $ per million input tokens
    cost_per_mtok_out: 15.0             # $ per million output tokens
    disabled: false                     # true removes it from every chain
    models:                             # two-tier filter matrix (below)
      include: ["claude-*"]
      exclude: ["claude-2*"]
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `adapter` | string | — (required) | Which transport implementation to use. |
| `auth_type` | string | `""` | Informational: `api_key`, `oauth2`, or `none`. |
| `api_key_env` | string | `""` | Name of the env var holding the API key. The key itself never appears in config. |
| `endpoint` | string | `""` | Base URL. Adapters supply standard defaults. |
| `model` | string | `""` | Default model ID submitted by this provider. |
| `priority` | int | `0` | Lower wins on deterministic tie-breaks. |
| `quota_limit_per_min` | int | `0` | Per-minute call budget; as usage approaches it, shadow-priced utility is discounted (quota pressure). `0` disables. |
| `max_context_tokens` | int | `0` | Hard constraint: providers whose window can't fit the payload are excluded from the chain. |
| `cost_per_mtok_in` / `cost_per_mtok_out` | float | `0` | Token prices used by shadow pricing and telemetry cost accounting. |
| `models` | filter | empty | Two-tier include/exclude matrix (below). |
| `disabled` | bool | `false` | Removes the provider from all chains without deleting the profile. |

Validation is intentionally strict:

- `adapter` must be one of `mock`, `openai`, `anthropic`, `gemini`, `proxy`, or
  `proxy_ide`.
- Enabled `openai`, `anthropic`, and `gemini` providers must name a non-empty
  `api_key_env`, and adapter construction requires that environment variable
  to resolve to a non-empty value.
- `proxy` / `proxy_ide` providers must define an `endpoint`.
- Live providers must have positive `max_context_tokens`.
- Token costs must be non-negative.

### The two-tier model filter matrix

Shell-style wildcards: `*` matches any run of characters, `?` exactly one.

Evaluation order (per model ID):

1. **Exclusion always wins.** If any `exclude` pattern matches → rejected.
2. **Non-empty include acts as a whitelist.** If `include` is non-empty, the
   model must match at least one pattern.
3. **Default allow.** Empty `include` admits everything not excluded.

```yaml
models:
  include: ["gpt-4o*", "gpt-4.1*", "o4*"]
  exclude: ["*-preview-*"]    # even an included pattern loses to this
```

Inspect verdicts with `tokenos providers`.

## `execution_routing` — route bindings

Binds kernel routes to a primary provider with an optional fallback. The
shadow-pricer + bandit reorder the resulting chain by live evidence.

```yaml
execution_routing:
  - provider: anthropic
    route_types: [IMPLEMENT, PATCH]
    fallback: openai            # appended to the failover chain
    timeout_ms: 120000          # per-attempt transport timeout
    max_context_tokens: 0       # optional per-rule window override
  - provider: openai
    route_types: [DIRECT, REUSE, DELEGATE, PARTIAL]
    fallback: mock
    timeout_ms: 60000
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `provider` | string | — (required) | Primary provider for these routes. |
| `route_types` | list | — (required) | Route names this rule covers (`DIRECT`, `REUSE`, `PATCH`, `IMPLEMENT`, `PARTIAL`, `DELEGATE`). |
| `fallback` | string | `""` | Single fallback provider appended to the chain. |
| `timeout_ms` | int | `0` | Transport timeout per attempt. |
| `max_context_tokens` | int | `0` | Optional rule-level window cap. |

Routes without a matching rule fall back to all enabled providers ordered by
shadow pricing.

## `pricing` — shadow-pricing weights

```yaml
pricing:
  alpha: 1.0     # weight on token cost
  beta: 0.002    # weight on latency (ms)
```

The utility quoted per provider is
`U = confidence / (alpha · tokenCost · 1000 + beta · latency)`.
Raise `alpha` to be more cost-sensitive; raise `beta` to be more
latency-sensitive.

## Complete worked example

```yaml
current_profile: production
policy:
  ask_threshold: 0.35
  direct_max_tokens: 600
  delegation_penalty: 1500
  delegation_min_scale: 1.5
pricing:
  alpha: 1.0
  beta: 0.002
providers:
  anthropic:
    adapter: anthropic
    auth_type: api_key
    api_key_env: ANTHROPIC_API_KEY
    model: claude-sonnet-4-20250514
    priority: 1
    max_context_tokens: 200000
    cost_per_mtok_in: 3.0
    cost_per_mtok_out: 15.0
    models:
      include: ["claude-*"]
      exclude: ["claude-2*"]
  openai:
    adapter: openai
    auth_type: api_key
    api_key_env: OPENAI_API_KEY
    model: gpt-4o-mini
    priority: 2
    max_context_tokens: 128000
    cost_per_mtok_in: 0.15
    cost_per_mtok_out: 0.60
    models:
      include: ["gpt-4o*", "gpt-4.1*", "o4*"]
  mock:
    adapter: mock
    auth_type: none
    model: mock-1
    priority: 100
    max_context_tokens: 128000
execution_routing:
  - provider: anthropic
    route_types: [IMPLEMENT, PATCH]
    fallback: openai
    timeout_ms: 120000
  - provider: openai
    route_types: [DIRECT, REUSE, DELEGATE, PARTIAL]
    fallback: mock
    timeout_ms: 60000
```

## `security` — security & governance policies

Configures traces retention, spend limits, and scoped API tokens.

```yaml
security:
  disable_traces: false           # set true to disable all out-of-band flight logs
  retention_days: 30              # auto-pruning window for database & traces (0 = forever)
  owner_only_permissions: true    # enforces owner-only (0o600/0o700) file permissions on Unix
  daily_spend_limit_usd: 0.0      # daily cost ceiling; 0 disables
  monthly_spend_limit_usd: 0.0    # monthly cost ceiling; 0 disables
  api_token_rate_limit_per_min: 60 # shared SQLite-backed per-token API request limit; 0 disables
  api_tokens:
    read_only_token_abc: ["read"]
    runner_token_xyz: ["run", "read"]
    admin_token_123: ["admin"]
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `disable_traces` | bool | `false` | If true, trace files are not written to disk. |
| `retention_days` | int | `30` | Auto-pruning window for loop history, telemetry records, and traces on startup. |
| `owner_only_permissions` | bool | `true` | Enforces owner-only file permissions on traces and state files. |
| `daily_spend_limit_usd` | float | `0.0` | Saturated daily spend blocks further execution; 0 disables. |
| `monthly_spend_limit_usd` | float | `0.0` | Saturated monthly spend blocks further execution; 0 disables. |
| `api_token_rate_limit_per_min` | int | `0` | Per-token API request ceiling per minute. Counts are stored by SHA-256 token hash in SQLite, so multiple TokenOS processes sharing the same DB coordinate this limit. |
| `api_tokens` | map | `{}` | Map of API bearer token to list of authorized scopes (`read`, `run`, `admin`). |

---

## Validation

- `tokenos config` prints the effective merged configuration.
- `tokenos providers` shows per-provider enablement and filter-matrix verdicts.
- A malformed YAML file fails fast with the parser's line/column diagnostics.
