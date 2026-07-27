# Durable background process execution

**Status:** implemented. A background process is identically durable however it
is started: start writes durable intent, and a lash-owned `DurableProcessWorker`
is the *sole* executor of every non-terminal process, behind a single-owner
`ProcessLease`. There is no off-lease in-process spawn — out-of-turn starts no
longer run on a detached `tokio::spawn`. The registry's non-terminal rows are the
durable work queue, and a `ProcessWorkDriver` drains them **after a successful
start and once on session open**. The same driver/awaiter seam owns process
waits; registries remain point-read/write state stores. So the *first* run of an out-of-turn start is
itself lease-protected and prompt (the driver is invoked directly after the
start), not merely eventually recovered by the next restart's sweep. This closes
the turn-vs-trigger asymmetry
— processes started by matched trigger occurrences run durably and recover on
crash, exactly like turn-started ones — and removes the double-execution window
the startup-only sweep left open. Timers and recurring jobs are host-owned
scheduling sources: a source owner emits a trigger occurrence with a stored
`source_key`, and every matched subscription starts a deterministic process run
with the same lease-protected execution + recovery. This note records the design
and principle; the *Implementation* section below maps it to the code that
shipped. Restate references name the first-party adapter and the regression this
design fixed; the boundary applies to any workflow host that supplies a scoped
effect controller and durable timers.

## Problem (the asymmetry this resolved)

A background `process` can be started two ways:

- **in a turn** — the model emits `start name(...)` while a turn runs;
- **by a trigger occurrence** — a host-owned source emits an occurrence
  through the runtime router, which matches stored trigger subscriptions by
  `source_type` and `source_key`.

Before this work, only the first was durably re-executed in a Restate
deployment, and the asymmetry was silent.

Durability of execution used to be borrowed from the **turn-local effect
scope**. A turn run through the old scope-taking `stream(&sink, scope)` facade threaded the
Restate-backed scoped controller into process admin via a silent
`effect_controller.unwrap_or(current.host.core.effect_controller)` fallback, so a
turn's `start name(...)` scheduled a `LashProcessWorkflow` invocation that
Restate re-invoked on crash — but trigger emission runs **outside** a turn.
It cannot borrow a turn-scoped controller, so process starts must use the
host-supplied runtime effect controller for the occurrence emission. Before the
cutover, the old session-coupled trigger path hit the **build-time** inline effect host in a
Restate deployment (the
Restate-backed controller is constructed per-invocation from a Restate `ctx`, so
it can never be a process-global build-time controller). The inline path
`tokio::spawn`ed the
process and dropped the `JoinHandle`: the run completed
in-process under normal operation, but the spawn held **no lease**, so a recovery
sweep on another node or after a restart could re-run a process already running
(the off-lease/double-exec window), and on crash the in-memory task was gone with
**no prompt re-execution** — only the next restart's startup sweep would re-run
it. Registry rows — record, events, grants, wake inbox — survived a restart; the
*execution* was either off-lease or merely eventually-recovered.

The fix removes that silent fallback **and** the off-lease spawn. Process admin
now routes through the controller the host **explicitly wired** for the path, and
a `Start` only *registers* the durable row; the lease-protected
`DurableProcessWorker` — driven promptly by a `ProcessWorkRunner` — is the single
executor for all out-of-turn process work. A durable host never silently
re-executes a process on an inline controller, and no process ever runs without
holding its `ProcessLease`.

## Principle

lash's thesis is that **the runtime is the durable end** for committed session
state and process admin records, while the active effect host owns in-flight
effect replay. A durable workflow host supplies handler history and timers for
turn effects; the configured store supplies committed session state and process
registry rows. SQLite and Postgres are first-party store adapters, but the
contract is the store trait, not a specific database. Process execution follows
that split: **lash owns the process durability logic; the host supplies the
backend.**

## What the OpenAI Agents SDK does — and why it is the wrong template

