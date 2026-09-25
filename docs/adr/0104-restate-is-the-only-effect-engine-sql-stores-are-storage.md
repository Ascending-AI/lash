# 0104: Restate is the only effect engine; SQL stores are storage

## Status

Accepted 2026-09-24 (FIG-3669). It records Sam's ruling on FIG-3664, including
the hard constraint that the effect interface stays engine-neutral. **Not yet
implemented**: FIG-3665 through FIG-3668, FIG-3670, FIG-3585, FIG-3600 and the
B2 backend-construction cutover build it, in the order under *Order*. Nothing below describes current behaviour unless it says
so. Implemented so far: step 2 (FIG-3585) and step 3 (FIG-3667): the
PostgreSQL engine is deleted, `PostgresBackend` with it. The B2 construction
of section 2 is implemented as described there: a PostgreSQL deployment runs a
`RestateEngine` over a `PostgresStoreSet`.

Supersedes [ADR 0102](0102-zero-infra-is-a-sqlite-in-memory-backend.md); see
"What 0104 kept" at the end of that ADR. Amends every ADR that specifies
SQL-engine behaviour; they are listed under *ADR text* and each carries a short
note pointing here.

The evidence is the durability prospect of 2026-09-24
(`/workspace/notes/lash/durability-prospect/REPORT.md`, verified in
`VERIFY.md` beside it), its independent review (`astra-sqlite-engine.md` in the
same directory) and the Restate testing research
(`/workspace/notes/lash/restate-testing-research/report.md`). Line counts below
are the prospect's, taken at origin/main `3fd6df8e1`.

## Context

Lash owns two effect engines and delegates to a third:

- **The store-backed replay driver** (`StoreEffectReplayDriver`) over two row
  stores. SQLite and PostgreSQL each implement `EffectReplayRowStore` (27
  methods), the process lease and wake outbox of `ProcessRegistry`, turn-input
  and queued-work claims, and `SessionExecutionLeaseStore`. The engine share is
  about 13.8k code lines in `lash-sqlite-store` and 12.2k in
  `lash-postgres-store`. The decisions are shared (`decide_*` and `plan_*`), but
  every transaction around them is written twice: 45 mirrored modules and 293
  dialect-only statements, 68 of them lock or clock forks.
- **The native effect host**, which FIG-3585 deleted under ADR 0102.
- **Restate**, through `lash-restate`. It implements `Backend`, `EffectHost` and
  `RuntimeEffectController` over the Restate SDK and forwards every other port
  to an `Arc<dyn StoreSet>`, SQLite or PostgreSQL. Under Restate the replay rows
  and the process lease are unused. The process registry's lifecycle, events and
  wakes are still written, and the core turn loop still takes the SQL
  session-execution lease and turn-input claims
  (`crates/lash-core/src/runtime/turn_loop/lease.rs`, `accept.rs`).

Production and multi-node deployments run Restate. No consumer runs PostgreSQL
as the engine. figments runs Restate over PostgreSQL storage, and
`PostgresBackend` is used outside its crate only by tests, lash-sim, lash-perf
and runbooks.

Owning an engine beside Restate means lash's hard semantics are implemented
twice and must be kept equivalent. The review counts seven obligation families
wired separately in the replay driver and in `lash-restate`: replaying
recorded outcomes, detecting divergence before dispatch, durable sleeps and
wakes, effect-group membership, dispatch and reopen, commit ordering with drain
barriers and settlement ranks, cancellation fencing fresh admission, and
ownership and recovery after interruption. A test on the SQLite engine proves
the SQLite engine; it does not prove what production runs. About 29 of the 41
fix-like commits since 2026-08-01 were engine fixes, and nine named one
backend.

ADR 0102 made zero-infra a SQLite in-memory backend, which keeps lash owning an
engine for the quickstart, the examples and most tests.

## Decision

### 1. One effect engine, and SQL stores hold storage only

