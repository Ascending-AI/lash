# 0106: Durable formats use migration, drain or coexistence

## Status

Accepted. The pre-1.0 version freeze applies: stored shapes change in place
without version bumps or upcasters. The 1.0 release boundary is governed by
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).

## Context

A rolling PostgreSQL deployment shares stored rows between builds, and nodes
of both builds claim actors from one store. Nothing re-runs against a recorded
history ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §2),
so changing kernel code never needs a drain; only a durable format a node
cannot decode does. SQLite storage belongs to one host and upgrades by closing
the serving build before the next one opens the database. Compatibility needs
distinct rules for shared values, immutable history, identities and in-flight
state.
The current code supplies those mechanisms and proves upgrades through the
synthetic-next tier; the freeze supplies no compatibility between arbitrary
pre-1.0 builds.

## Decision

The registered `UpgradePolicy` values distinguish migration, drain and
coexistence. Migration converts stored meaning through schema steps or read
upcasters. Drain finishes in-flight state on a node that decodes its format. Coexistence admits
both supported forms across a compatibility window. A policy declaration is
not evidence that every decoder already reads a predecessor: the decoder's
registered range and upcaster rows decide what it accepts.

Evidence: `crates/lash-core-execution/src/engine/contracts.rs`,
`crates/lash-core-store/src/compat.rs`,
`scripts/discover_version_surfaces.py`.

### 1. Long-running work and drain by release

A build's format set names every durable format it decodes: turn checkpoints,
VM continuations and snapshot blobs, the cell envelope used as snapshot data,
Run record bodies, wait rows, engine-state format ids and outcome materials.
Each format declares its version surface under
[ADR 0131](0131-durable-types-declare-their-version-surface.md). A node
records its decodable set in `lash_nodes.formats`; an actor records the format
set of its state in `lash_actors.formats`. A claim takes only actors whose
formats the claiming node decodes. An actor no live node can decode stays
visible and can be cancelled without decoding its payload (ADR 0132 §11). The
build's `SessionAdmissionWindow` (its supported session-state range and every
writer pin the recorded `F` could select, FIG-4454) and its ordered plugin
composition (each registered plugin's id and declared behaviour revision, in
hook order, FIG-4744) are part of what it decodes. The fleet epoch `F`,
described in §2, selects durable writer formats.

A build drains by release. An operator marks a node draining, and the node:

1. stops claiming;
2. finishes each actor it owns to its next committed phase, or to a snapshot
   for a VM;
3. releases it to `ready`.

Nodes of the new build then claim the released actors, under the claim filter
above. The drain moves no work by hand and parks nothing: a released actor
resumes from committed state on whichever node claims it. A VM continuation
bound to an executable identity whose bytecode contract changed finishes on a
node of the old build or ends `Abandoned { ResumeRefused }`.

Drain status counts the actors a draining node still owns, and the actors in
the store whose formats only draining nodes decode. Parked work requires a
decoding node or an operator control decision.

Evidence: `crates/lash/src/formats.rs`,
`crates/lash-core/src/runtime/shift/admission.rs`,
`crates/lash-core-store/src/store/state_version.rs`. The substrate lanes
implement the format set, the claim filter and drain by release.

### 2. Shared rows: the fleet format and finalize

`F` is the durable release compatibility epoch in the fleet-format row.
`FleetFormat::writer_version` maps a surface to the format that epoch selects.
Writers use that selection; readers accept the surface's supported window.
The normal build's writable epoch range is one epoch. Synthetic-next admits
the predecessor epoch and its own, so upgrade tests can prove read-both and
write-old behavior instead of checking equal constants.

Finalize closes the rollback window by moving `F` to the finalizing build's
epoch. It requires that no live node lacks the newer formats: a newer format
is never written while a node of the older build is live. An unread node table
refuses the operation. PostgreSQL's automatic mode also refuses an operator hold; its
explicit `--override-hold` mode bypasses only that hold. The fleet-row transaction
moves `F` and fences stale writers. PostgreSQL finalize also runs eligible
backfills.

The host owns the rollout and calls finalize as its final drain operation.
`lashctl finalize` exposes the PostgreSQL operation. A status read does not
schedule an automatic finalize. SQLite's schema migration runs on open;
finalizing its fleet epoch is a separate store operation with no operator hold.
It commits in one transaction of the one database, so there is no partial
transition to complete. A build whose
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

### 3. Object state outside the store

Retired: no durable state lives outside the lash store. Wait rows and source
seals are store rows under ADR 0132 §6, versioned as surfaces under §4.

### 4. Per-surface policy