The OpenAI Agents Python SDK draws the boundary in the opposite place. Its
durability primitive is `RunState`: a run interrupts (e.g. a human-in-the-loop
tool approval), the host calls `result.to_state()`, serializes it
(`to_string()`/`to_json()`), **persists it itself**, and later resumes via
`Runner.run(agent, state)`. The SDK owns only the serialization surface and the
resume entry point; the host owns when/where to persist, when to re-drive, and
idempotency. There is no background/trigger/timer process concept — "long
running" means host-driven pause/resume; Temporal integration lives in the
orchestrator, not the SDK.

That is coherent because the SDK is a **stateless request/response agent loop** —
durability genuinely is not its job. Copying it into lash would (a) contradict
the "durable runtime" thesis and (b) push lease/recovery/idempotency onto every
host *per background process*. Right answer for a thin stateless SDK; wrong
answer for a durable runtime with first-class background processes and triggers.

## Rejected: emitter-supplied durable turn scopes

The tempting stopgap is to let the emitter carry a durable scope, mirroring the
turn API. It is the wrong layer:

- it couples a process's durability to **whoever emitted the event**;
- it does not generalize to host timers, whose scheduler should not carry a
  Lash turn scope;
- it conflates "produce start intent" with "execute it durably."

Adding it would ossify an emitter-carries-durability model exactly where the
clean worker-owns-execution-durability model belongs.

## Design: separate intent from execution

**1. Start = durable intent, full stop.** `start name(...)` in a turn and a
matched trigger delivery each do one thing: write a durable process record
(carrying the captured execution-environment ref the worker will execute
against — see `docs/adr/0011-self-contained-processes.md`) plus any handle
grant to the registry. The rows survive restart. Turn starts carry turn
causality; trigger deliveries carry
`CausalRef::TriggerOccurrence { occurrence_id, subscription_id }`.
Neither path carries a borrowed execution scope or a live session binding.
The captured process environment is typed and closed: it contains Process
Plugin Options plus policy, not product metadata. Before a Lashlang
`ProcessInput::Engine { kind: "lashlang", payload }` row is registered, the
Lashlang runtime rebuilds the candidate plugin session, Tool Catalog, and
Lashlang Host Environment from those captured options and validates that
environment against the target module artifact's Host Requirements.
Trigger-started and turn-started processes both capture the same environment
shape, and worker recovery repeats the Host Requirements guard before compiling
or running the process.

**2. Execution = a lash-owned durable worker with a per-process lease +
recovery.** Turns use effect-host replay plus final commit stamps; processes use
registry events plus process leases:

| Unit    | Single-owner lease | Resumable state             | Recovery entry         |
| ------- | ------------------ | --------------------------- | ---------------------- |
| Turn    | effect-host history | host-recorded effects       | workflow handler replay |
| Process | `ProcessLease`     | registry events + journal   | `drive_pending_processes` (on start / on open) |

The `DurableProcessWorker` — with a persisted lease inline and a replay-stable
root Restate invocation id via `RestateCoreProcessRunner` /
`LashProcessWorkflow` — is the single executor for
**all** non-terminal registered processes, regardless of how they were started.
It:

- claims a per-process execution lease (single-owner, renewable, expiring);
- runs the process through the host-supplied durable controller;
- on startup / deploy / lease expiry, **sweeps the registry for non-terminal,
  unleased processes and recovers each by its declared disposition**
  (`docs/adr/0019-process-recovery-obeys-declared-disposition.md`): it re-runs a
  `Rerunnable` row, terminalizes a provably-dead started `OwnerBound` row as
  `Abandoned` instead of re-running it, and never claims an `ExternallyOwned`
  row. It is no longer a re-run-everything loop.

**3. Host supplies the backend; lash owns the durability logic** — exactly as
with turns. The host wires a durable registry (`SqliteProcessRegistry` for
local deployments or `PostgresProcessRegistry` for distributed deployments) and
runs the worker (under Restate, the worker handler is a workflow Restate
re-invokes). The host does not re-implement recovery, leasing, or idempotency
per deployment.

This deletes the asymmetry: a trigger-started process is identically durable to a
turn-started one, because both are just registered intent that the durable worker
executes. "How it was started" leaves the durability story entirely.