**Restate is the only effect engine.** `lash-restate` is the only implementor of
the effect interface (§2). SQLite and PostgreSQL are storage only. Both SQL
effect engines are deleted in clean cutovers with no shims: PostgreSQL in
FIG-3667, SQLite in FIG-3668.

**The SQL stores hold exactly this:**

- session commits and history;
- attachments and artifacts, including the Lashlang artifacts an RLM session
  writes: the backend's store set supplies that port (FIG-3633), so the
  artifacts live in the storage that reopens the session;
- process execution environments;
- process definitions, and process records as data;
- triggers;
- the `session_ingress` rows
  ([ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md),
  FIG-3600);
- parked-work storage (FIG-3659).

They decide no effect semantics: no replay, no lease, no claim arbitration, no
wake scheduling and no process scheduling.

### 2. The effect interface is engine-neutral

Restate is today's one implementor, not the interface. Temporal, or another
engine, must plug in later without redesigning lash. The contract review of
2026-09-24 (`/workspace/notes/lash/fig3664-engine-contract/astra-report.md`)
found that engine-neutral signatures alone do not make Temporal fit. The
obligations in §3 are therefore written as outcomes, and the execution seam
below is a requirement of its own.

**The interface.** One engine is one `EffectEngine` value. It carries the
effect host (`EffectHost`, `RuntimeEffectController`, with their scoped
controllers and effect-group handles), the work driver (`QueuedWorkSubstrate`,
`ProcessWorkSubstrate`, `TurnWorkDriver`, `TurnAttach`) and the store set it was
built over. A backend wraps exactly one engine. The end-state construction
(ruling B2 on FIG-3664's contract review) is:

```rust
RestateEngine::new(stores: Arc<dyn StoreSet>, cfg: RestateConfig)
    -> Result<RestateEngine, EngineConfigError>;
Backend::new(engine: Arc<dyn EffectEngine>) -> Backend;
EffectEngine::stores(&self) -> Arc<dyn StoreSet>;
EffectEngine::process_work(&self) -> ProcessWorkWiring;
StoreSet::binding_identity(&self) -> &StoreBindingId;
StoreSet::module_artifacts(&self) -> Arc<dyn ModuleArtifactStore>;
```

- **`Backend` has one private field, the engine.** Every port it hands out
  derives from that one binding. No engine operation takes a stores argument,
  `process_work` included: the engine already holds its store set.
- **`RestateConfig` selects `SubmitOnly` or `Serve`.** A submit-only engine
  keeps its scheduling and control clients and starts no handlers.
- **Storage identity and effect authority are distinct.** The store set's
  `StoreBindingId` names the storage; the engine's authority names the effect
  state. Both are derived coherently when the engine is constructed and never
  compared at runtime. No API accepts a second, independently assembled binding.
- **Ports above the kernel belong to the store set.** The Lashlang artifact
  port is `StoreSet::module_artifacts`, and its trait lives in a layer both
  the store sets and lashlang can depend on: the byte-level
  `ModuleArtifactStore` sits in `lash-core-execution`, and lashlang reads it
  through its typed `LashlangArtifacts` view, which owns the module codec.
  The port's name is language-neutral because the kernel crates name no
  integration (`lash-core`'s integration-boundary lint). This part is
  implemented. The RLM protocol factory takes the
  backend, never a store, and names it through `PluginFactory::bound_backend`.
  A core over any other backend refuses the factory with
  `EmbedError::PluginBackendMismatch`.
- **Fixtures move through one constructor.** Tests build an engine through a
  single `lash-restate-test` constructor (FIG-3665, FIG-3668), never by
  assembling ports.

**Implemented (B2 cutover).** `Backend` is a struct over one
`Arc<dyn EffectEngine>`; `RestateEngine::new(stores, RestateConfig)` replaces
`RestateBackend`; `StoreSet::binding_identity` names the storage, and the
runtime no longer compares it with the effect host's turn-control binding.
Two parts of the sketch above wait for FIG-3600, which replaces the queued-work
wiring with the engine's own session work: `RestateConfig` still carries the
existing `RestateQueuedWork` choice, and `SubmitOnly | Serve` is not a config
choice yet, because a serving endpoint needs the process worker of the core
built over the backend, which exists only after the engine does; a submit-only
process is still one that never calls `endpoint_builder`. `RestateEngine::new`
is infallible until a construction check exists. Until FIG-3668 deletes the
SQLite engine, `SqliteBackend` is its `EffectEngine`, and
`EffectEngine::process_work` returns `None` for it.

**The execution seam.** The driver exposes replayable decisions and
registered, serializable effect commands; adapters own scheduling and I/O
execution. Engines like Temporal need it: their workflow code must schedule
deterministically, and every I/O runs as an activity. Today lash hands the
controller a borrowed local runner and runs effects on Tokio, and running a
whole drive as one activity would record no per-effect history. The seam is
proven on paper against Temporal before FIG-3600's S5 freezes the drive API.
Deterministic logical operation ids are neutral; engine invocation ids stay
opaque to lash.

The rules:

- **No engine concept crosses into the kernel.** No engine type, id, context,
  virtual object, workflow, invocation, error code or journal format appears in
  `lash-core-store`, `lash-core-execution`, `lash-core` or the facade. Engine
  specifics live only in the engine's crate. The facade may re-export that
  crate behind its cargo feature (`lash::restate` behind `restate`), because
  choosing an engine is the host's choice; no other facade item names an
  engine.
- **Adding an engine changes no kernel crate.** A second engine lives in its own
  crate, implements the interface and passes the conformance suite (§3). If it
  needs a change to a kernel crate or the facade, the interface was wrong, and
  that is lash's defect
  ([ADR 0045](0045-services-are-stateless-substrates-own-continuation.md),
  *Conformance is the contract*).
- **The storage side is engine-neutral too.** `StoreSet` is storage-only, so an
  engine takes any SQL store set, and a store set assumes no engine.

**Known violations at origin/main `b4e1318cc`.** Each is a defect against this
section. FIG-3670 removes them: it renames the `restate_*` identifiers to
engine-neutral opaque ids (landed: `RuntimeExecutionContext::engine_execution_id`,
`ProcessExecutionWriteAuthority::engine_execution_id`,
`LeaseOwnerIdentity::engine_process_execution` and
`engine_process_execution_id`, and the `engine_execution_id` field of
lash-trace's language-execution identity with its OpenTelemetry attribute
`lash.language_execution.engine_execution_id`), renames the
`RuntimeErrorCode::Restate*` error vocabulary to `Engine*` variants with
`engine_*` wire strings (landed), moves the Restate format vocabulary below
`lash-restate`, and makes the substrate boundary gate refuse new ones. None
may be added.

- The Restate durable formats in the facade's format registry
  (`DurableFormat::Restate*`, `crates/lash/src/formats.rs`).

### 3. Engine obligations are contracts

Lash relies on these guarantees from its engine. Each is an outcome stated
against the interface, never a mechanism: the last column names Restate's
mechanism as one implementation, not as the contract. Every row comes from an
ADR, from code lash runs today, or from the contract review (O1–O6).

| Obligation | Contract | Restate, today's one implementation |
|---|---|---|
| **Per-session serialized execution** (O1) | One authorized logical drive per session; stale mutations are refused; retried external operations have stable idempotency identities. No engine alone makes arbitrary external effects non-overlapping, so a fence cannot retract a request already sent: the external operation's idempotency covers that. The session-head CAS stays the commit authority ([ADR 0029](0029-claims-are-generation-fenced-under-the-session-lease.md), ADR 0045). | A virtual object keyed by session, which owns ingress, claims and wakes (FIG-3600). **Not yet implemented**: today the Restate path takes the SQL session-execution lease and claim CAS, which FIG-3600 deletes. |
| **Durable acceptance and scheduling** (O2) | Persist acceptance and recoverable scheduling intent together; acknowledge engine submission separately; reconcile every unacknowledged intent. This covers ingress items, wakes and control commands. A guaranteed background recovery owner exists; a status read is not liveness. A drive request's id carries application dedupe across engine runs. | Acceptance rows in SQL ([ADR 0069](0069-durable-acceptance-is-the-sole-turn-ingress.md), [ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md)); the ingress sweep submits per row and Restate coalesces a submission onto a live key (ADR 0045). |
| **Durable step result** | Redrive preserves recorded outcomes: an effect's outcome is recorded before the execution depends on it, and a redrive returns it under the same replay key without dispatching again. | `ctx.run(..).name("lash:<replay_key>")` (`controller/journaled_effect.rs`). |
| **Durable timer** | A sleep survives a crash, fires once at or after its deadline and holds no worker while it waits. Deadlines are retained across replay and segment handover. | `ContextTimers::sleep` (`controller/context.rs`; wait deadlines in `durable_wait.rs`). |
| **Durable keyed promise** | A one-shot promise, addressed by a neutral key and scope-agnostic, that an execution parks on and anything holding the key resolves; every richer wait compiles onto it ([ADR 0003](0003-keyed-promise-is-scope-agnostic.md), [ADR 0012](0012-durable-waits-via-effect-host-engines.md)). Engine wait ids, such as awakeable ids, are never exposed. | `LashDurableWaitWorkflow` promises, indexed per session by the `LashDurableWaitIndex` object (`durable_wait.rs`). |
| **Effect groups** | The durable group of [ADR 0065](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md) and [ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md), which exceeds a step journal. An engine supports the whole surface or refuses it coherently. | The `EffectGroupIndex` and `EffectGroupPayload` objects and the `EffectGroupDispatch` workflow (`effect_group.rs`, `effect_group/`). |
| **Engine-owned retry, and park** (O3) | Preserve recoverable history and prohibit fresh semantic admission while parked; reconcile classified engine failures. A process body whose replay diverges parks through the process registry's park (L-E6): one park per divergence, re-parked rather than reopened on every attempt that refuses again, no terminal evidence, and its claims held. The engine re-drives a live fault under its own policy and owns backpressure (`owns_commit_backpressure`); lash never re-drives engine-owned work (ADR 0045). Retry needs an explicit attempt and timeout policy. An attempt count is not a divergence diagnosis, and not every engine pause means exhaustion: pause is adapter-private, and attempt and failure details are optional. Stalled session wrappers and children are reconciled as well as turns and processes, through retained owner mappings. | The deployment's invocation retry policy, and `RestateEffectGroupRetryPolicy` for group dispatch. A turn's replay divergence (an envelope or group-reopen mismatch) parks as the engine-neutral `effect_replay_divergence`: the turn handler fails the attempt retryably so the invocation keeps its journal, and `lash_restate::turn_service` bounds its attempts before the invocation pauses. A diverged process body parks the process through the registry's park (`ProcessTransition::Park`, FIG-3674): exactly one park per divergence, kept across the attempts that refuse again, no terminal evidence, and the segment's attempt fails retryably so the invocation keeps its journal. Parking an exhausted invocation is not yet implemented (ADR 0045; FIG-3600 A4, FIG-3675). |
| **Redrive, cancellation and recovery** (O4) | Redrive preserves recorded outcomes; cancellation cooperates with Lash closure; unsafe recovery returns a typed refusal. Cancellation is a durable, externally addressable stop request whose first winner is a keyed promise ([ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md)), honoured `Immediate` or `AfterStep`, and it reaches group children. Cancelling or killing an engine invocation is host break-glass and never proves a lash `Cancelled` result; it cannot substitute for tool draining or lifetime closure. Reset is not redrive: it discards history after a prefix and is never an ordinary recovery path. | Turn-control gate promises on the durable-wait workflow, and invocation cancel for group dispatch children (`effect_group/dispatch.rs`). |
| **Replay determinism** | A redrive by the same build issues the same effects in the same order under the same replay keys, with a nested effect's identity being its issue ordinal ([ADR 0103](0103-code-cells-replay-by-re-execution-on-every-host.md)). A mismatch is detected before dispatch and parks. A journal written by another generation is refused before any effect. | SDK journal replay over named entries, lash's envelope and replay-hash checks, and the `RESTATE_PROCESS_JOURNAL_VERSION` gate (ADR 0045, FIG-3588 amendment). |
| **Process identity across runs** (O5) | Preserve logical process identity, durable continuation and child obligations across engine runs. A journal is never unbounded ([ADR 0025](0025-bounded-journals-are-an-effect-controller-obligation.md)), and a run handover, like continue-as-new, explicitly carries continuation, dedupe and unresolved obligations, because the next run does not inherit history. | Segments chained across invocations keyed by process and segment, under a journal budget (`controller/journal_budget.rs`, `process/workflow.rs`). |
| **Admission before the first effect** (O6) | Validate current generation and retained execution authority before new effects, including child entries; preserve the admission marker across replay. A fresh execution of work that already started is refused (`SubstrateLost`) and never re-run: a false `Abandoned` is accepted, a duplicate effect is not (ADR 0045, FIG-3588 amendment). A cached first-step result does not prove current authority. | Two journaled steps, a verdict with a nonce and then a set-if-absent marker, which yield the `SegmentStarted` proof (`process/admission.rs`). Stated today for process segments. |
| **Process execution and terminal wait** | The engine admits pending process rows and executes them. A terminal wait goes only through `ProcessWorkSubstrate::await_process_terminal` ([ADR 0016](0016-process-waits-live-on-the-work-driver-seam.md)). Recovery applies the declared disposition mechanically ([ADR 0019](0019-process-recovery-obeys-declared-disposition.md)). | `LashProcessWorkflow` (`process/workflow.rs`) and the process-attach workflow (`process_attach.rs`). |

**Conformance laws are written against the interface.** The engine-owed laws
are the host-generic suites in `lash-conformance`: `effect_host_tests!`,
`effect_group_host_tests!`, `effect_host_await_event_tests!`,
`turn_crash_matrix_tests!` and their siblings. They reach an engine only through
the backend and the ports above, so a future engine runs the same suite
unchanged. Every obligation in the table owes laws there, and an obligation
without them is a gap in lash. The suite must hold these laws explicitly; "the
existing contract, unchanged" is not a law:

- **Groups.** Groups retain membership, replay validation, settlement rank,
  cancellation admission fences and protected-declaration drain across crashes.
- **Waits.** Wait registration and resolution survive resolve-before-wait,
  duplicate delivery and restart; resolution is first-writer-wins; revocation
  and timer/cancel races replay their winner. Timer deadlines survive replay and
  segment handover.
- **Admission.** The FIG-3588 admission identity is journaled before work is
  published; missing retained history refuses recovery rather than allocating
  anew.
- **Lifetime.** Only logical terminal evidence closes lifetime scopes; parking
  and segment changes do not. Protected tool declarations drain before terminal
  evidence, and Until-scope cancellation is then requested durably. Detached
  processes survive. Pinned history is retained, with its counts and bytes
  exposed (FIG-3607). Engine child-parent policies never replace these rules.
- **Observation.** Outcome attachment is root-addressed, observation cursors
  are replay-safe, and reconciliation failures are visible. Engine success,
  timeout or termination alone is not lash terminal evidence.

Two gaps are known today. The admission marker's laws live only in
`lash-restate` (`tests::substrate_lost`), because they are written against
Restate's identity and retry semantics. Per-session serialized execution has no
engine laws until FIG-3600 makes it an engine obligation.

Tests of how Restate implements an obligation live in `lash-restate`: the
protocol, always-replay, the `RESTATE_PROCESS_JOURNAL_VERSION` gate and
`tests::substrate_lost`. Laws that exist only for the SQL engines' internals,
such as `store_effect_group_drain_conformance` and session-execution-lease
renewal, are deleted with those engines.

### 4. Zero-infra is a local Restate server

Zero-infra is a local `restate-server` or `restate dev`, over a SQLite store
set.

- **`restate-server`** is one static binary. Measured on v1.7.12, it answers
  admin `/health` in about 0.2 s, is ready for queries in about 0.3 s, and idles
  at about 200 MB RSS.
- **`restate dev`** runs `restate-lite` inside the Restate CLI.

**The in-process test runtime is never zero-infra.** `lash-restate-test`
(FIG-3665) is a test double of the Restate **server**. It is not an effect
engine, and it is never shipped as durable.

**Embedded durable execution is not offered now.** Durable execution inside a
CLI or desktop process, with no server, is not offered. If it is needed later,
the named option is `restate-lite`: Restate's in-process node. It is
unpublished, its RocksDB budget defaults to 512 MiB, and it allows one running
instance per process. A second lash-owned engine is not an option.

### 5. Testing model

Tests run on production semantics, in three kinds:

1. **The `lash-restate-test` runtime** (FIG-3665). It is in-process,
   pool-executable and deterministic. A test-only invoker drives lash-restate's
   real endpoint (`Endpoint::handle`) and the real `restate-sdk-shared-core` VM
   (pinned 7.0.3). It keeps an in-memory journal per invocation, simulates a
   crash as drop-and-replay, fires timers on command under virtual time, and has
   an always-replay mode. It replaces the SQLite and in-memory effect hosts for
   turn-level tests, conformance and lash-sim.
2. **Real-server suites** (FIG-3666) for invoker, retry, suspension, network and
   restart behaviour, which the test runtime does not model. They run beside a
   `restate-server` on the CI runners and follow the upstream recipe: one server
   per test binary shared across its tests, unique keys per test, an
   always-replay leg (`INACTIVITY_TIMEOUT=0s`), retries off and millisecond
   sleeps.
3. **Storage laws** run against SQLite and PostgreSQL only.

**The engine-neutral laws do not depend on `lash-restate-test` internals.** The
runtime sits behind the same interface as a real server. A law that reaches past
that interface into the invoker's journal or clock is a Restate test, and it
belongs in `lash-restate`.

The test runtime only fakes the server's invoker. The SDK half of the protocol,
the VM and lash's handlers are the production code. That is what separates it
from the fakes ADR 0102 rejected, which re-implement engine rules by hand. What
it cannot model, the real-server suites cover.

### 6. Deletion list

Deleted from both SQL stores and from `lash-core-execution`, `lash-core` and
`lash-core-worker`:

- the effect replay driver and `EffectReplayRowStore`;
- the SQL effect host, controller and adapter;
- `AwaitEventResolver` on SQL;
- process leases, lease renewal and takeover;
- wake claiming and sweeps as SQL-engine duties;
- queued-work claims;
- `SessionExecutionLeaseStore`;
- turn-input claim CAS as engine ownership (FIG-3600 makes ingress, claims and
  wakes the engine obligation *per-session serialized execution*);
- the native process scheduler, and with it the in-process queued-work and
  process-work drivers, which have no store left to claim from;
- SQL crash windows and fault hooks;
- the engine legs of `lash-conformance` and the SQL worlds of `lash-sim`;
- SQL engine performance baselines;
- all docs and ADR text that describe SQL engines.

**Crates.** `lash-postgres-store` and `lash-sqlite-store` become storage-only,
with their shared SQL in `lash-store-sql`. `lash-restate` depends on `StoreSet`
and no concrete store. A production deployment compiles `lash-restate` and one
storage crate, and nothing else.

### 7. Order

The replacement comes first. Each deletion is one clean PR with no dual paths.

1. The `lash-restate-test` runtime and the real-server recipe (FIG-3665,
   FIG-3666). The harness exists and the slow suites are retuned.
2. FIG-3585 lands: the native host and `lash-core-memory` are deleted.
   FIG-3584 is rescoped: conformance, lash-sim and lash-perf move onto
   `lash-restate-test`, not onto the SQLite effect host.
3. The PostgreSQL engine is deleted (FIG-3667).
4. The FIG-3600 cutover: the Restate session virtual object owns ingress, claims
   and wakes, and the SQL session-admission leases are deleted.
5. The SQLite engine is deleted, and both stores split into storage-only crates
   (FIG-3668).
6. FIG-3607 is re-planned against the Restate-only engine: its SQL-engine rules
   drop, and identity plus storage lifetime remain.

Cancelled or reshaped:

- **Cancelled:** FIG-3644, whose three engine-neutral fixes are salvaged into a
  small PR.
- **Moot:** the SQL half of FIG-3588, FIG-3547's SQL rows, and S7's SQL
  re-drive-budget park.
- **Reshaped:** FIG-3659's SQL process-worker sweep and park logic goes; its
  storage stays.

### 8. ADR text

The ADRs below specify SQL-engine behaviour, and each carries a note pointing
here. Their SQL-engine passages stay as written while the code exists; the
deletion PR that removes the code (FIG-3667 or FIG-3668, or FIG-3600 for the
session lease) rewrites them in the same change. Their engine-neutral decisions
stand.

| ADR | SQL-engine behaviour it specifies |
|---|---|
| [0008](0008-confidence-gate.md) | SQLite and PostgreSQL backend conformance and lease-contention evidence in the gate lanes |
| [0009](0009-deterministic-simulation-harness.md) | lash-sim's SQL worlds and their lease, fencing and reopen contention artifacts |
| [0012](0012-durable-waits-via-effect-host-engines.md) | the SQL substrates' effect journal and promise rows |
| [0014](0014-operational-policy-stays-with-the-host.md) | Lease Timings for session-execution, effect-replay and process leases, and failover parity |
| [0016](0016-process-waits-live-on-the-work-driver-seam.md) | `NativeProcessAwaiter` and in-process process work for store-only deployments |
| [0019](0019-process-recovery-obeys-declared-disposition.md) | lease-validated completion and recovery after a lapsed process lease |
| [0025](0025-bounded-journals-are-an-effect-controller-obligation.md) | re-drive against `runtime_effect_replay` on the store tier, and SQL replay-row retirement |
| [0027](0027-unleased-completion-carries-explicit-authority.md) | completion with and without a Lash process lease |
| [0029](0029-claims-are-generation-fenced-under-the-session-lease.md) | generation-fenced claims under the session-execution lease |
| [0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md) | the lease generation that authorizes a cancel closure, and lease renewal and takeover around it |
| [0041](0041-child-turn-and-driver-stack-growth-have-canonical-seams.md) | session-execution leases and claims owned by a child runtime |
| [0045](0045-services-are-stateless-substrates-own-continuation.md) | *The reference substrate*: lash as the substrate over SQL, with its own redrive and budget |
| [0049](0049-session-ids-are-used-once.md) | SQL scope retirement of effect, group and promise rows, and the scope fence |
| [0053](0053-claim-nonces-scope-session-lease-lifecycle.md) | claim nonces of the session-execution lease |
| [0065](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md) | the SQL tiers' group rows, finalization and drain |
| [0067](0067-durable-rows-name-one-owner-and-one-reclaim-trigger.md) | owners and reclaim triggers of the effect-replay, group and await-event rows, and lease-held repair |
| [0068](0068-one-meaning-per-outcome-suffix.md) | `ProcessLeaseClaimOutcome` and `SessionExecutionLeaseClaimOutcome` |
| [0069](0069-durable-acceptance-is-the-sole-turn-ingress.md) | claiming accepted input under the session-execution lease |
| [0077](0077-session-state-migrates-totally-at-admission.md) | admission under the session-execution lease |
| [0080](0080-substrate-attestation-is-not-a-lease-short-circuit.md) | failover that waits out the session-execution lease TTL |
| [0082](0082-process-registry-is-composed-from-narrow-concern-traits.md) | the `ProcessLeases` and `ProcessWakeOutbox` registry concerns |
| [0094](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md) | the SQL tiers' parent-end ledger write and its crash window |
| [0097](0097-durable-session-and-live-session-are-two-authorities.md) (durable session) | the live session's Session Execution Lease |
| [0098](0098-one-owner-per-sql-table-across-both-stores.md) | table modules for engine tables such as `runtime_effect_replay` |
| [0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md) | `EffectReplayRowStore` group operations, SQL finalization, the store drain and SQL crash windows |
| [0100](0100-the-run-observation-contract.md) (run observation) | SQL replay rows keyed by scope and replay key |
| [0101](0101-one-session-ingress-carries-every-admitted-item.md) | ingress claims fenced by the session lease, and the in-process driver on SQLite and PostgreSQL |
| [0103](0103-code-cells-replay-by-re-execution-on-every-host.md) | the journal-row hosts and replay-hash parking on every SQL host |

`CONTEXT.md` follows the same rule. The Substrate Contract and engine entries
are updated now. The lease and claim entries (Lease Timings, Claim Generation,
Session Execution Lease Authority, Work Claim, Claim ID Derivation) are
rewritten or removed by the deletion PRs.

## What is deliberately not adopted

- **SQLite as the one engine lash owns** (the prospect's recommendation, and the
  review's verdict B). It keeps durable execution without a server for embedded
  hosts, and deleting an isolated engine later is easy. It also keeps two
  engines in scope: production still runs Restate, so every obligation above
  would stay implemented twice, and tests on SQLite would keep proving an
  engine production does not run.
- **One SQL engine behind dialect hooks** (DBOS, Temporal's SQL plugins). It
  would delete the mirrored transaction wrappers, but lash would still own a
  second engine beside Restate.
- **One engine over about fifteen primitive storage traits** (Golem, pi). It
  pays off only with a non-SQL backend, and it maps poorly onto cross-session
  sweeps and observers.
- **The in-process shared-core VM as a shipping durable backend** (the review's
  option C). A durable local host would still need journal persistence, timers,
  object serialization, dispatch and recovery around the VM. That is a lash-owned
  engine again. The same VM is adopted only as a test double of the server.
- **Embedding `restate-lite` now.** It is unpublished and a process-global
  singleton with a heavy dependency tree. It stays the named option if embedded
  durable execution is ever needed.

## Consequences

- **One set of semantics.** Each obligation is implemented once, and "engine
  once" comes free instead of through a SQL-engine abstraction.
- **Tests run what production runs.** Conformance, the crash matrices and
  lash-sim drive lash-restate's real handlers on the real VM.
- **Zero-infra needs a server.** A quickstart or local developer runs
  `restate-server` or `restate dev` next to a SQLite store set. That costs one
  binary and about 0.3 s at start.
- **No durable execution without a server.** An embedded CLI or desktop agent
  that wants restart durability runs a local Restate server. hirsel uses
  `lash_sqlite_store` at a stale pin; its durability moves to Restate when it
  updates.
- **Multi-node without Restate is not supported.** Nothing in lash offers
  PostgreSQL as an engine.
- **A future engine is a crate, not a redesign.** It implements §2's interface
  and runs §3's laws.
- **Risk: the test runtime drifts from the server.** It fakes the invoker, so a
  divergence between its journal handling and Restate's is possible. The
  mitigation is that it drives the real SDK VM and lash's real endpoint, has an
  always-replay mode, and that invoker, retry, suspension and restart behaviour
  stays on the real-server suites.
- **Risk: virtual time.** lash-sim's `SimClock` does not advance a Restate
  server's timers. That is why the test runtime owns timers and fires them on
  command.

## History: what the SQL engines gave us, and why they go

The store-backed replay driver made lash usable with no infrastructure. It ran
the quickstart, the examples and most tests. As the reference substrate of ADR
0045, it forced the effect contract into explicit, testable form: sealed row
operations, shared `decide_*` and `plan_*` decisions, generation-fenced claims,
scope fences and the crash matrices. Many contract defects were found there
first, and the conformance suite that now defines the engine obligations grew
up against it.

They go because they were a second engine. Every hard semantic was written in
the driver and again in `lash-restate`, and the transaction wrappers around the
shared decisions were written twice more, once per dialect. Most fixes since
August were engine fixes. The tests that ran on them certified an engine
production does not run, and no consumer used PostgreSQL as an engine. What the
SQL stores do well, storage, they keep.
