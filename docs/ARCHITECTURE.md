# TokenOS Architecture

TokenOS is a deterministic execution kernel for LLM-driven agents. Its single
governing rule:

> Never spend more resources deciding than the decision can save.

The active product is local-first: native desktop app, CLI, and embeddable Rust
library. The former browser dashboard and HTTP API have been retired.

## 1. System Positioning

TokenOS is an in-process control kernel, not a conversational proxy, HTTP
gateway, screen automation agent, or multi-tenant SaaS service.

| Axis | TokenOS pattern |
|---|---|
| Routing state | Local `State`, SQLite telemetry, deterministic signals |
| Context control | Symbol-indexed minimum viable context and distilled payloads |
| Credential boundary | API keys stay in env vars; prompts are masked before adapter calls |
| Loop memory | Persisted SQLite windows by stable task scope |
| Provider choice | Local shadow pricing, quota pressure, failure EWMA, UCB1 evidence |
| UI boundary | Native egui/eframe app with direct engine/store calls |

Remote identity, certificate operations, provider contract testing, disk
encryption, monitoring, and fleet-wide quota governance are operator-owned
controls.

## 2. Dataflow

```text
task + constraints
  -> kernel::decide()             zero-token deterministic route ladder
  -> contextidx                   minimum viable context, zero tokens
  -> payload::build()             cache-aligned prompt or delegation packet
  -> maskcodec::mask()            secrets masked before egress
  -> pricing + bandit             provider ordering, zero tokens
  -> provider adapter             only paid step unless mock/dry-run
  -> maskcodec::unmask()          caller-boundary restoration only
  -> jsonrescue + verify          free output rescue/checks
  -> loopdetect                   persisted semantic loop detection
  -> store + recorder             SQLite state and flight-recorder blobs
```

Everything except the provider adapter is local CPU and storage work.

## 3. Crate Layout

| Path | Role |
|---|---|
| `src/lib.rs` | Library root; modules are public for embedding |
| `src/main.rs` | CLI and native app dispatch |

| Module | Responsibility | Key invariant |
|---|---|---|
| `kernel` | Route ladder, signals, policy, state, delegation packets | Same input yields same route |
| `config` | YAML config, provider profiles, model filters, routing rules | Exclusion wins; defaults work offline |
| `engine` | Route, context, payload, provider failover, verify, record | The only place provider spend occurs |
| `provider` | Mock, OpenAI, Anthropic, Gemini, proxy-compatible adapters | Errors are classified |
| `pricing` | Shadow pricing, quota pressure, drift, UCB1 bandit | Hot-path statistics avoid coarse locks |
| `payload` | Static-first prompt construction | Byte-stable prefix supports provider caches |
| `verify` | Tiered verification | Free checks run before acceptance |
| `tokenizer` | Conservative token estimation | Never under-estimates versus heuristic |
| `jsonrescue` | Truncated JSON repair | Non-JSON prose is not fabricated into JSON |
| `maskcodec` | Secret masking | Reverse vault is request-scoped |
| `loopdetect` | Semantic loop detection | Window survives process restarts |
| `contextidx` | Structural symbol index | Context is bounded before prompt build |
| `store` | SQLite state and telemetry | State objects and metadata, not transcripts |
| `recorder` | CAS blobs and NDJSON journals | Diagnostics stay outside conversation context |
| `nativeapp` | egui/eframe desktop UI | Direct engine/store calls; no HTTP listener |

## 4. Routing Ladder

`kernel::decide()` walks a strict priority ladder:

| Priority | Route | Trigger | Provider cost |
|---|---|---|---|
| 0 | `ESCALATE-CONFLICT` | Contradictory constraints | 0 |
| 0 | `ESCALATE-SAFETY` | Safety violation signal | 0 |
| 0 | `ESCALATE-EXTERNAL` | Semantic loop or external blocker | 0 |
| 1 | `ASK` | Missing critical info or low confidence | 0 |
| 2 | `DIRECT` | Trivial, bounded local answer | minimal |
| 3 | `REUSE` | Exact verified solution-cache hit | 0 on replay |
| 4 | `PATCH` | Localized change with no repeated failure | small |
| 5 | `DELEGATE` | Repetitive bounded work with savings | packet only |
| 6 | `PARTIAL` | External blocker with useful completed work | bounded |
| 7 | `IMPLEMENT` | Default productive path | normal |

`ASK` emits exactly one local question, records a blocked task, and stops with
zero tokens and no provider/model. `REUSE` requires an exact, replayable
verified solution-cache hit; workspace context alone never routes to `REUSE`.

## 5. Provider Ordering

Provider utility is quoted from confidence, token price, expected latency,
quota pressure, context fit, and failure history. A process-local UCB1 bandit
then adjusts ordering from observed reward:

- verified success earns latency-discounted reward;
- transport and verification failures earn zero;
- unexplored arms remain eligible so provider exploration does not collapse.

Standings are visible through `tokenos telemetry` and the native Operations
view.

## 6. Payload And Response Leg

Payloads are serialized in volatility order: static contract, semi-static
constraints/failure memory, then volatile task/context. Delegation routes emit
a compact JSON contract instead of a full prompt transcript.

Responses pass through:

1. JSON rescue for JSON-intent truncated output;
2. static verification;
3. loop detection;
4. provider-attempt recording;
5. final execution recording and trace indexing;
6. caller-boundary unmasking.

Unmasked content is returned to the caller but not written to SQLite or
recorder blobs.

## 7. Persistence

| Store | Contents | Purpose |
|---|---|---|
| SQLite | Task states, failure memory, loop windows, executions, provider attempts, aggregate request stats retained for legacy DBs, trace metadata, solution cache | Queryable operational state |
| Flight recorder | Decision/prompt/response/rescue/error events and content-addressed blobs | Diagnostics without context-window cost |

Task state is compressed into goals, status, blockers, acceptance, and next
step. Conversation history is not the source of truth.

## 8. Concurrency And Resource Boundaries

- Native UI work is split between the egui main thread and a background Tokio
  runtime for long-running executions.
- Bandit/tracker hot paths use atomic updates.
- SQLite writes are transactional.
- Route signal extraction uses linear-time regexes.
- Context lookup is truncated before prompt build.
- JSON rescue is single-pass and only runs for JSON-intent tasks.
- Loop detection caps compared text before edit-distance work.

## 9. Determinism Guarantees

Same inputs produce the same route, same baseline provider order, and same
payload bytes. Runtime learning can reorder providers only through explicit
recorded evidence. Dry-run swaps in the mock provider so the full pipeline is
testable offline.

## 10. Related Docs

- [CONFIGURATION.md](CONFIGURATION.md)
- [CLI.md](CLI.md)
- [API.md](API.md)
- [SECURITY.md](SECURITY.md)
- [PRODUCTION_READINESS.md](PRODUCTION_READINESS.md)
