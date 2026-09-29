# Deploying and upgrading lash

This guide is for operators deploying lash 1.0 and planning a roll to the next
release. The [rolling upgrade runbook](../../runbooks/rolling-upgrade/runbook.md)
rehearses the two-build sequence. [ADR 0115](../adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
defines the compatibility contract.

## Choose the deployment shape

**In-process SQLite.** One host owns the store's durable-core,
process-registry and trigger databases. Back up all three files together and
stop that host before replacing its binary. Do not treat three separate files
as one atomic transaction. A partially advanced set must be completed forward
by a build with the needed migrations; an older build refuses it. The current
`lashctl` store commands require `LASH_POSTGRES_DATABASE_URL` and do not migrate
SQLite. SQLite migrates on open after a backup.

**PostgreSQL with Restate workers.** Workers share one PostgreSQL store and a
Restate namespace. Run `lashctl` with `LASH_POSTGRES_DATABASE_URL` pointing at
that store. The host owns traffic routing, backups, worker lifecycle and
Restate deployment registration and retirement. Give each build a distinct,
immutable Restate endpoint URI; never replace one generation's deployment at
another generation's URI. Keep the old deployment registered while its pinned
work remains. New invocations use the newest deployment; a recorded route or
parked journal can still require the old one.

## Read the compatibility report

```sh
lashctl version --json
```

The JSON envelope has `schema_version`, `command`, `result` and `error`.
`result` reports the release, drain generation `G`, fleet epoch `F`'s writable
range, each component's `reads` and `writes` ranges, and the remote and Restate
wire ranges. This `G` describes the CLI build; read each serving node's
generation from its deployment before a drain. A component is the PostgreSQL
schema, one of the three SQLite
databases, or a Restate object family. Each store stamp has a version and a
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
| `reader_floor_above` | A newer release contracted beyond this build. Roll forward; this build cannot read the store. |
| `shape_refused` | An addition would change how this build writes an expected table. Stop the roll and correct the migration. |
| `fleet_outside_writable` | The recorded `F` is outside this build's writable range. Below it means a skipped release; above it means the fleet has advanced. Use the intervening or newer build as appropriate. |
| `partially_advanced` | The three SQLite databases disagree after a partial migration. Reopen with a build able to complete the set forward. |
| `unknown_vocabulary` | A stored kind or state is unknown to this build. Keep the record and route to a build that understands it. |

A writer that observes a finalized `F` outside its range stops with
`WriterFenced` before making a mutation. A Restate call with disjoint wire
ranges returns `lash.wire_unsupported`; an object whose `_compat` floor is too
high returns `lash.incompatible`. Preserve the old deployment and the affected
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

2. Run the new build's preflight again, and verify the old build still admits
   the expanded store. Start N+1 workers beside N at a new Restate endpoint
   URI. Register that deployment, move traffic gradually, and watch both
   builds' errors and in-flight work. Do not unregister N during the roll.

3. When new traffic is on N+1, mark N's recorded generation for drain:

   ```sh
   lashctl drain "$OLD_GENERATION" --json
   lashctl drain-status "$OLD_GENERATION" --json
   ```

   Repeat `drain-status` until its `drained` field is true and exit code is 0.
   A code 5 means work remains; inspect the reported live and parked processes,
   turns, closing sessions and stalled obligations. Keep N's deployment
   registered while any invocation or recorded route still needs it. Settle
   stuck work through the owning host; do not treat a missing heartbeat or an
   empty host queue as retirement evidence.

4. Retire N's Restate deployment only after the drain and the host's pinned
   invocation check both pass. Close the generation drain after retirement:

   ```sh
   lashctl end-drain "$OLD_GENERATION" --json
   ```

   Record the drain status and retirement evidence with the release record.

## Roll back before finalize

Before finalize, N+1 writes the format selected by N's `F`, so a healthy N
build can reopen the expanded store. Register N at a **fresh** Restate URI and
route new traffic there. Keep N+1's deployment registered until its own pinned
invocations drain. Reverse the drain against N+1's `G`, check its status, then
retire it and end its drain:

```sh
lashctl drain "$NEW_GENERATION" --json
lashctl drain-status "$NEW_GENERATION" --json
lashctl end-drain "$NEW_GENERATION" --json
```

An N+1 continuation that N cannot decode stays on its recorded generation; a
rollback must not delete that deployment or its state. If either build reports
a typed incompatibility, stop traffic to that build and resolve the recorded
version or route before resuming.

**After 1.0:** `lashctl finalize`, its retired-deployment check and hold flag
arrive before the first real N-to-N+1 finalize. Finalize moves `F` only after
N has drained and its deployment is removed. That move fences N's writers and
ends rollback to N; recovery then rolls forward. Object upgrade handlers and
their sweep, backfill and contract also arrive after 1.0. They run after the
irreversible boundary, with contract waiting for the backfill ledger. The
1.0 binary does not serve these commands or steps.

## Client and server version skew

Each remote connection begins with a version-range `Hello`. The server picks
the highest version both sides support. N+1 and N select N's wire version
while their ranges overlap. A disjoint range is refused before a request is
decoded or has an effect. Every request carries the selected version; replies,
errors and stream events use that request's version, including when a load
balancer sends it to a server that did not see the original `Hello`.
Restate's shared handlers use the same range-selection rule per call. Keep
both builds' endpoints available while their recorded routes remain live.
