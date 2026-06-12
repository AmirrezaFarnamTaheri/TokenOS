# TokenOS Architecture

TokenOS is a deterministic execution kernel for LLM-driven agents. Its single
governing rule:

> **Never spend more resources deciding than the decision can save.**

This document explains how the subsystems compose, what invariants each one
maintains, and where the zero-token boundaries are.

---

## 1. High-level dataflow

```
            task + constraints
                    |
                    v
        +-----------------------+
        |  kernel::decide()     |  deterministic route ladder -- ZERO tokens
        |  (signals -> route)   |
        +-----------+-----------+
                    | Route in {DIRECT, REUSE, PATCH, IMPLEMENT,
                    |           PARTIAL, DELEGATE, ASK, ESCALATE-*}
                    v
        +-----------------------+
        |  contextidx           |  minimum viable context <= 2000 tokens
        |  (symbol index)       |  -- ZERO tokens
        +-----------+-----------+
                    v
        +-----------------------+
        |  payload::build()     |  JIT cache-aligned prompt
        |  (static->volatile)   |  (DELEGATE -> DelegationPacket JSON)
        +-----------+-----------+
                    v
        +-----------------------+
        |  maskcodec::mask()    |  secrets replaced with placeholders
        +-----------+-----------+  before any byte leaves the process
                    v
        +-----------------------+
        |  pricing + bandit     |  shadow-priced provider ordering,
        |  failover ordering    |  scaled by UCB1 evidence -- ZERO tokens
        +-----------+-----------+
                    v
        +-----------------------+
        |  provider adapter     |  THE ONLY PAID STEP
        |  (mock/openai/        |
        |   anthropic/gemini)   |
        +-----------+-----------+
                    v
        +-----------------------+
        |  maskcodec::unmask()  |  placeholder echoes restored
        |  jsonrescue::rescue() |  truncated JSON repaired (if JSON intent)
        +-----------+-----------+
                    v
        +-----------------------+
        |  verify::static_check |  free verification before acceptance
        |  loopdetect           |  semantic loop => ESCALATE
        +-----------+-----------+
                    v
        +-----------------------+
        |  store (SQLite)       |  compressed state, telemetry,
        |  recorder (CAS blobs) |  flight-recorder trace
        +-----------------------+
```

Everything above and below the provider adapter is pure CPU work. The kernel
guarantees that routing, context selection, verification, loop detection, and
provider ordering never consume a single token.

## 2. Crate layout

The crate ships as a **library plus a thin binary**:

| Path | Role |
|---|---|
| `src/lib.rs` | Library root — every module is `pub`, so the kernel embeds in other runtimes |
| `src/main.rs` | CLI binary (`clap`); a pure consumer of the `tokenos` library |

### Module map

| Module | Responsibility | Key invariant |
|---|---|---|
| `kernel` | Route ladder, signal extraction, `RouterPolicy`, `State`, `DelegationPacket` | Same input ⇒ same route, always. No I/O. |
| `config` | YAML config, provider profiles, two-tier model filter matrix, routing rules | Exclusion always wins; default config works offline |
| `engine` | Orchestrator: route → context → payload → mask → failover → verify → record | The single place where money can be spent |
| `provider` | Adapters: mock (fault-injectable), OpenAI, Anthropic, Gemini, proxy | Errors are classified (retryable vs terminal) |
| `pricing` | Shadow pricing, EWMA failure tracking, quota pressure, lock-free UCB1 bandit | All reads/writes lock-free (`AtomicF64` bitcast CAS) |
| `payload` | JIT cache-aligned prompt builder | Byte-stable static prefix ⇒ provider prompt-cache hits |
| `verify` | Tiered verification: free static checks | Free checks always run before paid ones |
| `tokenizer` | Calibrated heuristic + greedy BPE counter | `count_conservative` never under-estimates vs heuristic |
| `jsonrescue` | Single-pass truncated-JSON rescuer | Never "repairs" non-JSON prose (EOF-consumption guard) |
| `maskcodec` | Edge secret masking | Reverse vault lives only in the request's stack frame |
| `loopdetect` | Myers bit-parallel Levenshtein loop detection | Window persisted in SQLite — survives process restarts |
| `contextidx` | Structural symbol index (FTS5 with LIKE fallback) | Minimum viable context ≤ 2000 tokens |
| `store` | SQLite state: tasks, failure memory, loop history, telemetry, trace index, solution cache | State objects and trace metadata, never raw transcripts |
| `recorder` | Flight recorder: SHA-256 CAS blobs + NDJSON journal | Diagnostics never enter the context window |
| `webui` | axum control panel | Lock-free read handlers, bounded concurrent runs, constant-time bearer auth |

## 3. The routing ladder

`kernel::decide()` walks a strict priority ladder. The first matching rung wins:

| Priority | Route | Trigger | Token cost |
|---|---|---|---|
| 0 | `ESCALATE-CONFLICT` | Contradictory constraints detected | 0 |
| 0 | `ESCALATE-SAFETY` | Safety violation signal | 0 |
| 0 | `ESCALATE-EXTERNAL` | Semantic loop detected (loopdetect) | 0 |
| 1 | `ASK` | Missing critical info or confidence < `ask_threshold` (0.35) | 0 |
| 2 | `DIRECT` | Trivial task, estimate ≤ `direct_max_tokens` (600) | minimal |
| 3 | `REUSE` | Exact verified solution-cache hit for the same goal + constraints | 0 on replay |
| 4 | `PATCH` | Localized change with no repeated failure on this goal | small |
| 5 | `DELEGATE` | Repetitive + bounded + savings > `delegation_penalty × delegation_min_scale` | packet only |
| 6 | `PARTIAL` | External blocker — deliver completed portion | bounded |
| 7 | `IMPLEMENT` | Default productive path | normal |

