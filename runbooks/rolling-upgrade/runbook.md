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

Bazel builds the two `lash-upgrade-node` variants and `lashctl` for the
run. The node binaries are:

- **N** is the default build.
- **N+1** is the same tree with the `synthetic-next` Cargo feature. It moves
  the constants whose `#[cfg(feature = "synthetic-next")]` blocks sit beside
  them. Today it moves `JOURNAL_LOGIC_EPOCH`, so N+1's drain generation `G`
  and its generation lanes differ from N's. The PostgreSQL expand step, the
  SQLite component bumps, `F`'s writable range, the remote protocol and
  Restate wire ranges, the effect-group state format, the session node body,
  and the VM continuation format join as their lanes land (ADR 0115 §9, lane
  L8).

`lashctl version --json` reports the operator build's
`cli_build_generation`, the store's `fleet_generations`, and declared ranges.
The nodes write their build labels and generations to ready files.
The operator and scripted node builds enable different Lash features, so the
ready-file generations drive routing and drain calls. The N and synthetic N+1 `lashctl` variants are Bazel targets. `lashctl version`
reports the CLI build; the fleet generation comes from each node's ready file.

## What the run does

`just e2e-rolling` builds both node and operator binaries into `<artifacts>/bin/n/` and
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

**Not yet run.** Finalize waits for FIG-3800 B. The eight `phase_a` legs in
`crates/lash-upgrade-harness/tests/phase_a/` are listed there, each ignored
with the lane it waits for. The runbook grows a phase for each one as it
lands.

## Operator commands

The harness runs these exact command forms with `LASH_POSTGRES_DATABASE_URL`
set to the PostgreSQL test database. Every command uses the Bazel-built
`lashctl` binary, prints its JSON envelope in the E2E log, and must exit zero.
`G_N` and `G_N+1` below are the generations in the nodes' ready files.

| `lashctl` verb | Place in the PostgreSQL roll | SQLite roll | Runs today as |
|---|---|---|---|
| `lashctl version` | once per build; record each CLI build's ranges | nodes identify themselves in ready files | `lashctl version --json` |
| `lashctl migrate` | N before its first start; N+1 before its first start | migrates on open | `lashctl migrate --json` |
| `lashctl preflight` | before N and N+1 start, and before each return deployment | opens and checks stores on node start | `lashctl preflight --json` |
| `lashctl drain` | reverse drain before N+1 retires in rollback | no PostgreSQL generation drain | `lashctl drain "$NEW_GENERATION" --json` |
| `lashctl drain-status` | require drained after N+1 retires | no PostgreSQL generation drain | `lashctl drain-status "$NEW_GENERATION" --json` |
| `lashctl end-drain` | clear reverse drain after N+1 retires | no PostgreSQL generation drain | `lashctl end-drain "$NEW_GENERATION" --json` |
| `lashctl drain` | forward drain before N retires in roll | no PostgreSQL generation drain | `lashctl drain "$OLD_GENERATION" --json` |
| `lashctl drain-status` | require drained after N retires | no PostgreSQL generation drain | `lashctl drain-status "$OLD_GENERATION" --json` |
| `lashctl end-drain` | clear forward drain after N retires | no PostgreSQL generation drain | `lashctl end-drain "$OLD_GENERATION" --json` |

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

## Scorecard

The judge answers each item from the bundle and cites the file:

1. **Two builds.** The ready files differ in `build` and in `generation`.
   The PostgreSQL log shows `lashctl version --json` for the operator build.
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