Process waiting follows the same split. `ProcessRegistry` exposes state only;
terminal and event waits live in `ProcessWorkDriver` / `ProcessAwaiter` (see
`docs/adr/0016-process-waits-live-on-the-work-driver-seam.md`). Inline
deployments get prompt local wakeups from the watched-registry change hub and
bounded point-read backoff otherwise. External deployments can attach waits to
their own durable execution backend; the Restate driver attaches terminal waits
to the `LashProcessWorkflow/{process_id}/await_terminal` promise through
ingress instead of polling the store.

Observation and retention are two more host levers on the same seam, kept honest
against the durable log (see
`docs/adr/0017-process-observation-is-best-effort-push-over-state-truth.md`). An
optional `ProcessEventSink`, installed once at construction
(`ProcessWorkDriver::new_with_sink`, `RestateProcessDeployment::new_with_sink`,
or `LashCoreBuilder::process_event_sink`), pushes each appended event to the host
best-effort — freshness over the durable log, never truth. It makes no delivery
guarantee, so a consumer that needs completeness reconciles from `events_after`,
and terminal events deliberately do not ride the sink (they ride
`await_terminal`). Because the decorator awaits `emit` inline on the append path,
a sink must return fast and offload any I/O. Retention is
`ProcessRegistry::prune_terminal_processes(cutoff_epoch_ms)`: a host that has
projected a process's outcome into its own store calls it on the maintenance
cadence to drop terminal rows — with their events, wakes, grants, and leases —
older than a window comfortably longer than any in-flight `await_terminal`.

## The primitive (a process-side mirror of the turn machinery)

A generalization of code that already existed for turns — not a new subsystem:

- **`ProcessLease`** — claim / renew / expire, single-owner, so a recovered
  process is not double-run across nodes. Carries
  `schema_version`, `process_id`, `owner_id`, `lease_token`, `fencing_token`,
  `claimed_at_epoch_ms`, `expires_at_epoch_ms`, plus `ProcessLeaseCompletion`.
  The owner / lease-token / fencing-token triple is the distributed
  single-owner contract. Implemented on `SqliteProcessRegistry` and
  `PostgresProcessRegistry` (durable, fencing CAS) plus
  `TestLocalProcessRegistry` (in-memory).
- **`ProcessRegistry::list_non_terminal()`** — the worklist query: the
  non-terminal rows *are* the durable work queue, alongside the claim / renew /
  complete lease ops.
- **`DurableProcessWorker::drive_pending_processes`** — the sole inline
  process executor. Live tool starts, subagent fan-outs, trigger deliveries,
  admin starts, session-open work, and crash recovery all enter this same
  lease-protected drive. It lists the full non-terminal worklist, claims each
  lease (skipping any held live by another owner), re-checks terminality after
  claiming, then handles each row by its declared `RecoveryDisposition` (ADR
  0019). A `Rerunnable` row —
  or a not-yet-started `OwnerBound` one, since a first execution is not a
  re-execution — is run on the worker's wired controller while renewing the lease
  across the execution, then atomically validates the current fence, writes its
  terminal outcome, and releases the lease via
  `complete_process_with_lease`. A started `OwnerBound` row whose holder is provably dead is
  terminalized `Abandoned{Sweep}` rather than re-run; a merely silent one is left
  non-terminal. An `ExternallyOwned` row is never claimed. Idempotent by
  `process_id`: terminal processes are never on the worklist, and a process that
  became terminal between the list and the claim is detected and skipped.
  Engine infrastructure failures do not become producer `Failed` terminals:
  the worker releases the claim and lets the engine/substrate pace a retry.
  `max_attempts = None` deliberately means unbounded retry, so a deterministic
  failure may keep the row non-terminal and its awaiters pending until the host
  cancels or abandons it. Producers with deterministic failure modes should set
  an explicit `max_attempts`.
  Inline execution is bounded per worker by
  `process_execution_concurrency` (default 64, minimum 1). A run holds a slot
  for its own work, including provider streaming and tool I/O, releases it while
  parked on another process or external completion, and reacquires before it
  resumes. Two workers over one registry therefore have twice the configured
  aggregate capacity.
