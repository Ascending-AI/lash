# Runbook: Rolling Upgrade Across Two Builds (Phase A)

> **Read [../RULES.md](../RULES.md) first.** This is a deterministic companion,
> not an agent-judged browser leg. `just e2e-rolling` runs every command; the
> judge reads its artifact bundle and never drives a process, Docker, or Restate
> itself.

**Purpose.** Prove, before the 1.0 cut, that a 1.0 node can serve as an N-1:
it runs beside a newer build during a roll, and it can be rolled back to
([ADR 0115](../../docs/adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
§6, FIG-3805 phase A). The rollout follows the choreography of
[ADR 0106](../../docs/adr/0106-durable-formats-upgrade-by-migration-or-drain.md)
§6: migrate, half roll, rollback, roll, drain, retire, finalize.

**Execution class.** Deterministic-only. It is listed under `deterministic_only`
in `parity-matrix.toml`. Every model reply comes from the node's scripted
provider, which answers `served by <build> at generation <G>`. The run makes
no provider network call and produces no judged dialect row.

## The two builds

Head is built twice as the `lash-upgrade-node` binary of
`crates/lash-upgrade-harness`:

- **N** is the default build.
- **N+1** is the same tree with the `synthetic-next` Cargo feature. It moves
  the constants whose `#[cfg(feature = "synthetic-next")]` blocks sit beside
  them. Today it moves `JOURNAL_LOGIC_EPOCH`, so N+1's drain generation `G`
  and its generation lanes differ from N's. The PostgreSQL expand step, the
  SQLite component bumps, `F`'s writable range, the remote protocol and
  Restate wire ranges, the effect-group state format, the session node body,
  and the VM continuation format join as their lanes land (ADR 0115 §9, lane
  L8).

`lash-upgrade-node version` prints a build's identity: its label, `G`, and
every range it declares.

## What the run does

`just e2e-rolling` builds both binaries into `<artifacts>/bin/n/` and
`<artifacts>/bin/n+1/`. It starts one pinned `restate-server`, plus a pg16
container unless `LASH_POSTGRES_DATABASE_URL` names a database. Then it runs
`roll_and_rollback_smoke` in `crates/lash-upgrade-harness/tests/rolling/`.
The smoke rolls twice: once over PostgreSQL and once over a fresh SQLite store
directory. Each roll has its own Restate namespace and authority, and all of
its turns go to one session, so each build reads what the other wrote.

| Step | Nodes serving afterwards | Turns | Driver the run requires |
|---|---|---|---|
| migrate | N (URI 1) | host N | N |
| half roll | N (URI 1), N+1 (URI 2) | host N, then host N+1 | either (Restate routes between deployments) |
| rollback | N (URI 3) | host N | N |
| roll | N+1 (URI 4) | host N+1 | N+1 |

Every deployment registers at a fresh URI; no build ever re-registers over
another build's URI (ADR 0115 §3.5). A node that stops is killed the way a
pod dies. A node that has exited by itself before it is stopped fails the
run, because that exit means it refused something.

**Not yet run.** Drain, retire and finalize wait for `lashctl drain` (lane L6)
and for finalize (FIG-3800 B, after 1.0). The eight `phase_a` legs in
`crates/lash-upgrade-harness/tests/phase_a/` are listed there, each ignored
with the lane it waits for. The runbook grows a phase for each one as it
lands.

## Operator commands

Each step of the roll will run as the `lashctl` verb an operator types. `lashctl` is
lane L6 (FIG-3847) and does not exist yet. Until it lands, the steps run
through `lash-upgrade-node`, and the verbs below are the steps they will take
over. The operator guide (FIG-3806) names these verbs; its coverage check
reads this table.

| `lashctl` verb | Step | What it proves there | Runs today as |
|---|---|---|---|
| `lashctl version` | before migrate, once per build | each build prints its label, `G` and every declared range; N and N+1 differ | `lash-upgrade-node version` |
| `lashctl preflight` | before migrate, and before each roll | the store admits the build that is about to serve it | not yet: waits for L6 |
| `lashctl migrate` | migrate (N provisions), then half roll (N+1 expands) | the expand leaves N able to open the store | `lash-upgrade-node migrate` |
| `lashctl drain` | after roll (drain `G_N`); after rollback (drain `G_N+1`, the reverse drain of ADR 0115 §3.5) | the retiring generation takes no new work | not yet: waits for L6 |
| `lashctl drain-status` | between drain and retire | the retiring generation reads drained before its deployment is removed | not yet: waits for L6 |
| `lashctl end-drain` | after retire | the drain closes once its generation is drained and retired | not yet: waits for L6 |

When L6 lands, each row that says "not yet" becomes a step of `just e2e-rolling`,
with the verb's `--json` output kept in the artifact bundle and checked by the
scorecard below.

## Evidence

The artifact directory (default
`target/functional-e2e-artifacts/e2e-rolling/`) holds:

- `rolling-report.json`: one record per turn, giving the case, the step, the
  driver the run required (`null` during the half roll), and the node's
  `TurnReport` (host build, session, status, reply);
- `postgres/` and `sqlite/`: each case's scratch. `ready-<build>-<k>.json`
  is what the case's `k`-th `serve` node registered (build, `G`, URI), with
  N+1 spelled `np1` in the file name, and `ready-<build>-<k>.log` is that
  node's output. `sqlite/stores/` is the SQLite store set that every node of
  that roll opened;
- `restate-server.log`: the server's log.

## Scorecard

The judge answers each item from the bundle and cites the file:

1. **Two builds.** Both `bin/*/lash-upgrade-node version` reports differ in
   `build` and in `generation`. The run's first line prints both `G`.
2. **Ten answered turns.** `rolling-report.json` has ten records, five per
   case, and every `status` is `Answered`.
3. **Routing.** In each record where `expected_driver` is set, the reply names
   that build and its `G`. In the half roll, record which build drove each
   host's turn. Either is correct, but a host N turn driven by N+1 shows that
   the newest deployment took new invocations.
4. **Fresh URIs.** Across the `ready-*.json` of one case, every `uri` is
   distinct, and each `generation` matches its build's.
5. **No refusal.** No node log holds an error, a panic, or a typed store
   refusal (`Incompatible`, `WriterFenced`, `SchemaVersionOutOfRange`,
   `FleetFormatOutsideWritableRange`).

Any failed item is an Abort/RCA under [../RULES.md](../RULES.md).
