# 0106: Durable formats use migration, drain or coexistence

## Status

Accepted. The pre-1.0 version freeze applies: stored shapes change in place
without version bumps or upcasters. The 1.0 release boundary is governed by
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).

## Context

A rolling PostgreSQL deployment shares stored rows and Restate object state
between builds, while a journal must replay under compatible execution code.
SQLite storage belongs to one host and upgrades by closing the serving build
before the next one opens the store set. Compatibility needs distinct rules
for shared values, immutable history, identities and in-flight execution.
The current code supplies those mechanisms and proves upgrades through the
synthetic-next tier; the freeze supplies no compatibility between arbitrary
pre-1.0 builds.

## Decision

The registered `UpgradePolicy` values distinguish migration, drain and
coexistence. Migration converts stored meaning through schema steps or read
upcasters. Drain preserves a journal on its compatible code. Coexistence admits
both supported forms across a compatibility window. A policy declaration is
not evidence that every decoder already reads a predecessor: the decoder's
registered range and upcaster rows decide what it accepts.

Evidence: `crates/lash-core-execution/src/engine/contracts.rs`,
`crates/lash-core-store/src/compat.rs`,
`scripts/discover_version_surfaces.py`.

### 1. Long-running work and build generations

`G` is the build's drain generation. The core hashes the sorted drain-policy
format names and versions with `JOURNAL_LOGIC_EPOCH`, the build's
`SessionAdmissionWindow` (its supported session-state range and every writer
pin the recorded `F` could select, FIG-4454) and the ordered plugin
composition: each registered plugin's id and declared behaviour revision, in
hook order (FIG-4744). Two builds whose session admission differs therefore
never share a lane, so work sent on its opener's lane runs on a build that
admits every session its opener admitted; and two builds whose plugins differ
in any behaviour revision, or only in order, never share one either.
`G` exists once the core's plugins are registered: the core computes it when
it is built and binds it into the engine, and there is no other way to obtain
one, so no caller can open work on a lane no deployment serves.
The engine's journal-bearing services bind a stable
name and a generation name, `<Service>_g<G>`. Shared state services keep one
stable name. The fleet epoch `F`, described in §2, selects durable writer
formats independently of `G`.

A route is retained data beside a segment handover. Replay uses that route rather
than deriving it from the current caller. Restate keys workflow and idempotency
identity by service name, so recomputing a route can start different work. New
segment successors use the stable route to the latest build; incompatible
handovers retain their writer's generation route. Process terminals and source
seals use their shared stable services. The Run's ordinary tool records belong
to its owning handler's journal and generation.

Journal-bearing handlers check the recorded build generation before replaying
work. The session and turn handlers fold that sentinel into their first
recorded step. A foreign journal parks with `RetiredGeneration` rather than
running fresh effects. Process segments retain a bounded journal and hand over
continuation state, unresolved waits and ordinal state at a segment boundary.
There are no in-place patch markers.

An operator marks a generation draining. The recovery leader wakes its live
processes for handover through a distinct handoff arm, not cancellation.
Generation status counts store-tracked live and parked processes, parked and
in-flight turns, and closing sessions. A submitted successor remains counted
until its admission changes the process's generation. The engine's
`DeploymentRegistry` also counts `unfinished_invocations`
pinned to retained deployments, including paused work and old shifts. Short
source subscriptions transfer without leaving a pending waiter on the old lane;
unresolved transferred sources alone do not hold that deployment (L11). An
unreadable invocation count refuses drain or removal. Stalled obligations are
reported separately and do not hold the drain. These reads are not one atomic
snapshot or an enumeration of every engine invocation. Finalize additionally
checks retained deployments in §2. Parked work requires compatible replay or
an operator control decision.

A session shift's drain is bounded by one run per shift. Restate pins a
`LashSession` shift invocation to the build that started it, and a shift
admits one run after another, so a busy session would hold its pinned build
for a whole backlog. Every admission after the run a shift started on
therefore reads the drain mark of the build its invocation is pinned to,
inside the recorded `AdmitShift` step. Marked, with work pending, the step
admits nothing and records `AdmitVerdict::Draining` with that generation. The
shift then sends the rest to its continuation request under the stable name,
which the newest build serves, and stops `ShiftStop::Draining`; a waiter
follows the continuation. The run in flight always runs to its terminal
commit on its build. A replay decodes the recorded verdict and hands over
where the first execution did, whatever the mark says by then. A shift
resumed on a generation's lane executes the run it was resumed for and hands
over the same way, its lane continuations included. A shift's first admission
reads no mark: under the stable name a new invocation is already the newest
build's, so a drain never passes work back and forth. The in-process shift
loop is pinned to no build and reads none.