`ASK` and all escalations terminate locally at **zero LLM cost**. `REUSE` is
not a workspace-index hit. The workspace index only supplies minimum viable
context for a prompt; a task routes to `REUSE` only when the durable verified
solution cache has an exact, replayable goal+constraint match.

Failure memory feeds back into the ladder: a goal that has previously failed
with a similar approach is biased away from `PATCH`, and the failed approach
is *forbidden* in the payload's constraint block.

ASK is deliberately local. Once the router decides information is missing, the
engine emits one deterministic clarifying question, marks the task blocked,
records zero tokens and no provider/model, and stops. Sending an ASK to a model
would violate the routing contract because the system already knows it needs
human input.

## 4. Shadow pricing and the bandit

For each candidate provider the engine computes a quote:

```
U = confidence / (alpha * tokenCost * 1000 + beta * latency)
```

- `confidence` is discounted by a per-provider **failure EWMA**, and by
  **quota pressure** as the provider approaches its per-minute limit.
- A hard constraint removes providers whose context window cannot fit the
  payload.
- Ties break deterministically (provider name ordering) so failover order is
  reproducible.

The **UCB1 bandit** (lock-free, `AtomicF64` compare-and-swap over bitcast
`u64`s) then scales each utility by an *exploitation weight*:

| Arm state | Weight | Effect |
|---|---|---|
| Unexplored (0 pulls) | `1.0` | Shadow pricing alone decides — every arm still gets explored |
| Explored | `0.5 + mean_reward` | Live evidence reorders the failover chain |

Reward signals:

- Verified success → reward `1.0`, latency-discounted.
- Transport error → reward `0.0`.
- Verification failure → reward `0.0`.

Standings are observable via `tokenos telemetry`, `GET /api/stats/bandit`,
and the dashboard's *Bandit Standings* panel.

## 5. Payload construction and cache alignment

`payload::build()` serializes sections in strict volatility order:

1. **Static** — `KERNEL_CONTRACT` (byte-stable across all calls)
2. **Semi-static** — constraints, forbidden approaches (from failure memory)
3. **Volatile** — task, surgical context

Because the static prefix is byte-identical on every call, providers with
prompt caching (Anthropic, OpenAI) hit their cache on the longest possible
prefix.

`Route::Delegate` short-circuits into `build_delegation()`, which emits a
compact `DelegationPacket` JSON — task, scope, constraints, acceptance
criteria, next step. Conclusions only; no history, no reasoning, no
transcript.

## 6. The response leg

After a provider responds, in order:

1. **JSON rescue** — if the task signals JSON intent (case-insensitive
   `json` in the task or constraints) and the output is truncated JSON, the
   single-pass lenient parser repairs it: partial strings keep their content,
   dangling keys are dropped, open containers are closed. A truncation guard
   refuses to touch prose that merely *starts* with a bracket. Rescues are
   flight-recorded as `rescue` events.
2. **Static verification** — free checks: diff shape for PATCH, exactly-one-
   question contract for ASK, brace balance, ellipsis/truncation detection.
3. **Loop detection** — normalized Myers bit-parallel Levenshtein distance
   over a sliding window of the last 5 outputs (persisted in SQLite).
   Distance < 3% ⇒ semantic loop ⇒ `ESCALATE-EXTERNAL`.
4. **Recording** — telemetry row in SQLite; trace metadata in SQLite; full
   payloads as SHA-256 content-addressed blobs in the flight recorder.
5. **Unmask for caller only** — placeholder echoes are restored from the
   request-scoped vault after durable writes complete. The unmasked form is
   returned to the caller and is not written to SQLite or recorder blobs.

## 7. Persistence model

Two stores, deliberately separate:

| Store | Contents | Why separate |
|---|---|---|
| **SQLite** (`store.rs`) | Compressed task states, goal-keyed failure memory (max 5/goal), loop-detection windows, execution telemetry, trace metadata, verified solution cache | Queryable, transactional, survives restarts |
| **Flight recorder** (`recorder.rs`) | Decision/prompt/response/rescue/error events (NDJSON journal) + full payload blobs (SHA-256 CAS) | Diagnostics must never compete with state for context tokens |

State is stored as **compressed state objects** (goal, status, blockers,
acceptance, next step) — never transcripts. The conversation is not the
source of truth; SQLite is.

## 8. Concurrency model

- **Web handlers are lock-free**: the axum router shares an `Arc<Engine>`;
  there is no global mutex. A long-running `/api/run` never blocks telemetry
  reads.
- **Bandit and tracker** use atomic CAS loops over bitcast `f64`s — no locks
  on the hot path.
- **Route previews** (`/api/route`) run on `spawn_blocking` so pure CPU work
  never stalls the async reactor.
- **Execution backpressure** caps concurrent `/api/run` work at four in-process
  slots. Saturated servers return `429` while keeping dashboard reads and route
  previews available.
- **Adapters map** is behind an `RwLock` only for registration; the read path
  is shared.

## 9. Determinism guarantees

Same inputs produce:

1. The same route (`kernel::decide` is pure).
2. The same provider order (deterministic tie-breaking; bandit weights are
   the only intentional run-time variation, and they default to neutral).
3. The same payload bytes (BTreeMap-ordered config, stable serialization).

This is what makes the kernel testable offline: `--dry-run` swaps in the
fault-injectable mock adapter and the entire pipeline runs deterministically
with zero network access.

## 10. Where to go next

- [CONFIGURATION.md](CONFIGURATION.md) — every config field explained
- [CLI.md](CLI.md) — full command reference
- [API.md](API.md) — HTTP API reference
- [SECURITY.md](SECURITY.md) — threat model and hardening details