Constants declare their upgrade policies with `version_surface` and their
manifest rows with `format_manifest` in source. Their `version_guard` markers
name the guarded shapes and files. `scripts/versioned-surfaces.toml` retains
class exclusions, admission floors, constants that do not version durable
formats, and permanently reserved retired hash domains.
`scripts/check_format_registry.py` checks the declaration against the typed
manifests. Engine-state formats are declared by their process engines.

| Stored shape | Current compatibility mechanism |
|---|---|
| SQL schema and component stamps | Compatibility descriptors and explicit schema runners; PostgreSQL migrates before worker open, SQLite at store-set open. |
| Session-state marker | Epoch-fenced admission reads the marker and applies its fleet read window. |
| Mutable payloads | Surface read ranges, registered upcasters and `F`-selected writer versions. |
| Immutable, hash-addressed history | Decode the admitted range and lift in memory without rewriting the stored bytes or identity preimage. |
| Derived workflow graph and type facets | Their declared read ranges and projection policy. |
| Content addresses and idempotency families | Preserve stored identity preimages; admit the declared family rather than re-derive an old identity with a new family. |
| Turn checkpoints, VM snapshots, Run records, wait rows and engine state | Their declared payload read range, the claim filter and drain by release (§1). |
| Live remote wire | Negotiated or declared wire read/write windows, separately from stored-value versions. |
| Release fixtures | Capture by release tag; synthetic-next supplies the current upgrade proof. |

Session-state admission validates the session actor's epoch in the store
transaction, reads the independent version marker, and returns the session id,
version and epoch. It runs no per-session converter chain and advances no marker.
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

When migration is required, SQLite store-set open migrates its one database
after a complete backup under its migrator lock (ADR 0132 §12). An interrupted
migration completes or restores from its manifest and stamps.
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
add to the process total. A rollback choreography can retain three builds,
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
separate binaries and tests expanded-store rollback, drain by release,
finalize racing writers, immutable history after finalize, negotiated wire,
retention delivery and skipped-release refusal. The rolling harness exercises
PostgreSQL overlap and SQLite stop-then-start, including rollback, drain,
hold, finalize and contract. It tests the synthetic release window, not arbitrary
pre-1.0 binary compatibility. The multi-node leg (`just e2e-rolling-cluster`)
runs the same choreography under load on the Helm load topology, on demand
rather than per PR (ADR 0115 §6).

Storage laws run against SQLite file, SQLite memory and PostgreSQL. Laws run
the production runtime over a fault-injecting store with labelled commits, a
virtual clock and `SimNodes` (ADR 0132 §14). The drain-by-release law cuts at
every commit of a release and requires each released actor to be claimed by a
new-build node with no `Once` body started twice. The claim-filter law
requires that a node lacking a format never claims an actor stored in it.
Format-registry checks validate policy declarations; release fixture capture
writes `fixtures/release/<tag>/`.

Evidence: `crates/lash-upgrade-harness/tests/phase_a/main.rs`,
`crates/lash-upgrade-harness/tests/rolling/main.rs`,
`scripts/capture_release_fixtures.py`,
`scripts/check_format_registry.py`.

### 7. What stays fail-closed

Startup and decoder admission refuse unsupported component and surface ranges,
unregistered predecessor conversions, malformed compatibility records and
integrity failures. Synthetic-next tests prove a skipped compatibility release
refuses. A node never claims an actor whose formats it cannot decode.
Finalize refuses while a node lacking the newer formats is live, on an unread
node table, or on a hold in automatic mode. A stale writer refuses after the fleet
epoch leaves its writable range. These are typed outcomes; missing evidence
is not permission to mutate or decode under another contract.

Evidence: `crates/lash-core-store/src/compat.rs`,
`crates/lash-upgrade-harness/tests/phase_a/skipped_compatibility_release_refused.rs`.

### 8. The release boundary

The current tree carries compatibility descriptors, writer pins, migration
runners and fixture capture. Synthetic-next supplies a
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

Patch markers and generation lanes keep old code alive for recorded histories;
with no replay there is no history to keep code for, so only formats drain.
Worker-boot DDL races mixed-version workers, so PostgreSQL uses one explicit
runner.

## Consequences

A supported upgrade needs its declared read/write window and conversion or
drain path. The rollback boundary is the fleet-epoch flip, which requires that
no node of the older build is live. Operators run schema migration, drain by
release and finalize in that order. Immutable history retains its bytes and identities. The current
upgrade evidence comes from synthetic-next; the version freeze does not
promise migration or rollback between arbitrary development builds.
