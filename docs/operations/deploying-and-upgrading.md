# Deploying and upgrading lash

This guide is for operators deploying lash 1.0 and planning a roll to the next
release. [ADR 0115](../adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
defines the compatibility contract. [Hosting lash 1.0 on the durable
substrate](durable-hosting.md) covers what a node runs, its configuration,
and the contracts a host implements.

## Upgrading lash before 1.0

Until 1.0, lash keeps its stored format versions frozen: a build may change
stored shapes in place without moving a version, so unchanged versions are
not evidence that two builds are compatible.

Every lash version bump before 1.0 must therefore reset lash's state
instead of rolling: stop the old build and recreate the stores. At the 1.0
cutover, a stable build at or above 1.0.0 refuses every store stamped by a
release below 1.0.0, including `0.0.0-dev` and `1.0.0-rc.1`, with the typed
`pre_release` refusal regardless of its format counters. This refusal is the
one documented signal to recreate the store once at the cutover; every other
refusal stops the deploy. Drain by
release does not make a pre-1.0 roll safe: with the versions frozen, both
builds spell the same format sets, so the next build would claim state whose
shape it may not decode.

From 1.0 on, any format change moves its version, and the upgrade path in
this guide applies: the old build drains by release and the next build
claims what it released.
[ADR 0115](../adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
states the freeze in its release-cut guardrails and defines the post-1.0
contract.

## Choose the deployment shape

**In-process SQLite.** A SQLite deployment is one database file (ADR 0132
§12): the host configures its path, and the session catalog, the process
registry and the durability core all live in it, so one
transaction commits rows of every family or none. One host owns the file.
Stop that host before replacing its binary. When the release changes a
durable format, drain the old build first (`LashCore::drain`, below), shut it
down, then start the new one: its node claims what the old one released. The current `lashctl` store
commands require `LASH_POSTGRES_DATABASE_URL` and do not migrate SQLite:
SQLite migrates on open, after a backup. A configured path that is a
directory holding the retired layout of three database files
(`durable-core.db`, `process-registry.db`, `triggers.db`) is refused
`retired_sqlite_layout`, unchanged; formats reset at 1.0 and no release
migrates that layout.

When `SqliteStoreSet::open` finds the database older than the build writes,
it first needs the store to itself: it takes the store's migrator lock
(`<database>-migration.lock`), checkpoints and closes the database, and if
another connection still holds it, waits up to the busy timeout and then
refuses, changing nothing. It then copies the database file, byte for byte
and synced, into a new `sqlite-backup-NNNNNN` directory with a
`manifest.json`, and only then migrates, in one transaction that holds the
database exclusively. `SqliteStoreSetOptions::migration_backup` sets where the
backups go (`BesideStore`, which is `migration-backups/` beside the database
file, or a directory of the host's) and how many finished backups of the
store to keep (`retain`, default 2). A backup that an unfinished migration
still needs is never removed.

An interrupted migration is finished by the next open: one whose transaction
committed is recorded finished, and one that did not commit starts again from
a fresh backup. A migration that fails before its commit changes nothing; its
backup goes with the error. A component opened on its own
(`SqliteStore::open` and the like) never migrates
and refuses an older database with `migration_pending`.

**PostgreSQL workers.** PostgreSQL 17 and 18 are supported for lash 1.0; 18
is primary, and every release also passes its PostgreSQL suites on 17. Workers share one PostgreSQL store. Run
`lashctl` with `LASH_POSTGRES_DATABASE_URL` pointing at that store. The host owns traffic
routing, backups and worker lifecycle.

## Connect PostgreSQL nodes

Several lash nodes can serve one PostgreSQL store (ADR 0132 §3). One
validated `PostgresHostConfig` sizes, guards and names every connection a
process opens: the work, scheduler and critical pools, a renewal connection
and a listener session per served node, the detached schema and sweep
sessions and, when configured, the live replay store's pool and listener.
[`postgres.md`](postgres.md) is the reference: every field, the sizing
formula (24 server connections for one default node, 32 with live replay),
the declared deployment budget a connect checks against the server, and the
pooler rules. The listener needs a session of its own: connect directly or
through a pooler in session mode, or give a transaction-mode pooler's
deployment a separate session endpoint. A transaction-mode pooler silently
drops both `LISTEN` and the session lock.

When a node's process dies, its listener session ends and the other nodes
reap it within a claim poll (250 ms by default) instead of waiting for its
15-second lease. A node cut off by a network partition keeps its session until
the server notices the dead connection, so set `tcp_keepalives_idle`,
`tcp_keepalives_interval` and `tcp_keepalives_count` on the server to bound
that; otherwise the lease reaps it. Either way the epoch fence refuses every
commit the cut-off node attempts after the reap.

Wake hints are only hints: a node polls for claimable work and for its hot
actors' mail at most every claim poll, so a lost notification costs latency,
never work. `Notifier::PollOnly` turns the listener off, and with it the fast
crash detection.

The topology is the host's (ADR 0132 §13): `Once` survives a database
failover only when acknowledged commits survive promotion. The host guide
explains what that means for [synchronous and asynchronous
replicas](durable-hosting.md#durability-across-a-failover).

## Read the compatibility report

```sh
lashctl version --json
```

The JSON envelope has `schema_version`, `command`, `result` and `error`.
`version` reads no store. It reports the release, fleet epoch `F`'s writable
range and each component's `reads` and `writes` ranges. Hosts own any client
transport compatibility ([ADR 0136](../adr/0136-hosts-own-their-wire-contracts.md)).

One release runs one worker feature set.
A component is the PostgreSQL schema or the SQLite database. Each store stamp has a version and a
`min_reader` floor. An older build admits a safely expanded component while
the floor still allows it; an unsafe schema addition is refused. `F` selects
the format all live writers emit. Before finalize, N+1 writes only shapes and
semantics that N can read.

```sh
lashctl preflight --json
```

This command currently checks the PostgreSQL store and returns its database,
release and fleet-format verdict. Run it with the build that is about to serve
traffic. The JSON exit codes are 0 for done, 1 for an unexpected failure, 2
for invalid usage, 3 for a refused precondition and 4 for an incompatible
store. A refusal can carry a useful `result` as well as an `error`; keep both
in the deployment record.

An incompatible store may report a typed refusal:

| Refusal | Meaning and action |
| --- | --- |
| `unstamped` | A populated component lacks its stamp. Stop; use the matching migration or restore a consistent backup. |
| `malformed_stamp` | The stamp cannot be read or its floor is invalid. Stop and repair or restore the stamp; do not guess its version. |
| `too_old` | The component predates this build's read range. Upgrade through the intervening release. |
| `migration_pending` | A SQLite database is older than this build writes and was opened on its own. Open the whole store with `SqliteStoreSet::open`, which backs it up and migrates it. |
| `reader_floor_above` | A newer release contracted beyond this build. Roll forward; this build cannot read the store. |
| `shape_refused` | An addition would change how this build writes an expected table. Stop the roll and correct the migration. |
| `fleet_outside_writable` | The recorded `F` is outside this build's writable range. Below it means a skipped release; above it means the fleet has advanced. Use the intervening or newer build as appropriate. |
| `fleet_unrecorded` | The PostgreSQL store records no `F`. `lashctl migrate` seeds it and a worker open never records one. Run `lashctl migrate`, then open again. |
| `retired_sqlite_layout` | The configured SQLite path is a directory in the retired layout of three database files. Nothing migrates it: configure the path of a database file and recreate the store there. |
| `unknown_vocabulary` | A stored kind or state is unknown to this build. Keep the record and route to a build that understands it. |
| `pre_release` | A stable build at or above 1.0.0 refuses every store stamped below 1.0.0, whatever its counters. This is the one signal to recreate the store once at the 1.0 cutover; every other refusal stops the deploy. The refusal names `writing_release` and leaves the store unchanged. |

A writer that observes a finalized `F` outside its range stops with
`WriterFenced` before making a mutation. Preserve the old build and the
affected state while investigating the refusal.

## Roll PostgreSQL workers from N to N+1

1. Back up the shared store and record both builds' `version` reports. Set
   `LASH_POSTGRES_DATABASE_URL` for the target store. Use N+1's binary to inspect and apply its expand migration:

   ```sh
   lashctl migrate --phase expand --dry-run --json
   lashctl migrate --phase expand --json
   ```

   Expand may add only shapes N can tolerate and must retain N's reader floor.
   It does not move `F`. Check the result and stop on any refusal.

   Every `migrate` run also seeds `F` when the store records none, at the
   floor of the migrating build's writable range, and never changes a recorded
   `F`. A fresh store therefore starts at the older release's epoch even when
   N+1 migrates or opens it first. Workers never record `F`, so a store that
   no `migrate` seeded refuses them with `fleet_unrecorded`. Each SQLite
   database seeds `F` the same way when its open-time migration provisions it.

2. Run the new build's preflight again, and verify the old build still admits
   the expanded store. Start N+1 workers beside N, move traffic gradually, and
   watch both builds' errors and in-flight work. Do not stop N during the
   roll.

3. Drain N by release once N+1 serves. On each N node, call
   `LashCore::drain()`
   ([durable hosting](durable-hosting.md#drain-by-release)): the node claims
   nothing more, stops each session it owns at its next committed phase and
   releases it `ready`, then stops. It answers a `NodeDrainReport` naming
   the sessions and processes it released; record it. N+1's nodes claim
   them and resume each from its committed rows. Then call `shutdown` on N.
   A node that serves processes through `node::serve` drains when the host
   starts its `NodeServe::drain`, and `serve` returns `Stopped::Drained`.
   A drain that answers `NodeDrainError::Stopped` lost its node first; what
   it held is claimed as after a crash, and nothing acknowledged is lost.

   Before you stop N, also read the stalled artifact cleanups (the
   one obligation kind left, `artifact_cleanup`; every other kind is a
   mailbox write in its producer's transaction, ADR 0132 §12).
   `lashctl stalled list <kind>` lists each one of a kind in id order, with
   its `obligation_id`, typed `reason` (`attempts_exhausted`, `refused` or
   `undecodable`), the `row` it lives on, and, when this build cannot name
   that row, an `undecodable` detail such as a kind no build of this release
   knows. No obligation is pinned to a build: whichever build leads recovery
   delivers one that is re-armed (`lashctl stalled rearm <kind> <id>`), and an
   `undecodable` one stays stalled whichever builds remain. Stopping N neither
   loses nor settles them. Settle each obligation through the owning host
   (re-arm it once its cause is fixed), and keep the ones no build can decode,
   with the listing, in the release record.

## Finalize the release

No build moves the fleet epoch `F` now: the release cut owns moving it
(ADR 0115). The generation drain's finalize and `lashctl finalize-hold` are
gone; actor state crosses the release by the drain in step 3. Backfill and
contract below still wait for `F` to move.

A backfill rewrites rows into N+1's shape in batches. Each batch is one
transaction that rewrites a bounded run of rows after the backfill's cursor
and moves the cursor in the same commit, so an interruption loses at most the
batch in flight, and rows already in the new shape are left as they are. The
`lash_migrations` ledger records each backfill's `running` or `applied`
state, its cursor and its rewritten-row count. A backfill is resumed from
its cursor, at any time after `F` moved:

```sh
lashctl migrate --phase backfill --json
```

A backfill that runs before finalize is refused
`backfill_before_finalize`: nothing rewrites a row N reads while rollback to
N is promised. A constraint the release tightens is added `NOT VALID` by the
backfill, so new rows obey it at once, and validated by contract.

Contract drops or tightens what only N needed and raises the schema's reader
floor, so N refuses the store with `reader_floor_above` from then on. It
waits for finalize and for the ledger to show every backfill it names
`applied`:

```sh
lashctl migrate --phase contract --dry-run --json
lashctl migrate --phase contract --json
```

Before finalize it is refused `contract_before_finalize`; before its
backfills are done, `contract_before_backfills`, naming each pending one.
Both are exit 3, and the dry run refuses the same way the run does.


## Roll back before finalize

Before finalize, N+1 writes the format selected by N's `F`, so a healthy N
build can reopen the expanded store. Route new traffic to N. If either build reports
a typed incompatibility, stop traffic to that build and resolve the recorded
version or route before resuming.

Rollback is safe until `F` moves, and only until then: the move fences N's
writers, and recovery then rolls forward.

## Host transport version skew

Lash ships no client/server wire protocol or version negotiation. A host
owns its transport's DTOs, version skew and compatibility policy
([ADR 0136](../adr/0136-hosts-own-their-wire-contracts.md)). Lash's Rust types
carry no wire-stability promise.

Across N and N+1, lash owns compatibility of its stored formats and recorded
routes. Keep the recorded config and model routes available to the builds
resuming that work, and use the store read/write ranges, fleet epoch `F` and
actor format sets to decide which build can serve it. A typed incompatibility
stops traffic to that build until its recorded version or route is resolved.
Host endpoint availability and client transport compatibility follow the
host's own contract.