- **`ProcessWorkDriver` (`claim_and_run_pending`)** — the seam that *drives*
  `drive_pending_processes` promptly: invoked directly after a successful start
  and once on session open (`drive_process_on_open`), the latter folding in what
  used to be a separate boot-time recovery sweep. A `ProcessRunHandle` selects
  the tier: the inline `InlineProcessRunHandle` delegates to the worker's
  `drive_pending_processes`; the Restate `RestateProcessIngressRunner` POSTs
  `LashProcessWorkflow/{process_id}/run/send` to the ingress per non-terminal
  row (Restate coalesces by workflow key, so duplicate submits are idempotent).
  The control seam drives the driver after every successful `Start` —
  idempotently, since the driver skips leased/terminal rows and the durable
  submit is keyed-idempotent — so in-turn-inline, trigger delivery, and other
  explicit out-of-turn starts are all driven the same way.
  The driver lives on the runtime host, not the trait-object controller.

## Idempotency

Two layers, both in place. **Start-idempotency**: the registry's `process_id`
uniqueness is the replay boundary for process admin issued outside a turn
lease — `ProcessCommandRunner::run` (`process_runners/control.rs`) routes every
command through the explicitly selected controller and drives the work driver
after a successful `Start`, and a duplicate `Start` re-registers the same row
rather than spawning a second execution. **Execution-idempotency**: the
`ProcessLease` ensures a recovered, non-terminal process is re-run by exactly
one owner. Process execution identity is the persisted `process_id` end-to-end
(the Restate invocation is already keyed `LashProcessWorkflow/{process_id}`); a
retry — a Restate `run` re-invocation or a recovery sweep — must present that
stable id, and an empty/fresh id is rejected loudly
(`DurableProcessWorker::ensure_stable_process_id`), mirroring how
`ExecutionScope::turn` rejects an empty turn id when scoped controller creation
validates it.

## Recovery obeys declared disposition (ADR 0019)

The sweep above used to re-run every non-terminal row it could claim. That policy
was invisible and wrong for two whole classes of work: a `shell.start` row is
schema-identical to any recoverable tool call, so recovery re-executed the command
(a fresh PTY, duplicated side effects) while the original OS child was never
reaped; and an `External` row's "nothing to execute" branch fabricated a `Success`
outcome for work lash never observed. Both are the same defect — the schema could
not express what recovery is allowed to do, so recovery guessed identically for
rows with opposite contracts.

**Recovery Disposition** makes the contract a required, defaulted-nowhere field on
every registration:

- **`Rerunnable`** — another owner may re-execute the work. The contract for
  journaled, idempotent inputs: the lashlang engine and subagent `SessionTurn`
  rows declare it, and recovery re-runs them exactly as before.
- **`OwnerBound`** — the contract binds at first start. Before any owner has begun
  executing, any worker may claim the row; once execution has started, no other
  owner may ever re-execute it, and abandonment is the only recovery. `shell.start`
  declares it.
- **`ExternallyOwned`** — lash never executes the row. Closure can come from the
  explicitly unleased `complete_process` path when an external actor or a
  process-keyed workflow supplies its own single-writer authority; Lash-owned
  workers always use `complete_process_with_lease`. Recovery never claims the
  external work itself. External placeholders and detached commands declare it.

The unleased `complete_process` path takes a required `ProcessCompletionAuthority`
(`ExternalOwner` / `WorkflowKey` / `ReconciledAbandon`) that each backend
validates against the row's disposition inside the completion operation and
records on the terminal event as audit evidence — so who was allowed to close an
unleased row is explicit and enforced uniformly, not left to per-caller
convention (ADR 0027).

Deriving the disposition from the input class was rejected (it re-hides the
contract in a heuristic) and defaulting to `Rerunnable` was rejected (a producer
that forgot the field would silently re-ship the exact unsoundness this removes),
so construction without a disposition does not compile. Each store bumps its
schema and reject-and-recreates pre-column rows (SQLite 8→9, Postgres 6→7); the
wire mirror bumps `REMOTE_PROTOCOL_VERSION` 6→7.

### Abandoned is a written fact, not an inferred one