A run is stamped with the generation of the build that admits it, the one
serving the admission's shift invocation. The engine passes that generation
to admission separately from the wire request. A shift request carries only
its intended lane, with no generation suffix in its request id. So a run the
newest build admits from a hand-over counts in that build's in-flight turns and
parks
under its generation, and the draining generation counts only the run still
running on its own build.

Evidence: `crates/lash/src/formats.rs`,
`crates/lash-core/src/runtime/shift/admission.rs`,
`crates/lash-restate/src/session_shifts.rs`,
`crates/lash-restate/src/session_shifts/continuation.rs`,
`crates/lash/src/tests/drain_hand_over.rs`,
`crates/lash-core-store/src/store/state_version.rs`,
`crates/lash-restate/src/services.rs`,
`crates/lash-restate/src/deployment_registry.rs`,
`crates/lash-restate/src/sentinel.rs`,
`crates/lash-core-store/src/store/generation_drain.rs`,
`crates/lash-restate/src/process/workflow.rs`,
`crates/lash-restate/src/tests/wait_handoff_generations.rs`.

### 2. Shared rows: the fleet format and finalize

`F` is the durable release compatibility epoch in the fleet-format row.
`FleetFormat::writer_version` maps a surface to the format that epoch selects.
Writers use that selection; readers accept the surface's supported window.
The normal build's writable epoch range is one epoch. Synthetic-next admits
the predecessor epoch and its own, so upgrade tests can prove read-both and
write-old behavior instead of checking equal constants.

Finalize closes the rollback window by moving `F` to the finalizing build's
epoch. It requires the retired generation to be marked drained and the engine
to retain no deployment serving it. An unread deployment registry refuses the
operation. PostgreSQL's automatic mode also refuses an operator hold; its
explicit `--override-hold` mode bypasses only that hold. The fleet-row transaction
moves `F` and fences stale writers. PostgreSQL finalize also runs eligible
backfills; object sweeps are a separate operation after finalize, under §3.

The host owns the rollout and calls finalize as its final drain operation.
`lashctl finalize` exposes the PostgreSQL operation. A status read does not
schedule an automatic finalize. SQLite's schema migration runs on open;
finalizing its fleet epoch is a separate store operation with no operator hold.
It seals a durable intent after checking retirement and before committing any
database. A fresh open completes that checked transition before admitting the
store; it does not decide a new retirement. A build whose
writable range excludes `F` refuses rather than writing another format.

The two upgrade values remain distinct: `UpgradePolicy` describes how a
surface crosses a release, while `FleetFormat` selects what a writer emits.
Rollback is supported while the expanded store and writer formats remain
inside the older build's read/write windows. After finalize fences it, the
older build cannot keep serving writes.

Evidence: `crates/lash-core-store/src/store/fleet_format.rs`,
`crates/lash-core-store/src/store/fleet_finalize.rs`,
`crates/lash-postgres-store/src/postgres/finalize.rs`,
`crates/lash-postgres-store/src/lib.rs`,
`crates/lashctl/src/main.rs`,
`crates/lash-sqlite-store/src/backend.rs`,
`crates/lash-sqlite-store/src/finalize.rs`.

### 3. Restate object state

The durable-wait registry and source seals live in Restate under stable object
keys. `LashDurableWaitIndex` and `LashDurableWaitWorkflow` retain their shared-state
compatibility rules. Stored-value format and handler wire are distinct registered
surfaces. The Rust registry trait is `LashDurableWaitRegistry`. Run aggregate
records and material references live in the opener journal rather than object
state in another service.

Stored values carry `{format, body}`. A supported predecessor is lifted by the
surface's registered upcasters; a foreign stamp refuses before the handler
acts. Writers stamp the format selected by `F`. Each object also retains a
`_compat` record: exclusive handlers check read and write admission, and shared
handlers check read admission. Clearing an object keeps that compatibility
record so a stale writer cannot recreate its values.

An exclusive `upgrade` handler rewrites one object's values and raises its
compatibility record when `F` selects that family's newest writer format.
The object preflight uses Restate SQL introspection to list older objects;
the sweep calls their handlers and reads
preflight again. Object state is the resumable cursor. Introspection measures
progress and supplies no writer fence. A family still writing its predecessor
refuses with `NotFinalized`; finalize selects its newest format. The operator
exposes these operations through `lashctl objects-preflight` and
`lashctl objects-sweep`.

Evidence: `crates/lash-restate/src/object_state.rs`,
`crates/lash-restate/src/object_upgrade.rs`,
`crates/lashctl/src/main.rs`.

