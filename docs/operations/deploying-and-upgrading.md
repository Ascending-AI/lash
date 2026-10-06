# Deploying and upgrading lash

This guide is for operators deploying lash 1.0 and planning a roll to the next
release. The [rolling upgrade runbook](../../runbooks/rolling-upgrade/runbook.md)
rehearses the two-build sequence. [ADR 0115](../adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
defines the compatibility contract.

## Upgrading lash before 1.0

Until 1.0, lash keeps its stored format versions and journal logic epoch
frozen: a build may change stored and journal shapes in place without
moving the build generation, so an unchanged generation is not evidence
that two builds are compatible.

Every lash version bump before 1.0 must therefore reset lash's state
instead of rolling: recreate the stores, and retire the old build's Restate
deployments, its generation lanes (`…_g$OLD_GENERATION`) and any
invocations still pinned to them.

From 1.0 on, any replay or format change moves the generation, and the
drain, finalize and object-upgrade path in this guide applies.
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

**PostgreSQL with Restate workers.** Workers share one PostgreSQL store and a
Restate namespace. Run `lashctl` with `LASH_POSTGRES_DATABASE_URL` pointing at
that store. The host owns traffic routing, backups, worker lifecycle and
Restate deployment registration and retirement. Give each build a distinct,
immutable Restate endpoint URI; never replace one generation's deployment at
another generation's URI. Keep the old deployment registered while its pinned
work remains. New invocations use the newest deployment; a recorded route or
parked journal can still require the old one.

## Size Restate's invoker timeouts for long tool work

A tool body runs as a recorded step inside its Run's Restate invocation, and
the invocation cannot suspend while the body runs. Restate's invoker asks an
invocation to suspend after `worker.invoker.inactivity-timeout` without
progress and aborts it `worker.invoker.abort-timeout` later. On the pinned
Restate 1.7 the defaults are 1 minute and 10 minutes. Lash sets neither.

A body that runs longer than the two together is aborted before its result
is recorded. Every retry replays the journal and runs the body again from the
start, and the aborted attempt's body still runs to its end. Lash's handler
attempt bound (`TURN_HANDLER_MAX_ATTEMPTS`, 8) ends the loop: the invocation
pauses with its journal kept, and `send` answers the turn as parked with
`EngineRetryExhausted`. So such a body runs up to 8 times, and the turn never
finishes on its own.

Declare long work `isolated` instead: the call starts a lash process that
the host's registered `ProcessEngine` runs in its own invocation, on the
host's nodes. How that engine waits on long work is the engine's business;
an engine that suspends its invocation on durable timers while the work runs
is never aborted by the invoker's timeouts. Work a host runs outside lash
takes the same path: the host's engine submits it and awaits its completion
durably, so it has the process's deadline, cancellation and recovery. Lash
has no process row it never runs. Lash ships no engine that runs OS
programs. Its cancellation of a process is cooperative, and a host that
needs hard isolation (an OS kill and reap) builds it into its own engine.

Size the server's two timeouts above the longest inline tool body you admit.
The `long-tool-body` suite of `scripts/restate-suites.toml` holds the tool
body rule on a live server with a 1 second inactivity and 2 second abort
timeout.

## Read the compatibility report

```sh
lashctl version --json
```

The JSON envelope has `schema_version`, `command`, `result` and `error`.
The CLI reports no generation of its own: a deployment's drain generation `G`
folds in its registered plugins, so only the serving node knows it.
`result.fleet_generations` lists generations with pinned work or a drain mark
in the PostgreSQL store. Each entry has `generation`, `draining`, and
`source: "postgres"`. `version` does not read the Restate admin API, so this
list cannot show a registered deployment that has no store work and no drain
mark; `finalize` reads it. Use `fleet_generations` to find the generation to drain, and confirm the
serving node's generation before retiring its deployment. The result also
reports the release, fleet epoch `F`'s writable range, each component's
`reads` and `writes` ranges, and the remote and Restate wire ranges.

One release runs one worker feature set. If workers use mixed feature sets,
they occupy separate generation lanes, and each lane must drain separately.
A component is the PostgreSQL schema, the SQLite database, or a Restate
object family. Each store stamp has a version and a
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
| `pre_release` | A build from before 1.0 wrote the store, the Restate object or the call. 1.0 restarted every counter, so nothing reads or migrates it. Recreate the stores and serve this build from a Restate namespace no pre-release build has used. |

A writer that observes a finalized `F` outside its range stops with
`WriterFenced` before making a mutation. A Restate call with disjoint wire
ranges returns `lash.wire_unsupported`; an object whose `_compat` floor is too
high, or that a pre-release build stamped, returns `lash.incompatible`, as
does a call from a pre-release build. Preserve the old deployment and the affected
state while investigating either refusal.

## Roll PostgreSQL workers from N to N+1

1. Back up the shared store and record both builds' `version` reports. Keep
   the Restate namespace unchanged. Set `LASH_POSTGRES_DATABASE_URL` for the
   target store. Use N+1's binary to inspect and apply its expand migration:

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
   the expanded store. Start N+1 workers beside N at a new Restate endpoint
   URI. Register that deployment, move traffic gradually, and watch both
   builds' errors and in-flight work. Do not unregister N during the roll.

3. When new traffic is on N+1, mark N's recorded generation for drain:

   ```sh
   lashctl drain "$OLD_GENERATION" --json
   lashctl drain-status "$OLD_GENERATION" --restate-admin-url "$RESTATE_ADMIN_URL" --json
   ```

   `drain` marks the generation and hands its work over at once, as a core's
   drain does: every turn parked on a durable wait and every live process on
   N is asked to move to N+1 through the Restate deployment that
   `RESTATE_INGRESS_URL`, `RESTATE_AUTHORITY_ID` and `RESTATE_NAMESPACE` name.
   Repeat `drain-status` until its `drained` field is true and exit code is 0.
   Drained means nothing left needs N's deployment: the generation is marked,
   and it holds no live or parked process, no parked or in-flight turn, no
   session is closing, and no unfinished engine invocation is pinned to a
   deployment serving N. `unfinished_invocations` includes real old work,
   terminal reads, session shifts and paused invocations until they return.
   `drain-status` obtains that evidence through Restate's admin API and requires
   `--restate-admin-url`. An unreachable or unreadable API fails the command;
   it never reports drained. Code 5 means retained work remains: inspect the
   counts and settle it through its owning host.

   Continuation adoption transfers the complete logical Run and rebinds short
   source subscriptions to N+1. A source may remain unresolved after N is
   removed; stale subscriptions, waits, reads or shifts must not pin N. L11's
   deployment witness resolves that source after non-forced removal. Process
   segment journal pins and independently owned old work still hold the drain.
   Keep N registered until every real invocation or retained route needs it
   no longer. A missing heartbeat or empty host queue is no retirement proof.

   Stalled obligations do not hold the drain, so a drained result can still
   list them. `stalled_obligations` counts them per kind, and `stalled` lists
   each one, at most 100 per kind in id order, with its `kind`,
   `obligation_id`, typed `reason` (`attempts_exhausted`, `refused` or
   `undecodable`), the `row` it lives on, and, when this build cannot name
   that row, an `undecodable` detail such as a kind no build of this release
   knows. No obligation is pinned to a generation: whichever build leads
   recovery delivers one that is re-armed, and an `undecodable` one stays
   stalled whichever deployments remain. Retiring N neither loses nor settles
   them. Read the list before you retire N, settle each obligation through
   the owning host (re-arm it once its cause is fixed), and keep the ones no
   build can decode, with the listing, in the release record.

   A `scope_close` obligation stalled as `refused` under
   `runtime_store_corrupt` also left a fault on its session (ADR 0109 §9):
   the session admits nothing until the owning host repairs the stored
   data, clears the fault (`LashCore::clear_session_fault`) and re-arms the
   close. `LashCore::session_faults` lists every faulted session, including
   one whose shift admission met the corruption with no obligation to stall.

4. Retire N's Restate deployment only after the drain and the host's pinned
   invocation check both pass: remove every deployment that serves N's
   generation lanes (`…_g$OLD_GENERATION`) from the Restate server. Then
   finalize (next section) while N's drain mark still stands, and close the
   generation drain after it:

   ```sh
   lashctl end-drain "$OLD_GENERATION" --json
   ```

   Record the drain status, the retirement evidence and the finalize result
   with the release record.

## Finalize the release

Finalize ends the rollback window. It is the last step of N's drain, and it
is irreversible: it moves the fleet epoch `F` to N+1's, every writer whose
writable range excludes the new `F` stops with `WriterFenced` at its next
transaction, and N no longer opens the store. Run it with N+1's `lashctl`,
the build whose epoch it moves to, and name N's generation and the Restate
admin API the deployments are registered with:

```sh
lashctl finalize "$OLD_GENERATION" --restate-admin-url "$RESTATE_ADMIN_URL" --json
```

Finalize changes nothing, and refuses typed, until all of these hold:

| Refusal | Exit | Meaning and action |
| --- | --- | --- |
| `generation_not_drained` | 5 | N's generation is not marked draining, or it still holds a live or parked process, a parked or in-flight turn, a closing session, or unfinished invocations pinned to its deployment. The refusal carries the drain status; keep polling `drain-status`. |
| `deployments_retained` | 3 | The Restate server still holds a deployment serving N's generation lanes, in any namespace. The refusal lists each by id and URI. Remove them once their pinned invocations have drained. |
| `held` | 3 | An operator holds the automatic finalize. The refusal carries the hold's reason and when it was set. |

Retirement is read from the Restate server's deployment listing and the
store's own drain records, never from worker heartbeats: a registered
deployment that is asleep still counts. A Restate admin API that cannot be
read fails closed with exit 1.

The host's rollout runs `lashctl finalize` as the drain's automatic last
step. An operator who wants to keep the rollback window open, for example to
watch N+1 under production traffic, holds it first:

```sh
lashctl finalize-hold set --reason "watch N+1 for a day" --json
lashctl finalize-hold show --json
lashctl finalize-hold clear --json
```

The hold lives on the fleet-format row, which finalize locks to move `F`, so
a hold set while a rollout finalizes is either seen by that finalize or set
after it committed. While the hold stands, the automatic finalize refuses
`held`, and an operator can still finalize by hand:

```sh
lashctl finalize "$OLD_GENERATION" --restate-admin-url "$RESTATE_ADMIN_URL" --override-hold --json
```

Finalizing by hand leaves the hold set; clear it separately.

After it moves `F`, finalize runs every backfill N+1 carries to completion.
A backfill rewrites rows into N+1's shape in batches. Each batch is one
transaction that rewrites a bounded run of rows after the backfill's cursor
and moves the cursor in the same commit, so an interruption loses at most the
batch in flight, and rows already in the new shape are left as they are. The
`lash_migrations` ledger records each backfill's `running` or `applied`
state, its cursor and its rewritten-row count, and the result lists every
backfill finalize completed. If finalize is interrupted after it moved `F`,
run it again before `end-drain`: it checks retirement again, reports
`already_finalized` and resumes the backfills from their cursors. The
backfills alone can also be resumed, at any time after finalize:

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

### Upgrade the Restate objects

Each Lash virtual object records its family format in its `_compat` state,
and every family binds an `upgrade` handler. After finalize, `upgrade`
rewrites the object's values in the newest format of its family and raises
its `_compat` in one exclusive invocation. Before finalize it rewrites
nothing, because rollback to N is still promised. List the objects still at
an older format, then sweep them:

```sh
lashctl objects-preflight --restate-admin-url "$RESTATE_ADMIN_URL" --namespace "$LASH_NAMESPACE" --json
lashctl objects-sweep --restate-admin-url "$RESTATE_ADMIN_URL" --restate-ingress-url "$RESTATE_INGRESS_URL" --namespace "$LASH_NAMESPACE" --json
```

Both commands read the engine, not the PostgreSQL store. The preflight lists
each family's objects that are still at an older format and exits 0 when
there are none, or 5 when some remain. The sweep calls `upgrade` on each
listed object and exits 0 once none remain. Before finalize it is refused
`not_finalized` (exit 3). An object whose `_compat` does not admit this build
is refused `incompatible` (exit 4). The object state is the sweep's only
cursor, so a sweep that is interrupted can be run again: the objects it
already upgraded answer `current`, and it upgrades the rest. The LashTurn
outcome is session history and is read in place; the sweep never rewrites it.

A SQLite store is not finalized by `lashctl`. The host that owns it
finalizes with `SqliteStoreSet::finalize`, which applies the same drain and
retirement checks and commits `F` and the plugin writer ranges in one
transaction that holds the database exclusively, so a crash leaves the old
epoch or the new one, never a mix. Finalize and migration share the store's
ownership lock. A SQLite store has no operator hold.

## Roll back before finalize

Before finalize, N+1 writes the format selected by N's `F`, so a healthy N
build can reopen the expanded store. Register N at a **fresh** Restate URI and
route new traffic there. Keep N+1's deployment registered until its own pinned
invocations drain. Reverse the drain against N+1's `G`, check its status, then
retire it and end its drain:

```sh
lashctl drain "$NEW_GENERATION" --json
lashctl drain-status "$NEW_GENERATION" --restate-admin-url "$RESTATE_ADMIN_URL" --json
lashctl end-drain "$NEW_GENERATION" --json
```

An N+1 continuation that N cannot decode stays on its recorded generation; a
rollback must not delete that deployment or its state. If either build reports
a typed incompatibility, stop traffic to that build and resolve the recorded
version or route before resuming.

Rollback is safe until finalize, and only until then. Finalize moves `F`
only after N has drained and its deployments are removed, and that move
fences N's writers; recovery then rolls forward. The Restate object sweep
also runs only after finalize.

## Client and server version skew

Each remote connection begins with a version-range `Hello`. The server picks
the highest version both sides support. N+1 and N select N's wire version
while their ranges overlap. A disjoint range is refused before a request is
decoded or has an effect. Every request carries the selected version; replies,
errors and stream events use that request's version, including when a load
balancer sends it to a server that did not see the original `Hello`.
Restate's shared handlers use the same range-selection rule per call. Keep
both builds' endpoints available while their recorded routes remain live.