`ProcessTerminalState::Abandoned` is a fourth terminal state, peer to
`Completed | Failed | Cancelled`, with a matching `ProcessAwaitOutput::Abandoned`
arm. It records that the owner stopped executing without recording an outcome: the
true result is unknowable and no cleanup is assumed to have run. The terminal
carries an `AbandonEvidence` payload — the `AbandonWriter` that wrote it, the
dead-or-lapsed owner identity it was established against, and the timestamp — and
it is immutable: an owner that reappears is fenced by its stale lease token, never
healed back to running.

There is exactly one legitimate writer per path:

- **`OwnerDrain`** — the owner abandons its own started `OwnerBound` work inline at
  graceful drain, under its own live lease (`DurableProcessWorker::drain_owner_bound_work`).
- **`Sweep`** — the next host-triggered recovery sweep writes it for a started
  `OwnerBound` row whose holder is provably dead (`is_definitely_dead_for_claimant`).
- **`ReconciledRequest`** — the sweep reconciles a durable **Abandon Request** into
  `Abandoned` once the row's lease has lapsed.

**Elapsed time alone never produces a terminal state.** Lease expiry without death
evidence is exposed read-side, not terminalized: a started `OwnerBound` holder that
is silent but not provably dead stays non-terminal until real death evidence
appears or an operator authorizes abandonment. `Abandoned` rides `await_terminal`
and reconcile like any terminal, and — unchanged by ADR 0017 — it does not ride
the best-effort event sink.

### Read-side facts and the third-party escape hatch

The durable **`first_started`** fact (recorded under a fenced lease immediately
before a runner executes) is what lets the sweep distinguish a started `OwnerBound`
row (never re-run) from a never-started one (still claimable). `ObservedProcess`
now exposes the raw facts a host classifies staleness from — `disposition`,
`first_started`, `lease_holder`, `lease_expires_at_ms`, and a pending
`abandon_request` — with no derived "stuck" verdict; stuck detection is a host-built
read-side classification, not a lash daemon.

A non-owner cannot write a terminal at all. `ProcessRegistry::request_process_abandon`
(surfaced as `Processes::request_abandon`) writes a durable, non-terminal **Abandon
Request** marker — who, when, why — the operator's recorded authorization to accept
uncertainty. The sweep reconciles it into `Abandoned{ReconciledRequest}` only once
the lease has lapsed; the marker never terminates anything by itself and stays the
single system writer's input, not its own writer. `record_first_started` and
`get_process_lease` complete the registry's new state-only surface — it still
records facts and holds monitors, never links.

### Detached commands and OS ownership

Work meant to outlive every lash host is not registered as running at all.
`shell.start` with `detach: true` double-forks and `setsid`s the command out of the
runtime's process group, then writes an `ExternallyOwned` row that is **terminal at
birth**, carrying `{pid, pgid, command, started_at}` as an immediately-terminal
audit fact. lash never claims it is running, never signals it, and never stops it —
it is host/OS property from birth. For tracked (non-detached) PTY processes the
shell runtime's `ShellProcessTable` SIGKILLs every process group it still tracks on
teardown, including the lease-lost path, so lash's registry role — record facts,
hold monitors, never kill or supervise — stays intact while the spawning component
owns its OS resources.

### The unified facade

`core.processes()` is the single global `Processes` surface (start / get / list /
list_granted_to / list_originated_by / events / signal / await_output / cancel /
cancel_all / transfer / prune / request_abandon / session_snapshot / observer),
carrying two distinct filters: **grants** are addressability (what a session may
address) and **provenance** is origin (what a session created). `session.processes()`
is thin grant-scoped sugar returning `ObservedProcess`, and the old
`SessionProcessAdmin::await_all` misnomer — a session-graph refresh, never a wait —
is renamed `LashSession::refresh_background_graph`. `ProcessDrainReport` lives in
`lash::durability`; the Restate tier skips `ExternallyOwned` submission at ingress,
reconciles abandon requests, and completes a re-invoked started `OwnerBound` row as
`Abandoned{Sweep}` instead of re-running it. The operations runbook renders the full
recovery verdict table and the drain / crash / stuck-detection paths:
`docs/operations.html`.

