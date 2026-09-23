# 0102: Zero-infra is a SQLite in-memory deployment; every host journals; one substrate per deployment

## Status

Accepted 2026-09-23 (FIG-3574) as the design freeze for arc FIG-3573. **Not yet
implemented**: FIG-3575 through FIG-3584 build it, and FIG-3585 is the cut that
deletes the superseded code and rewrites the passages listed at the end. Nothing
below describes current behaviour unless it says so.

The rulings are Sam's, recorded on FIG-3573 on 2026-09-23. The evidence is a
targeted prospect round over seven references read from local clones, every
claim verified: `/workspace/notes/lash/prospect-native-host-2026-09-23.md`.

## Context

`CONTEXT.md` says the Substrate Contract has exactly two implementations: the
Store-Backed Effect-Replay Driver and an Engine-Backed Host. Lash ships three.
The third is the zero-infra path, and it is the default for the quickstart, the
examples and most tests:

- **The native effect host.** `NativeEffectHost`, `NativeRuntimeEffectController`
  and `NativeEffectGroups` keep no journal and implement effect groups a second
  time: group table, dispatch, reopen, close and finalize, reap, rank
  allocation, drain barrier, admission fence, quiescence and more, at least
  fifteen concepts. `native_controller.rs` calls itself "the conformance
  definition of the contract's observable semantics" and "the in-memory twin of
  the SQL tiers' four-step finalization", while ADR 0099 §14 claims "one
  semantic path on every tier".
- **`lash-core-memory`**, a hand-written session store of 9.4k lines, whose
  divergence from the SQL stores produced FIG-3217, 3523, 2850, 2884, 2840,
  1840, 1841, 1780, 1697, 3231, 1515 and 780.

The tier difference is observable, not internal. Under `EffectJournaling::Journaled`
a before-LLM failure and a controller error return `Err` where `Local` records a
failed turn; an aborted direct turn is never re-driven and its input is swept
into the session's next turn; a deterministic failure in a queued run retries
eight times and stays pending. That is live today on `SqliteEffectHost`.

Two more shapes exist only to serve the native path. SQL session stores are
paired by hand with the non-journaled host, which needs store-delegated turn
control (`store_turn_control.rs`, `TurnControlAuthorityOwner`). And in-memory
persistence is a silent default: the facade and `EmbeddedRuntimeBuilder` fall
back to `InMemoryTriggerStore`, and `RuntimeServices::new` and
`PersistentRuntimeServices::new` fall back to the in-memory attachment and
process-exec-env stores.

## Decision

### D1. Every host journals

`EffectJournaling { Local, Journaled }` is deleted, with every `Local` arm, the
`ProcessLifetime` completion-key route and `allow_process_lifetime_completion_keys`.
ADR 0045's last permitted tier question goes with it. There is no host that runs
effects without recording them.

### D2. One substrate per deployment

A deployment is one value that supplies every persistence port and the effect
host: SQLite (file or memory), Postgres, or Restate. What D2 forbids is
non-journaled effects, and ports assembled by hand from different substrates. No
API accepts a mixed set.

A Restate deployment is the Restate engine host over one SQL store set. Restate
journals effects and stores no sessions, so engine-plus-SQL is one deployment,
not a mixture, and D2 does not forbid it. The pairing of SQL session stores with
a non-journaled effect host is deleted, with the machinery that exists only for
it: `store_turn_control.rs`, the store turn-cancellation-authority seam,
`TurnControlAuthorityOwner` and the `HostOwned` turn-control binding.

### D3. Zero-infra is a SQLite in-memory deployment

Zero-infra is SQLite in memory for every persistence port SQLite implements:
sessions, effects, process registry, triggers, process definitions, process exec
env and attachments. It runs the same `StoreEffectReplayDriver` over the same
SQLite store as a file deployment. `lash-core-memory` and the `InMemory*`
persistence stores (attachment, process-exec-env, trigger, process-definition
registry, `TestLocalProcessRegistry`) are deleted. The live-replay stream buffer
stays, because it is observation, not persistence.