### 4. Per-surface policy

Constants declare their upgrade policies with `version_surface` and their
manifest rows with `format_manifest` in source. Their `version_guard` markers
name the guarded shapes and files. `scripts/versioned-surfaces.toml` retains
class exclusions, admission floors, constants that do not version durable
formats, and permanently reserved retired hash domains.
`scripts/check_format_registry.py` checks the declaration against the typed
manifests. Engine formats are declared by
the engine; the facade includes them only when that engine is enabled.

| Stored shape | Current compatibility mechanism |
|---|---|
| SQL schema and component stamps | Compatibility descriptors and explicit schema runners; PostgreSQL migrates before worker open, SQLite at store-set open. |
| Session-state marker | Shift-fenced admission reads the marker and applies its fleet read window. |
| Mutable payloads | Surface read ranges, registered upcasters and `F`-selected writer versions. |
| Immutable, hash-addressed history | Decode the admitted range and lift in memory without rewriting the stored bytes or identity preimage. |
| Derived workflow graph and type facets | Their declared read ranges and projection policy. |
| Content addresses and idempotency families | Preserve stored identity preimages; admit the declared family rather than re-derive an old identity with a new family. |
| Effect, process, session-shift and group-dispatch journals | Drain generation, retained routes and generation sentinels. |
| Turn checkpoints, VM continuations and process handovers | Their declared payload read range and the writer generation of in-flight work. |
| Restate object values | Stored-value ranges, fleet-selected stamps and exclusive object upgrades. |
| Live remote and Restate wire | Negotiated or declared wire read/write windows, separately from journal and stored-value versions. |
| Release fixtures | Capture by release tag; synthetic-next supplies the current upgrade proof. |

Session-state admission validates the `ShiftFence` in the store transaction,
reads the independent version marker, and returns the session id, version and
shift epoch. It runs no per-session converter chain and advances no marker.
Recovery also checks the marker before guarded payload decoding. Each record
reader still enforces its own surface window, as specified by
[ADR 0077](0077-session-state-migrates-totally-at-admission.md).

Some surfaces have an exact range until a compatibility change supplies its
predecessor conversion. A registry policy does not grant blanket additive
compatibility. The freeze changes normal shapes in place; synthetic-next
widens the selected ranges and registries for executable upgrade evidence.

Evidence: `scripts/discover_version_surfaces.py`,
`scripts/check_format_registry.py`,
`crates/lash/src/formats.rs`,
`crates/lash-core-store/src/store/fleet_format.rs`,
`crates/lash-core-store/src/store/state_version.rs`,
`crates/lash-sqlite-store/src/persistence/session_commit.rs`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs`,
`crates/lash-core-store/src/store/persisted_state_tests.rs`,
`crates/lash-upgrade-harness/tests/phase_a/history_after_finalize.rs`.

### 5. PostgreSQL and SQLite schema changes

PostgreSQL workers verify schema compatibility and run no DDL on open.
`lashctl migrate` owns the schema advisory lock and migration ledger.
Expand runs before a roll. Backfill runs after its required fleet epoch and
commits bounded row batches with their cursor. Contract requires its fleet
epoch and completed backfills, then raises the reader floor. A rerun resumes
from the ledger. A semantic column or key change uses a declared migration;
stored identities are not recomputed as a side effect of opening a worker.

`lashctl migrate` and its dry run acquire the published, database-wide schema
advisory lock with a server-enforced 30-second lock-wait bound. Expand and
contract take it exclusively; planning and backfill catalog reads take it
shared. A concurrent holder queues the command in PostgreSQL. A holder that
keeps the lock past 30 seconds produces the typed `Contended` store error;
there is no sleep/retry loop. A slow query without lock contention does not
produce `Contended` through this acquisition path.

The runner temporarily sets `lock_timeout = '30s'` and disables
`statement_timeout` only during advisory-lock acquisition, then restores the
connection's inherited settings before reading or changing the catalog. The
bound covers each advisory acquisition, not the whole migration or connection
establishment. DDL and row locks retain the deployment's own timeouts and may
also report contention. The lock lives on a detached connection, which closes
on completion, error or cancellation; a cancelled waiter cannot return a
locked connection to the pool. Worker timeouts and schema verification are
unchanged.

When migration is required, SQLite store-set open migrates all three databases
after a complete backup under its migrator lock. Each database commits its own
step in store-set order.
An interrupted migration completes or restores from its manifest and stamps.
An open that cannot obtain exclusive migration ownership refuses typed,
including `MigrationOpenElsewhere`. A component opened independently verifies
its stamp and does not migrate. SQLite upgrades are stop-then-start; live
mixed-version overlap belongs to PostgreSQL.

A PostgreSQL roll declares its connection budget:

```
peak = processes_per_generation * pool_max * overlapping_generations
     + workers + admin_headroom