## Non-goals

- Not "make every emitter supply a durable turn scope".
- Not "the host re-implements recovery" (the OpenAI `RunState` model).
- Not exactly-once side effects inside a process beyond what the active effect
  host can replay or dedupe — the same at-least-once-with-idempotency semantics
  apply.

## Implementation

Replay behavior is derived from the controller the host wired, not from a
runtime mode or an end-to-end durability claim. `EffectReplayOwnership::Runtime`
means Lash executes and records effects locally; `Controller` means the
controller owns replay and Lash dispatches through it. Store traits do not
advertise a durability tier, and the runtime does not try to infer deployment
guarantees by comparing independently wired store facets.

Out-of-turn starts (trigger deliveries; facade `start`; `api.rs`) write
durable intent and execute via the worker. The silent
`effect_controller.unwrap_or(...)` fallback at `process_runners/control.rs` is
gone — out of turn there is no turn-scoped controller, so the host's explicitly
named runtime effect controller is used for the effect that registers the row. A
`Start` only *registers* the row (the inline off-lease `tokio::spawn` is
deleted), and `ProcessCommandRunner::run` drives the host's
`ProcessWorkDriver` after a successful `Start`. The driver is wired by the
host through one of two facade sources: `.process_registry(...)` selects the
inline source — the default driver's `DurableProcessWorkerConfig` is built
eagerly at `build()` (failing loudly with
`EmbedError::ProcessRegistryRequiresStoreFactory` when no session store
factory is wired) and the inline driver is constructed lazily exactly once on
the first `session().open()` that needs it — while `.process_work_driver(...)`
installs an externally owned driver whose registry becomes the core's process
registry, so no inline driver is constructed. The facade wraps raw inline
registries once with the watched-registry decorator so `env.process_registry`
and `driver.process_registry()` refer to the same decorated state store. A Restate deployment builds a
`RestateProcessDeployment` and passes its `process_work_driver()` into the core
(`lash-restate/src/lib.rs`).

An emitter-supplied durability API was **not** added — the worker owns execution
durability, not the emitter.

Intentional cancellation now follows the host-owned ability seam instead of the
durable-start path. `ProcessCancelAbility` receives typed cancel requests with a
source (`HostApi`, `Tool`, or `Lashlang`) and reason; cancel-all first lists live
visible handles, then calls the same cancel-summary path for each. Handle
revocation and process cancellation remain explicit operations rather than a
combined unreferenced-handle cleanup request.

### Subagent collapse

A subagent spawn is now itself a process that runs a child lash session,
collapsed onto the generic `ProcessInput::SessionTurn` primitive
(`process_runners/session.rs::run_process_session_turn`). `agents.spawn`
(`crates/lash-subagents`) builds a `SessionCreateRequest` (capability → policy,
seed → initial nodes, RLM termination → plugin options, depth ceiling →
hidden tools) plus a `TurnInput`, and emits one `ProcessInput::SessionTurn` —
all deltas are request **config**, not new structure. The bespoke parallel
child-creation path (`rlm.rs::run_child_session`, the dead
`SubagentSessionConfigurator` / `NoopSubagentSessionConfigurator`) is deleted.
Because children now flow through the same generic path Phases A/B made durable,
they inherit provider re-supply, durability, and recovery — which is what
dissolves the `rlm_spawn` provider-inheritance failure: there is no bespoke
route left to drop the live provider handle. Recursion safety
(`MAX_SUBAGENT_DEPTH`, tool-hiding at the depth ceiling) and the `submit_error`
terminal are carried as request config / tool-access, not lost.

## References

- `crates/lash-core/src/runtime/process_worker.rs` — `DurableProcessWorker`:
  `drive_pending_processes`, `ensure_durable_store_facets`, `ensure_stable_process_id`,
  lease-renewing run.
- `crates/lash-core/src/runtime/process_work_driver.rs` — `ProcessWorkDriver`
  (`claim_and_run_pending`, driven on start / on open), `ProcessRunHandle` /
  `InlineProcessRunHandle`.
