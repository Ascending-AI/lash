# Runbook: Rolling Upgrade Across Two Builds

> **Read [../RULES.md](../RULES.md) first.** This runbook is agent-judged over
> the artifact bundles of two deterministic companions: `just e2e-rolling`
> (Phase A, local processes) and `just e2e-rolling-cluster` (Phase B, the
> multi-node cluster under load). The companions run every command; the judge
> reads their bundles and never drives a process, Docker, kind or Restate
> itself.

**Purpose.** Prove, before the 1.0 cut, that a 1.0 node can serve as an N-1:
it runs beside a newer build during a PostgreSQL roll, upgrades SQLite
stop-then-start after a complete backup, and can be rolled back to
([ADR 0115](../../docs/adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
§6, FIG-3805). The rollout follows the choreography of
[ADR 0106](../../docs/adr/0106-durable-formats-upgrade-by-migration-or-drain.md)
§6: migrate, half roll, rollback, roll, drain, retire, finalize, and then
the backfill and contract that finalize releases (FIG-3800 B, FIG-3817).

The version freeze holds until the cut, so both phases roll head (N) to the
repository's synthetic successor (N+1, the `synthetic-next` feature), not to
a real 1.1. The first real successor reuses this machinery unchanged.

- **Phase A** (`just e2e-rolling`, `just phase-a`) runs the two builds as
  local processes over PostgreSQL, a SQLite store directory and one
  `restate-server`, one session at a time, and pins each typed refusal.
- **Phase B** (`just e2e-rolling-cluster`) runs the same choreography on
  the FIG-4167 load topology: three Restate nodes with replication two,
  PostgreSQL and two workers per generation on kind, while the FIG-4168
  load driver's sessions keep sending. It is where the four laws are judged
  under load. It is short: it proves each step, and it sets no performance
  baseline.

**Execution class.** Deterministic-only. It is listed under `deterministic_only`
in `judged-matrix.toml`. Every Phase A model reply comes from the node's
scripted provider, which answers `served by <build> at generation <G>`;
Phase B's come from the load topology's synthetic provider. Neither makes a
provider network call or produces a judged dialect row.

## The two builds

Buck2 builds the two `lash-upgrade-node` variants and `lashctl` for the
run. The node binaries are:

- **N** is the default build.
- **N+1** is the same tree with the `synthetic-next` Cargo feature. It moves
  the constants whose `#[cfg(feature = "synthetic-next")]` blocks sit beside
  them. It moves `JOURNAL_LOGIC_EPOCH`, so N+1's drain generation `G`
  and its generation lanes differ from N's. It also exercises PostgreSQL
  expansion, SQLite component bumps, `F`'s writable range, remote and Restate
  wire ranges, and durable surface writer pins and lifts (ADR 0115 §6).

`lashctl version --json` reports the store's `fleet_generations` and the
operator build's declared ranges.
The nodes write their build labels and generations to ready files.
The operator and scripted node builds enable different Lash features, so the
ready-file generations drive routing and drain calls. The N and synthetic N+1 `lashctl` variants are Buck2 targets. `lashctl version`
reports the CLI build; the fleet generation comes from each node's ready file.

## What the run does

`just e2e-rolling` builds both node and operator binaries into `<artifacts>/bin/n/` and
`<artifacts>/bin/n+1/`. It starts one pinned `restate-server`, plus a pg16
container unless `LASH_POSTGRES_DATABASE_URL` names a server. Then it runs
`roll_and_rollback_smoke`, `sqlite_migration_overlap_refused`, and
`sqlite_stop_then_start_roll_and_rollback` in
`crates/lash-upgrade-harness/tests/rolling/`, without parallel test execution.
The smoke rolls twice: once over a fresh PostgreSQL database of its own, and
once over a fresh SQLite store directory. Each roll has its own Restate
namespace and authority, and all of its turns go to one session, so each
build reads what the other wrote.

The PostgreSQL leg permits live mixed-version overlap:

| Step | Nodes serving afterwards | Turns | Driver the run requires |
|---|---|---|---|
| migrate | N (URI 1) | host N | N |
| half roll | N (URI 1), N+1 (URI 2) | host N, then host N+1 | either (Restate routes between deployments) |
| rollback | N (URI 3) | host N | N |
| roll | N+1 (URI 4) | host N+1 | N+1 |

On PostgreSQL the half roll also starts the forward drain of N's
generation, and the rollback happens mid-drain: the forward drain ends, N+1's
generation drains in reverse, and N comes back. The roll then drains N's
generation again, and finalize runs before the roll's turn:

| Finalize step (PostgreSQL) | What the run requires |
|---|---|
| `finalize` while N's stopped deployments are still registered | exit 3, refusal `deployments_retained`; `F` unchanged |
| remove every deployment serving N's generation lanes | at least one removed |
| `finalize-hold set`, then `finalize` | exit 3, refusal `held` |
| `migrate --phase contract` before finalize | exit 3, refusal `contract_before_finalize` |
| `finalize-hold clear`, then `finalize` | `F` moves from 1 to 2; every backfill `applied` |
| `end-drain` of N's generation, then `migrate --phase contract` | the contract step runs |
| N probes the store | refused `reader_floor_above` (floor 2) |

SQLite is a single-host embedded store. Each transition waits for the
session's invocations and generation drain to finish, then stops and reaps
the serving node before its replacement opens the same directory:

| Step | Node serving afterwards | Turn and required driver |
|---|---|---|
| migrate | N | host N, served by N at its generation |
| roll | N+1, after N closes | host N+1, served by N+1 at its generation |
| rollback before finalize | N, after N+1 closes | host N, served by N at its generation |
| re-roll | N+1, after N closes | host N+1, served by N+1 at its generation |
| finalize | N+1, after N's deployments retire and F moves from 1 to 2 | host N+1, served by N+1 at its generation |

N must refuse the finalized store with `fleet_outside_writable`. SQLite has
no separate contract phase because migration changes the whole store set
on open. The exclusive migration lock remains required. The separate
`sqlite_migration_overlap_refused` check deliberately opens N+1 while N
holds the store, requires the typed `MigrationOpenElsewhere` refusal naming
the database and directory, then proves the open succeeds after N closes.

Every deployment registers at a fresh URI; no build ever re-registers over
another build's URI (ADR 0115 §3.5). A node that stops is killed the way a
pod dies. A node that has exited by itself before it is stopped fails the
run, because that exit means it refused something. After the last turn of
each roll, the run requires every turn answered and each turn's model call
recorded exactly once in the case's effects log: nothing lost, nothing
duplicated.

The eight `phase_a` legs in `crates/lash-upgrade-harness/tests/phase_a/` run
under `just phase-a`. The legs that finalize (`history_after_finalize`,
`object_sweep_crash_resume`) retire N's deployments and run `lashctl
finalize` the same way; `finalize_races_every_writer` races writers against
the production flip itself.

## PostgreSQL connection budget

Before starting a roll, declare the peak server-wide capacity:

```text
peak connections = processes per generation * pool max * overlapping generations
                   + workers + admin headroom
```

An ordinary N/N+1 roll has two overlapping generations. The roll with rollback
procedure can retain three generations, so declare `--generations 3` for it.
`pool max` is the sum of every persistent PostgreSQL pool in one process.
Open one `PostgresStorage::connect_with` with an explicit
`PostgresStoreConfig.max_connections` per process, then clone that storage
for its store components. Independent pools add their maxima to this total.
`workers` is the sum of connection limits for other clients of every database
on the server. Administrative headroom includes both PostgreSQL reserved-slot
settings and operator probes.

Run the budget preflight before starting replacement processes:

```bash
lashctl preflight --processes-per-generation 2 --pool-max 18 --generations 3 --workers 12 --admin-headroom 10 --json
```

This declaration needs 130 connections. Preflight reads `max_connections`,
`superuser_reserved_connections` and, where present, `reserved_connections`
from the live server. It exits 3 with `connection_budget_exceeded` when the peak
cannot fit, or `reserved_connections_unbudgeted` when headroom omits reserved
slots. A successful report includes the declaration, the observed server
settings and the peak. Unflagged `preflight` remains a schema probe; it does
not authorize a roll. Changing a server setting alone does not replace the
budget check.

The load topology declares `workers.pgConnections` for its store pool and
`workers.witnessConnections` for its separate witness database pool. Those
witnesses are test clients, not a production Lash requirement. The local
limits are 16 + 2 per load worker. Its other-client allocation is 12: provider
2, smoke client 2, load driver 3 including its measurement connection,
fault probe 1, migration/preflight 2, and diagnostic clients 2.
If the witness cap changes, the other-client allocation must cover at least
`3 * workers.witnessConnections + 6`; both sizing and chart rendering check it.
`postgres.adminHeadroom` is 10 and `postgres.maxGenerations` defaults to 2.
`scripts/loadtest_connection_budget.py::peak_connections` computes the same
formula for `scripts/multi-node-load.sh`, which provisions the server at the
declared peak. A rollback campaign sets `maxGenerations` to 3 before sizing.
The chart refuses a declaration above `postgres.maxConnections` and runs a
budget preflight hook against the live server before every Helm upgrade.
Keep both checks: a rendered chart cannot prove the server has restarted with
its configured capacity.

## Phase B: the cluster roll under load

`just e2e-rolling-cluster` (run it through `kiln gate lash <fork> --`, like
`just multi-node-load`) is `scripts/multi-node-load.sh local rolling-upgrade`.
It builds the load topology's worker, provider and driver, the synthetic
N+1 worker, and both builds' `lashctl`, into two images: N's and N+1's, each
carrying its own `lash-e2e-worker` and `lashctl`. It boots a three-node kind
cluster, installs the `deploy/helm/lash-loadtest` chart, provisions Restate
(three metadata members, 24 partitions, replication two), registers the
bootstrap workers and answers one public smoke turn. It skips the FIG-4167
quorum-loss hold, which proves the topology, not the upgrade. Then it starts
the `smoke-v1` load, four sessions running open-loop until the campaign ends,
and `scripts/loadtest_upgrade.py` runs the campaign.

Worker generations are Kubernetes names, and each one serves at its own
immutable URI `http://lash-loadtest-workers-<generation>:18100`:

| Generation | Build | Serves from | Retired at |
|---|---|---|---|
| `initial` | N | install | the roll, after N's drain |
| `next` | N+1 | the half roll | the rollback, after N+1's drain |
| `rollback` | N | the rollback | Restate: finalize; pods: the fence |
| `final` | N+1 | the roll | never |

The campaign's five steps, in order. Each waits for load work on the
generation it moves away from, and recovers only when every operation in
flight at it has answered, a turn, a queued input and a cron emission sent
after it have answered, and every session has settled a turn it sent after
it (answered, or cancelled as its plan asked):

| Step | What the operator does | What must hold |
|---|---|---|
| `half-roll` | Helm starts `next` beside `initial`; its pre-upgrade hook runs N+1's `lashctl migrate`; N+1's `lashctl preflight` and `version`; `next` registers; N+1's `lashctl drain <G_N>` | the two builds report different `G`; turns after it are answered by `next` workers |
| `rollback` | N+1's `lashctl end-drain <G_N>`; Helm starts `rollback` without a migrate hook; N's `lashctl preflight` admits the expanded store; `rollback` registers; N's `lashctl drain <G_N+1>` | `rollback` reports N's `G`; turns after it are answered by `rollback` workers; `lashctl drain-status <G_N+1>` drained with nothing stalled and no unfinished invocation pinned to `next`'s deployment; `next` retires from Helm and Restate; N's `lashctl end-drain <G_N+1>` |
| `roll` | Helm starts `final` (its hook runs N+1's migrate again, a no-op); N+1's `lashctl preflight`; `final` registers; N+1's `lashctl drain <G_N>` | turns after it are answered by `final` workers; `<G_N>` drained, nothing pinned to either N deployment; `initial` retires from Helm |
| `finalize` | N+1's `lashctl finalize <G_N> --restate-admin-url ...`, once `lashctl drain-status <G_N>` reads drained; an exit 5 `generation_not_drained` (a stopped pod's session close still in the store) waits for the drain again | refused `deployments_retained` while N's deployments are registered; after both are removed, `F` moves from 1 to 2 with every backfill `applied`; `lashctl end-drain <G_N>`; `lashctl migrate --phase contract` executes steps; `lashctl objects-preflight`, `lashctl objects-sweep` and `lashctl objects-preflight` again leave no object at N's format |
| `fence` | the still-running `rollback` N worker marks a drain; a fresh `lash-e2e-worker` starts inside its pod; N's `lashctl preflight` | the live write answers HTTP 500 `writer fenced: ...` and `drain-status` shows no mark written; the fresh process exits non-zero with the reader-floor or fleet-epoch refusal; N's preflight exits 3 or 4; then `rollback` retires |

The driver then reconciles the witness ledgers. Beside the 19 load classes
(FIG-4168) it judges the campaign's own classes
(`runbooks/restate-postgres-workers/src/load/upgrade_verify.rs`):
`upgrade-campaign`, one class per step, and `sessions-through-roll`. The
four laws read from them:

| Law | Where the verdict proves it |
|---|---|
| No lost or duplicated effects across the roll | every step's in-flight operations answered; `turns`, `tools` and `child-processes` span the roll: every answered turn's effects committed exactly once with the regenerated result, and no effect committed that no turn planned |
| Stale writers are fenced after finalize | `finalize` (refused while N was registered, then `F` 1 to 2) and `fence` (the live N write fenced and unwritten, a fresh N process and N's operator refused) |
| The rollback leg restores N cleanly before finalize | `rollback`: N's own `G` serves and takes admission, N+1 drained with nothing stalled or pinned and retired, and `roll` and `finalize` only after it recovered |
| Every session keeps working through the roll | `sessions-through-roll`: every session settled a turn sent after every step |

Phase B is PostgreSQL only: mixed-version overlap is PostgreSQL's (FIG-4254),
and SQLite's stop-then-start leg with its migration backup is Phase A's.

## Operator commands

The harness runs these exact command forms with `LASH_POSTGRES_DATABASE_URL`
set to the PostgreSQL test database. Every command uses the Buck2-built
`lashctl` binary, prints its JSON envelope in the E2E log, and must exit zero.
`G_N` and `G_N+1` below are the generations in the nodes' ready files.

| `lashctl` verb | Place in the PostgreSQL roll | SQLite roll | Runs today as |
|---|---|---|---|
| `lashctl version` | once per build; record each CLI build's ranges | nodes identify themselves in ready files | `lashctl version --json` |
| `lashctl migrate` | N before its first start; N+1 before its first start | migrates on open | `lashctl migrate --json` |
| `lashctl preflight` | before N and N+1 start, and before each return deployment | opens and checks stores on node start | `lashctl preflight --json` |
| `lashctl drain` | reverse drain before N+1 retires in rollback | node uses the SQLite generation drain API | `lashctl drain "$NEW_GENERATION" --json` |
| `lashctl drain-status` | require drained after N+1 retires | node uses the SQLite generation drain API | `lashctl drain-status "$NEW_GENERATION" --restate-admin-url "$RESTATE_ADMIN_URL" --json` |
| `lashctl end-drain` | clear reverse drain after N+1 retires | node uses the SQLite generation drain API | `lashctl end-drain "$NEW_GENERATION" --json` |
| `lashctl drain` | forward drain at the half roll, ended by the rollback, and again before N retires in roll | node uses the SQLite generation drain API | `lashctl drain "$OLD_GENERATION" --json` |
| `lashctl drain-status` | require drained after N retires | node uses the SQLite generation drain API | `lashctl drain-status "$OLD_GENERATION" --restate-admin-url "$RESTATE_ADMIN_URL" --json` |
| `lashctl finalize` | refused while N's deployments are registered and while held, then finalizes after they are removed | SQLite finalize is `SqliteStoreSet::finalize`, not a `lashctl` verb | `lashctl finalize "$OLD_GENERATION" --restate-admin-url "$RESTATE_ADMIN_URL" --json` |
| `lashctl finalize-hold` | set before finalize to prove the hold, then cleared | no SQLite hold | `lashctl finalize-hold set --reason <text> --json`, `lashctl finalize-hold clear --json` |
| `lashctl end-drain` | clear forward drain after finalize | node uses the SQLite generation drain API | `lashctl end-drain "$OLD_GENERATION" --json` |
| `lashctl migrate` | contract: refused before finalize, runs after the backfills | no separate contract; whole-set migration on open | `lashctl migrate --phase contract --json` |
| `lashctl objects-sweep` | `object_sweep_crash_resume` leg: refused `not_finalized` before finalize, then resumes the sweep a crash interrupted | not run over SQLite; the sweep reads the engine, not the store | `lashctl objects-sweep --restate-admin-url "$RESTATE_ADMIN_URL" --restate-ingress-url "$RESTATE_INGRESS_URL" --namespace "$LASH_NAMESPACE" --json` |
| `lashctl objects-preflight` | `object_sweep_crash_resume` leg: lists the objects the crash left at format 1, then none | not run over SQLite; the sweep reads the engine, not the store | `lashctl objects-preflight --restate-admin-url "$RESTATE_ADMIN_URL" --namespace "$LASH_NAMESPACE" --json` |

The N+1 operator binary runs N+1's migrate, preflight, version and drain
commands. `OLD_GENERATION` and `NEW_GENERATION` are the node ready-file values.
`lashctl version` describes the CLI build; drain and routing use the node's
ready-file generation.

SQLite's migration-on-open follows ADR 0106 §5. `lashctl` currently accepts a
PostgreSQL database URL, so a SQLite `lashctl` invocation would test that
PostgreSQL database rather than the SQLite case.

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
- `restate-server.log`: the server's log;
- the E2E log: every `lashctl` JSON envelope and turn report.

Phase B's run directory is `target/fig-3790/<gate>-<timestamp>/`. It holds:

- `result.txt`: the topology gate line and `rolling upgrade passed: ...`;
- `upgrade.log`: every step's intent, injected and recovered row, and every
  `lashctl` envelope, in order; `faults.jsonl` holds the same ledger rows;
- `lashctl.jsonl`: one row per `lashctl` call, with the generation and
  build that ran it, its arguments, its exit code and its envelope;
- `load-witness.txt` and `load.log`: the driver's per-class lines and its
  verdict;
- `*-values.yaml`: the Helm overlay of each step (`half-roll`, `rollback`,
  `rollback-retired`, `roll`, `roll-retired`, `fence-retired`);
- `faults-kubectl.log`: every `kubectl` and `helm` call the campaign made;
- each pod's log (`<pod>.log`), `events.txt` and `resources.txt`, captured
  before the cluster is deleted.

## Scorecard

The judge answers each item from the bundle and cites the file:

1. **Two builds.** The ready files differ in `build` and in `generation`.
   The PostgreSQL log shows `lashctl version --json` for the operator build.
2. **Ten answered turns.** `rolling-report.json` has ten records, five per
   case, and every `status` is `Answered`.
3. **Routing.** In each record where `expected_driver` is set, the reply names
   that build and its `G`. In the PostgreSQL half roll, record which build
   drove each host's turn. Either is correct, but a host N turn driven by N+1 shows that
   the newest deployment took new invocations.
4. **Fresh URIs.** Across the `ready-*.json` of one case, every `uri` is
   distinct, and each `generation` matches its build's.
5. **No refusal.** No node log holds an error, a panic, or a typed store
   refusal (`Incompatible`, `WriterFenced`, `CompatRefusal::TooOld`,
   `CompatRefusal::ReaderFloorAbove`, `CompatRefusal::FleetOutsideWritable`).
6. **Finalize.** The E2E log shows, in order: `lashctl finalize` refused
   `deployments_retained`, then refused `held`; `lashctl migrate --phase
   contract` refused `contract_before_finalize`; `lashctl finalize` answering
   `{"outcome":"finalized","from":1,"to":2}` with every backfill `applied`;
   and `lashctl migrate --phase contract` executing its step. SQLite records
   its finalize flip, has no separate contract step, and N refuses its
   finalized fleet epoch.
7. **SQLite backup before migrate.** The E2E log prints the SQLite roll's
   migration backup after N+1's first turn and again after finalize: one
   `manifest.json` in state `migrated`, naming all three databases, each
   moving from a lower to a higher version. No backup existed before N+1
   opened the store.

Phase B, from its run directory:

8. **The cluster.** `result.txt` shows the topology gate line (three Restate
   nodes, three metadata members, replication two) and `rolling upgrade
   passed`. `lashctl.jsonl` shows both builds' `lashctl` (`build` `n` and
   `n+1`), and the half roll's injected row reports two different `G`.
9. **Every step, in order.** `upgrade.log` shows `half-roll`, `rollback`,
   `roll`, `finalize` and `fence`, each `intent`, `injected`, `recovered`,
   each injected after the previous step recovered, and the campaign
   `complete`. Each injected row names in-flight work and each recovered row
   gives `in_flight_at_injection` of at least one.
10. **No lost or duplicated effects.** `load-witness.txt` reports
    `violations=0` for `turns`, `tools`, `child-processes` and every step
    class, and the verdict line reads `verdict=passed`.
11. **The rollback restores N before finalize.** The `rollback` injected row's
    `restored_generation` equals the half roll's `old_generation`, its
    recovered row shows `drained: true` with zero pinned and stalled, and
    `retired: next`; the `lashctl.jsonl` row for N's `preflight` in the
    rollback exited 0; `finalize` comes after it.
12. **Stale writers are fenced.** `lashctl.jsonl` shows `finalize` refused
    `deployments_retained`, then answering `{"outcome":"finalized","from":1,
    "to":2}` with every backfill `applied`, then `migrate --phase contract`
    executing steps and the object sweep leaving nothing at N's format. The
    `fence` rows show the live write answering HTTP 500 `writer fenced`, no
    drain mark written, the fresh N process's refusal text and exit code,
    and N's `preflight` exiting 3 or 4.
13. **Every session keeps working.** `load-witness.txt` shows
    `sessions-through-roll` with five or more witnessed and zero violations.

Any failed item is an Abort/RCA under [../RULES.md](../RULES.md).
