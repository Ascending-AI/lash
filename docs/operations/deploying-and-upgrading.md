# Deploying and upgrading lash

This guide is for operators deploying lash 1.0 and planning a roll to the next
release. [ADR 0115](../adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
defines the compatibility contract.

## Upgrading lash before 1.0

Until 1.0, lash keeps its stored format versions and journal logic epoch
frozen: a build may change stored and journal shapes in place without
moving the build generation, so an unchanged generation is not evidence
that two builds are compatible.

Every lash version bump before 1.0 must therefore reset lash's state
instead of rolling: stop the old build and recreate the stores.

From 1.0 on, any replay or format change moves the generation, and the
drain and finalize path in this guide applies.
[ADR 0115](../adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
states the freeze in its release-cut guardrails and defines the post-1.0
contract.

## Choose the deployment shape

**In-process SQLite.** A SQLite deployment is one database file (ADR 0132
§12): the host configures its path, and the session catalog, the process
registry, the trigger store and the durability core all live in it, so one
transaction commits rows of every family or none. One host owns the file.
Stop that host before replacing its binary. The current `lashctl` store
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
(`SqliteStore::open`, `SqliteTriggerStore::open` and the like) never migrates
and refuses an older database with `migration_pending`.

**PostgreSQL workers.** Workers share one PostgreSQL store. Run `lashctl` with
`LASH_POSTGRES_DATABASE_URL` pointing at that store. The host owns traffic
routing, backups and worker lifecycle.

## Read the compatibility report

```sh
lashctl version --json
```

The JSON envelope has `schema_version`, `command`, `result` and `error`.
The CLI reports no generation of its own: a deployment's drain generation `G`
folds in its registered plugins, so only the serving node knows it.
`result.fleet_generations` lists generations with pinned work or a drain mark
in the PostgreSQL store. Each entry has `generation`, `draining`, and
`source: "postgres"`. Use `fleet_generations` to find the generation to drain,
and confirm the serving node's generation before stopping it. The result also
reports the release, fleet epoch `F`'s writable range, each component's
`reads` and `writes` ranges, and the remote wire range.

One release runs one worker feature set. If workers use mixed feature sets,
they occupy separate generation lanes, and each lane must drain separately.
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
for invalid usage, 3 for a refused precondition, 4 for an incompatible store,
and 5 for a drain still pending. Exit 5 can include a useful `result` as well
as an `error`; keep both in the deployment record.

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
| `pre_release` | A build from before 1.0 wrote the store. 1.0 restarted every counter, so nothing reads or migrates it. Recreate the stores. |

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

3. When new traffic is on N+1, mark N's recorded generation for drain:

   ```sh
   lashctl drain "$OLD_GENERATION" --json
   ```

   `drain` marks the generation and hands its work over at once, as a core's
   drain does: every turn parked on a durable wait and every live process on
   N is asked to move to N+1.

   Stalled obligations do not hold the drain. `lashctl stalled list <kind>`
   lists each one of a kind in id order, with its `obligation_id`, typed
   `reason` (`attempts_exhausted`, `refused` or `undecodable`), the `row` it
   lives on, and, when this build cannot name that row, an `undecodable`
   detail such as a kind no build of this release knows. No obligation is
   pinned to a generation: whichever build leads recovery delivers one that
   is re-armed (`lashctl stalled rearm <kind> <id>`), and an `undecodable` one
   stays stalled whichever builds remain. Stopping N neither loses nor
   settles them. Read the list before you stop N, settle each obligation through
   the owning host (re-arm it once its cause is fixed), and keep the ones no
   build can decode, with the listing, in the release record.

   A `scope_close` obligation stalled as `refused` under
   `runtime_store_corrupt` also left a fault on its session (ADR 0109 §9):
   the session admits nothing until the owning host repairs the stored
   data, clears the fault (`LashCore::clear_session_fault`) and re-arms the
   close. `LashCore::session_faults` lists every faulted session, including
   one whose shift admission met the corruption with no obligation to stall.

4. Finalize (next section) while N's drain mark still stands, and close the
   generation drain after it:

   ```sh
   lashctl end-drain "$OLD_GENERATION" --json
   ```

   Record the drain and the finalize result with the release record.

## Finalize the release

Finalize ends the rollback window. It is the last step of N's drain, and it
is irreversible: it moves the fleet epoch `F` to N+1's, every writer whose
writable range excludes the new `F` stops with `WriterFenced` at its next
transaction, and N no longer opens the store. Finalize changes nothing until
N's generation is drained, and refuses `held` while an operator holds it,
carrying the hold's reason and when it was set. This build's `lashctl` has no
`finalize` verb.

An operator who wants to keep the rollback window open, for example to
watch N+1 under production traffic, holds the automatic finalize first:

```sh
lashctl finalize-hold set --reason "watch N+1 for a day" --json
lashctl finalize-hold show --json
lashctl finalize-hold clear --json
```

The hold lives on the fleet-format row, which finalize locks to move `F`, so
a hold set while a rollout finalizes is either seen by that finalize or set
after it committed. While the hold stands, the automatic finalize refuses
`held`.

After it moves `F`, finalize runs every backfill N+1 carries to completion.
A backfill rewrites rows into N+1's shape in batches. Each batch is one
transaction that rewrites a bounded run of rows after the backfill's cursor
and moves the cursor in the same commit, so an interruption loses at most the
batch in flight, and rows already in the new shape are left as they are. The
`lash_migrations` ledger records each backfill's `running` or `applied`
state, its cursor and its rewritten-row count, and the result lists every
backfill finalize completed. If finalize is interrupted after it moved `F`,
run it again before `end-drain`: it reports `already_finalized` and resumes
the backfills from their cursors. The backfills alone can also be resumed, at
any time after finalize:

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

A SQLite store is not finalized by `lashctl`. The host that owns it
finalizes with `SqliteStoreSet::finalize`, which applies the drain checks and
commits `F` and the plugin writer ranges in one
transaction that holds the database exclusively, so a crash leaves the old
epoch or the new one, never a mix. Finalize and migration share the store's
ownership lock. A SQLite store has no operator hold.

## Roll back before finalize

Before finalize, N+1 writes the format selected by N's `F`, so a healthy N
build can reopen the expanded store. Route new traffic back to N. Reverse the
drain against N+1's `G`, then stop it and end its drain:

```sh
lashctl drain "$NEW_GENERATION" --json
lashctl end-drain "$NEW_GENERATION" --json
```

An N+1 continuation that N cannot decode stays on its recorded generation; a
rollback must not delete that state. If either build reports
a typed incompatibility, stop traffic to that build and resolve the recorded
version or route before resuming.

Rollback is safe until finalize, and only until then. Finalize moves `F`
only after N has drained, and that move fences N's writers; recovery then
rolls forward.

## Client and server version skew

Each remote connection begins with a version-range `Hello`. The server picks
the highest version both sides support. N+1 and N select N's wire version
while their ranges overlap. A disjoint range is refused before a request is
decoded or has an effect. Every request carries the selected version; replies,
errors and stream events use that request's version, including when a load
balancer sends it to a server that did not see the original `Hello`. Keep
both builds' endpoints available while their recorded routes remain live.