- `crates/lash/src/core.rs` — `InlineWorkDriverSlot` / `ProcessWorkDriverSetup`
  (lazy default-driver construction on first open),
  `.process_registry(...)` / `.process_work_driver(...)`.
- `crates/lash-core/src/runtime/process/model.rs` — `ProcessLease` /
  `ProcessLeaseCompletion`, typed `ProcessExecutionEnvSpec`.
- `crates/lash-core/src/runtime/process/validation.rs` — start-time and
  worker-time Lashlang Host Requirements validation from captured Process Plugin
  Options.
- `crates/lash-core/src/runtime/process/registry.rs` — `ProcessRegistry`
  `list_non_terminal()` and the lease ops; state only, no wait loops.
- `crates/lash-core/src/runtime/process/awaiter.rs` — `ProcessChangeHub`,
  watched-registry decorator, `ProcessAwaiter`, and `ProcessAttach`.
- `crates/lash-core/src/runtime/session_manager/process_runners/control.rs` —
  explicit-controller routing (the silent fallback removed), register-and-poke
  seam after a successful `Start`, + out-of-turn idempotency comment.
- `crates/lash-core/src/runtime/process/service.rs` —
  `ProcessService::start_from_request`, `ProcessCancelAbility`,
  `ProcessCancelAllRequest`, and typed cancel summaries.
- `crates/lash-core/src/runtime/effect/executor.rs` —
  `InlineEffectHost` / inline controller (stateless; the off-lease
  `tokio::spawn` deleted, `Start` only registers the row, cancel is a durable
  event append).
- `crates/lash-core/src/runtime/effect/controller.rs` —
  `EffectReplayOwnership` propagation through scoped controllers.
- `crates/lash-core/src/store/mod.rs` — committed session state and turn commit stamps
  (the model the process lease mirrors).
- `crates/lash-subagents/src/rlm.rs` — `agents.spawn` emitting
  `ProcessInput::SessionTurn`.
- `crates/lash-restate/src/tests.rs` —
  `sqlite_trigger_started_process_recovered_after_worker_registry_reopen` and
  `sqlite_process_recovery_reopens_registry_worker_grants_wakes_and_cancel`.
- `crates/lash-core/src/testing/conformance/` — process-lease single-owner /
  fencing conformance suite (`process_registry.rs`), plus the ADR 0019 cases:
  sweep obeys disposition, Abandoned requires death evidence or a lapsed-lease
  reconciled request, a revenant's lease-fenced writes are rejected, and owner
  drain terminalizes inline.
- `docs/adr/0019-process-recovery-obeys-declared-disposition.md` — the ratified
  contract: required `RecoveryDisposition`, the `Abandoned` terminal, single
  writer per path, and elapsed-time-never-terminalizes.
- `crates/lash-core/src/runtime/process/model.rs` — `RecoveryDisposition`, the
  durable `first_started` fact, and `AbandonRequest`.
- `crates/lash-core/src/runtime/process/events.rs` —
  `ProcessTerminalState::Abandoned`, `AbandonWriter`
  (`OwnerDrain | Sweep | ReconciledRequest`), and `AbandonEvidence`.
- `crates/lash-core/src/runtime/process/observation.rs` — `ObservedProcess`
  exposing `disposition`, `first_started`, `lease_holder`, `lease_expires_at_ms`,
  and `abandon_request`.
- `crates/lash-core/src/runtime/process_worker/mod.rs` — the disposition-driven
  `recover_process` verdicts and `drain_owner_bound_work`.
- `crates/lash-core/src/runtime/process/registry.rs` — `record_first_started`,
  `request_process_abandon`, and `get_process_lease` (state-only).
- `crates/lash/src/process_admin.rs` — the global `Processes` facade with the
  `granted_to` / `originated_by` filters and `request_abandon` / `prune`.
- `crates/lash-tools/src/shell/mod.rs` / `shell/runtime.rs` — `shell.start`
  `detach: true` (double-fork/`setsid`, `ExternallyOwned` row terminal at birth)
  and the `ShellProcessTable` teardown SIGKILL.