```

Shared `PostgresStorage` clones use the same pool; independently opened pools
add to the process total. A rollback choreography can retain three generations,
so its declaration budgets three. `lashctl preflight` accepts all five budget
terms and checks the live server's capacity and reserved connections before
the roll. The [rolling runbook](../../runbooks/rolling-upgrade/runbook.md#postgresql-connection-budget)
states the operator declaration.

Evidence: `crates/lash-postgres-store/src/postgres/migrate.rs`,
`crates/lash-sqlite-store/src/migration.rs`,
`crates/lash-sqlite-store/src/backend.rs`,
`crates/lashctl/src/main.rs`.

### 6. Tests and gates

Upgrade proofs use synthetic-next alongside the normal build. Phase A runs
separate binaries and tests expanded-store rollback, generation handoff,
finalize racing writers, immutable history after finalize, negotiated wire,
object-sweep crash resume, retention delivery and skipped-release refusal.
The rolling harness exercises PostgreSQL overlap and SQLite stop-then-start
against live Restate, including rollback, drain, deployment removal, hold,
finalize and contract. It tests the synthetic release window, not arbitrary
pre-1.0 binary compatibility. The multi-node leg (`just e2e-rolling-cluster`)
runs the same choreography under load on the Helm load topology, on demand
rather than per PR (ADR 0115 §6).

Storage laws run against SQLite file, SQLite memory and PostgreSQL. Execution
hosts are the in-process Restate server double, live Restate and lash-sim's
in-process effect host. The handoff laws check signal and event delivery across
segment transitions. Format-registry checks validate policy declarations;
release fixture capture writes `fixtures/release/<tag>/`.

Evidence: `crates/lash-upgrade-harness/tests/phase_a/main.rs`,
`crates/lash-upgrade-harness/tests/rolling/main.rs`,
`crates/lash-restate/src/tests/wait_handoff_generations.rs`,
`scripts/capture_release_fixtures.py`,
`scripts/check_format_registry.py`.

### 7. What stays fail-closed

Startup and decoder admission refuse unsupported component and surface ranges,
unregistered predecessor conversions, malformed compatibility records and
integrity failures. Synthetic-next tests prove a skipped compatibility release
refuses. A foreign execution generation parks rather than issuing fresh work.
Finalize refuses an undrained generation, a retained deployment, an unread
registry or a hold in automatic mode. A stale writer refuses after the fleet
epoch leaves its writable range. These are typed outcomes; missing evidence
is not permission to mutate or replay under another contract.

Evidence: `crates/lash-core-store/src/compat.rs`,
`crates/lash-core-store/src/store/fleet_finalize.rs`,
`crates/lash-restate/src/sentinel.rs`,
`crates/lash-upgrade-harness/tests/phase_a/skipped_compatibility_release_refused.rs`.

### 8. The release boundary

The current tree carries compatibility descriptors, writer pins, generation
routing, migration runners and fixture capture. Synthetic-next supplies a
successor format and schema for proving them. The normal build remains under
the version freeze. The 1.0 cut owns the release baseline, strict version and
upgrade gates, the baseline migration catalog, fixture capture at `v1.0.0`,
and the fixture read-back target. ADR 0115 specifies that cut; the presence of
its mechanisms here does not make a pre-1.0 build a compatibility release.

Evidence: `crates/lash-core-store/src/store/fleet_format.rs`,
`crates/lash-sqlite-store/src/migration.rs`,
`crates/lash-postgres-store/src/postgres/migrate.rs`,
`scripts/capture_release_fixtures.py`.

## Rejected alternatives

Patch markers require old journals to replay under changed code. Generation
routing preserves their execution contract instead. Versioned object
namespaces split shared wait and group state; stable keys and versioned values
keep its coordination intact. Moving that state to SQL needs separate
idempotency and wake delivery around each Restate call. Worker-boot DDL races
mixed-version workers, so PostgreSQL uses one explicit runner. Heartbeat expiry
cannot prove a deployment cannot execute a pinned journal; finalize checks
retained deployments.

## Consequences

A supported upgrade needs its declared read/write window and conversion or
drain path. The rollback boundary is the fleet-epoch flip, and generation
retirement requires both drained work and deployment removal. Operators run
schema migration, generation drain, finalize and eligible object sweeps in
that order. Immutable history retains its bytes and identities. The current
upgrade evidence comes from synthetic-next; the version freeze does not
promise migration or rollback between arbitrary development builds.
