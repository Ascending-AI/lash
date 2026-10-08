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

A build's format set names every durable format its actors' state holds:
turn checkpoints, the session-state generation, VM continuations and
Lashlang snapshots, the RLM cell envelope used as snapshot data, Run record
bodies, wait rows, outcome materials and each process engine's state format.
Each format declares its version surface under
[ADR 0131](0131-durable-types-declare-their-version-surface.md). A set is
spelled canonically from its formats' ids and versions, per actor kind: one
session set, and one set per process engine. A node records every set it
decodes in `lash_nodes.formats_json`; an actor records the set its state is
in, in `lash_actors.formats`. A process starts in its engine's unstarted set,
which any node with the engine decodes, and its first transition stamps the
engine's state set. A claim takes only actors in a set the claiming node
decodes. An actor no live node decodes stays visible and ready; a pending
cancel lets any node claim it to end it without decoding its state (ADR 0132
§11).

A build drains by release. An operator starts a node's drain, and the node:

1. records `lash_nodes.draining` and stops claiming;
2. finishes each actor it owns to its next committed phase: a session stops
   before its next model call or cell, a process once its running steps have
   committed their outcomes;
3. releases it `ready` under `drain.release`, and stops when none is left.

Nodes of the new build then claim the released actors, under the claim
filter above. The drain moves no work by hand and parks nothing: a released
actor resumes from committed state on whichever node claims it. A VM
continuation bound to an executable identity whose bytecode contract changed
finishes on a node of the old build or ends `Abandoned { ResumeRefused }`.
Plugin composition is not part of the session set: plugin state is refused
or admitted at session admission, so a plugin roll needs no drain.

Evidence: `crates/lash-durable/src/formats.rs`,
`crates/lash-core-execution/src/formats.rs`,
`crates/lash/src/formats.rs` (`actor_state_surfaces`),
`crates/lash-durable/src/runner.rs`,
`crates/lash-durable/src/laws/formats.rs`,
`crates/lash-durable-test/tests/drain_by_release.rs`.

### 2. Shared rows: the fleet format

`F` is the durable release compatibility epoch in the fleet-format row.
`FleetFormat::writer_version` maps a store-resident surface to the format
that epoch selects. Writers use that selection; readers accept the surface's
supported window. The normal build's writable epoch range is one epoch.
Synthetic-next admits the predecessor epoch and its own, so upgrade tests can
prove read-both and write-old behavior instead of checking equal constants.
The release cut owns moving `F` (ADR 0115).

Actor state follows the same rule through the node table: a writer that knows
several sets for an actor writes the newest one every live node serving such
actors decodes (`fleet_writable` over `live_decodes`), so a newer format is
never written while a node of the older build is live.

A build whose writable range excludes `F` refuses rather than writing another
format. The two upgrade values remain distinct: `UpgradePolicy` describes how
a surface crosses a release, while `FleetFormat` selects what a writer emits.

Evidence: `crates/lash-core-store/src/store/fleet_format.rs`,
`crates/lash-durable/src/formats.rs`.

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
| Turn checkpoints, VM snapshots, Run records, wait rows, outcome materials and engine state | The actor's format set, the claim filter and drain by release (§1); 1.0 decode-and-resume fixtures. |
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
`crates/lash-core-store/src/store/persisted_state_tests.rs`.

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

Storage laws run against SQLite file, SQLite memory and PostgreSQL. Laws run
the production runtime over a fault-injecting store with labelled commits, a
virtual clock and `SimNodes` (ADR 0132 §14). The drain-by-release law cuts
the release's `drain.release` commit under every fault and requires the
released actor to finish on a node of the newer build with no `Once` body
started twice. The claim-filter law requires that a node lacking a format set
never claims an actor stored in it, that the actor stays visible, and that a
pending cancel ends it engine-free; the fleet-format law requires that a newer
set is not written while an older node is live. The 1.0 decode-and-resume
fixtures commit encoded state in every format of the build's sets, and a
check requires a fixture for every format id. Format-registry checks validate
policy declarations; release fixture capture writes `fixtures/release/<tag>/`.

Evidence: `crates/lash-durable/src/laws/formats.rs`,
`crates/lash-durable-test/tests/drain_by_release.rs`,
`crates/lash-durable-test/tests/format_fixtures.rs`,
`crates/lash-core-execution/src/runtime/actor/process_laws.rs`,
`scripts/capture_release_fixtures.py`,
`scripts/check_format_registry.py`.

### 7. What stays fail-closed

Startup and decoder admission refuse unsupported component and surface ranges,
unregistered predecessor conversions, malformed compatibility records and
integrity failures. Synthetic-next tests prove a skipped compatibility release
refuses. A node never claims an actor whose formats it cannot decode, except
to end it on a pending cancel without decoding it. A writer never writes a
newer format set while a live node lacks it. A stale writer refuses after the
fleet epoch leaves its writable range. These are typed outcomes; missing evidence
is not permission to mutate or decode under another contract.

Evidence: `crates/lash-core-store/src/compat.rs`,
`crates/lash-durable/src/laws/formats.rs`.

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
no node of the older build is live. Operators run schema migration, then
drain by release. Immutable history retains its bytes and identities. The current
upgrade evidence comes from synthetic-next; the version freeze does not
promise migration or rollback between arbitrary development builds.