**The memory form is named `memdb` databases, not `:memory:`.** A deployment
opens many connections that must reach each other by name: one core connection
per session, the effect driver and its closure lifecycle, the registry, the
effect journal that ATTACHes the registry for scope fences, and the retention
sweep. A plain `:memory:` database is private to its connection. Shared-cache
`mode=memory` fails a contending `BEGIN IMMEDIATE` at once instead of waiting on
the busy handler. The `memdb` VFS is shared by name, can be ATTACHed by URI, and
makes a contending writer wait.

- Each of the four databases (core, effects, registry, triggers) keeps its own
  schema version and is opened as `file:/lash-<uuid>/<db>?vfs=memdb`.
- The deployment holds one **anchor connection** per database for its lifetime.
  A `memdb` database disappears with its last connection, so the anchors are
  what keep it alive, and dropping the deployment releases them.
- The deployment's identity is `sqlite-memory:<uuid>`, beside `sqlite:<canonical
  root>` for a file deployment. That one identity drives the turn-control
  binding, the key of the journal notifiers the replay driver parks on, and the
  registry and retention ATTACHes.
  The store and the host no longer mint their own. `validate_effect_host_path`
  keeps refusing raw `:memory:` and `file:` strings; the typed location is the
  only way in.
- A memory deployment's effect host issues completion keys, and they resolve for
  the deployment's lifetime. There is no process-lifetime opt-in.

A fresh four-database memory deployment costs about 5–6 ms with full DDL, and a
single-row write transaction about 6 µs.

### D4. Names follow the substrate; one cutover

There is no "native" effect host. The zero-infra entry point is
`lash::sqlite::SqliteDeployment::memory()`, beside `open(root)` and
`memory_with_clock(..)`. The in-process worker drivers (`NativeSubstrateConfig`,
`NativeQueuedWork`, `NativeProcessWork`, `with_native_queued_work`) are not
effect hosts; they drive queued and process work in process over whichever
deployment is configured, and they keep their names. The cutover happens once,
with no aliases, forwarding constructors, dual paths or legacy mode.

### The `Deployment` trait

A `Deployment` trait in lash-core-execution is the one value from which a
runtime takes every persistence port and its effect host: the session-store
factory, the effect host, the process registry, the trigger store, the
process-definition registry, the process-exec-env store, the attachment store,
and the binding identity.

- **`SqliteDeployment`**, file or memory, supplies all of them from one typed
  location, including a SQLite attachment store over the core database.
- **The Postgres deployment** takes an attachment backend at construction.
  Postgres implements no `AttachmentStore`, and a deployment with no attachment
  port is not a deployment.
- **The Restate deployment** is the Restate host over one SQL store set.

The runtime builder takes the deployment as a required argument, so a build
without one cannot be written, and the per-port setters are gone. No in-memory
default exists anywhere: every fallback listed in *Context* is deleted.

The kernel names no concrete store and no concrete effect host.
lash-sqlite-store, lash-postgres-store and lashlang depend on lash-core-execution
and lower crates, not on lash-core, so lash-core and lash-core-worker tests can
use a SQLite memory deployment as a dev-dependency without a cycle.

### Failure settlement is classified by cause, on every host

Turn-failure settlement is a rule about the cause of the failure, not a question
about the tier (FIG-3575):

- A deterministic failure, an outcome over journaled inputs, is recorded as a
  failed turn.
- Only a live fault, one a re-drive can fix, aborts with `Err`. An aborted direct
  turn keeps its claim, binds it to its turn, and returns its acceptance receipt;
  its input is never folded into a later turn. Only a re-drive of that turn or a
  cancel by the receipt consumes it ([ADR 0069](0069-durable-acceptance-is-the-sole-turn-ingress.md)
  §7, FIG-3589).
- A queued run never retries a deterministic failure in a loop. It settles once;
  the retry budget applies to live faults only.

This is what makes D1 safe: once no failure path reads `EffectJournaling`,
deleting `Local` changes nothing a host can observe.

### The `sqlite` feature stays optional, with no default

The facade's `sqlite` feature stays optional and `default = []` stands (ADR
0079). Postgres- and Restate-only embedders do not compile SQLite. `lash::testing`
never pulls in SQLite: a fixture that needs a deployment takes one, and the
SQLite memory deployment is reached through `sqlite`, not through `testing`.

### Conformance

The SQLite suite runs on both a file and a memory deployment. The `in_memory`
conformance module is gone. `store_contract_state_machine` keeps its independent
reference model; its memory backend becomes the SQLite memory deployment.
lash-sim's memory world is the SQLite memory deployment under `SimClock`.

ADR 0044's main complaint is closed: the default test host replays.

## Open design risk: memdb readers block on a writer

`memdb` has no WAL. A reader blocks while any writer holds a write transaction
(`memdbLock` in the bundled `sqlite3.c`). Any path that holds a transaction on
one connection while waiting on another connection will stall or time out on a
memory deployment where it passes on a file one. Lanes must respect this: when
the memory conformance run exposes such a wait, fix the ordering in the store.
Do not raise the busy timeout to hide it and do not special-case memory.

## What is deliberately not adopted

Every reason below is verified in the prospect notes.

- **A HashMap `EffectReplayRowStore`.** A third row store re-deriving 26 ordered
  atomic operations. That is the Durable Task emulator's drift shape (locks
  never expire, history read throws `NotSupported`, 5 of its 10 functional tests
  commented out), and `lash-core-memory`'s own pattern.
- **Keeping `Local` as a driver mode**, in the style of Golem's
  `AgentMode::Ephemeral`. It is a tier flag, which ADR 0045 forbids.
- **A separate fast fake**, in the style of Temporal's SDK test environment,
  which "never replay[s]" and hand-copies server rules such as
  `ValidateRetryPolicy`. The fakes are where the drift shows.
- **File-only SQLite for zero-infra**, as in Golem's local mode. Too heavy for
  the quickstart and unit tests.

The consensus runs the other way. Nobody ships a second hand-written
implementation of the semantics as the zero-infra path. Temporal's `start-dev`
and Inngest's `inngest dev` run the production engine over in-memory SQLite;
DBOS runs one implementation over SQLite or Postgres; Golem and Restate swap
storage behind one trait.

## Consequences

- **Embedders.** Zero-infra is `lash::sqlite::SqliteDeployment::memory()` and
  needs `features = ["sqlite"]`. The builder takes the deployment as an
  argument.
- **Failures.** Deterministic failures are still recorded as failed turns. Live
  faults return `Err` carrying the acceptance receipt.
- **Completion keys** on a memory deployment resolve for the deployment's
  lifetime. ADR 0099 W18 already promises nothing past process death.
- **The durable-admission gate** now applies to zero-infra sessions, because
  every session has a store. FIG-3416 becomes user-visible on the quickstart.
- **FIG-3546** is closed by deletion: reopen reads the journal.

## Superseded by this ADR on landing of FIG-3585

These passages describe the native tier, `EffectJournaling` or store-delegated
turn control. They stay as written until FIG-3585 rewrites them in the change
that deletes what they describe.

- ADR 0045:78-81, the `EffectJournaling` allowed difference, plus the stale
  `supports_concurrent_effects` at :80.
- ADR 0099 §14 (:1037-1070), :63, :86, :375-377, :728, :839, the W18 row at
  :1097, and the stale `supports_concurrent_effects` at :1109.
- ADR 0039:32-35 and :69-70, the store-delegated turn-control aliases.
- ADR 0044:63-67.
- ADR 0025:12, 24, 50, 57, 84.
- ADR 0094:246.
- ADR 0012:74, ADR 0065:46, 146 and 504, ADR 0047:78.
