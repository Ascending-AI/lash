# Kernel migration sweep as an operator procedure

Read [the shared rules](../RULES.md). This is `deterministic_only`, tier
`deterministic`, model `scripted-provider`, cost $0; it emits no judged browser
row. The risk is an upgrade stranding sessions or a closing build refusing to
start without an operator remedy. [ADR 0106](../../docs/adr/0106-durable-formats-upgrade-by-migration-or-drain.md),
[ADR 0115](../../docs/adr/0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
and [the kernel design §6](../../docs/kernel/design.md) bind the decisions.

## Phase 1 — Read the two-build evidence

Source `env.sh`. The ordinary `kernel_two_build_claim__test` label excludes
`synthetic-next` and executes zero laws from this file. Use the existing
SQLite/TypeScript feature witness below, whose generated feature set includes
`synthetic-next`, `testing`, `rlm` and `sqlite`. Run each full selector once:

```sh
kiln test //crates/lash:kernel_two_build_claim__test__fv_0c6870c9 --test_arg=--exact --test_arg=an_idle_process_and_an_idle_session_are_swept_by_build_n_plus_1_and_read_by_the_closing_build
kiln test //crates/lash:kernel_two_build_claim__test__fv_0c6870c9 --test_arg=--exact --test_arg=the_closing_build_refuses_to_start_while_a_session_is_unmigrated_and_starts_after_the_sweep
kiln test //crates/lash:kernel_two_build_claim__test__fv_0c6870c9 --test_arg=--exact --test_arg=the_listing_names_a_refused_session_cell_with_its_reason
```

Save each log, report and `kiln: test passed executed=1 passed=1 failed=0`
receipt. Zero executed is Abort. Inspect the observed assertions against
`crates/lash/tests/kernel_two_build_claim.rs`:

- Build N leaves an idle process and session; N+1's sweep wakes both, they
  migrate under its node, and the closing build reads their retained state.
- The closing build refuses with typed `Unmigrated`, naming
  `lashctl kernel-migration run`, while the old session stands. After N+1's
  sweep, startup succeeds and `double(21)` still returns 42.
- The listing names the refused session and its cell with the typed reason
  for the parked library body the synthetic migration cannot carry. It never
  silently drops that cell.

The previous-release/window-closing half is supplied by these synthetic
build laws. A single current-build SQLite rehearsal cannot independently
certify a released two-binary upgrade or rollback window. Do not edit a
store's format stamps to simulate one.

## Phase 2 — Prepare an owned SQLite operator rehearsal

Keep evidence under `.kiln/<ticket>/run-root/kernel-migration-sweep/sqlite/`.
Boot an owned workbench with fresh data/run directories and the explicit
`AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=migration-sweep` selector, through the
launcher and judged build geometry. Startup must name that dev scenario,
TypeScript and SQLite; readiness is `/healthz`.

```sh
. ./env.sh
export AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=migration-sweep
export AGENT_WORKBENCH_DATA_DIR="$PWD/target/migration-sweep-data"
export AGENT_WORKBENCH_RUN_DIR="$PWD/target/migration-sweep-run"
(ulimit -v 8388608; /usr/bin/time -v bash scripts/agent-workbench-dev.sh up --port 7537)
```

Send “Seed the migration operator rehearsal.” The fixture defines `double`,
starts a process sleeping for 600000 ms, and replies with marker
`migration-sweep seeded`, value 42 and the process handle. Capture
`00-ready.png`, `01-seeded-state.json`, `01-seeded-work.json`, `01-seeded.png`
and a read-only SQL extraction of the idle session and process actor. The
process must have `processes.status=waiting` and `actors.state=waiting` with
a future `next_due_ms`; its sleep is an actor wake, not a row in the session
`waits` table.
Require the process is non-terminal and the session has no active turn. A
missing actor is Abort, even if `lashctl` later returns an empty list.

`deployment-status --accepting-new-work false` is a status query with the
operator's admission-policy assertion. It does **not** configure the host's
admission or drain a node. Stop accepting inputs in the host before a real
upgrade; this isolated rehearsal sends only its named inputs. Retain the
status response and the workbench's actual store path.

## Phase 3 — Drive lashctl

All four calls must address that same owned `lash-sessions.db`. Build/run
through Kiln. If using a prebuilt CLI, resolve it from its Kiln build report
and run inside `kiln gate lash <fork> -- ...`. Capture stdout as the named
JSON evidence files, stderr separately, and the exit code.

```sh
kiln run //crates/lashctl:lashctl -- --json deployment-status --accepting-new-work false --sqlite-path "$AGENT_WORKBENCH_DATA_DIR/lash-sessions.db"
kiln run //crates/lashctl:lashctl -- --json kernel-migration list --sqlite-path "$AGENT_WORKBENCH_DATA_DIR/lash-sessions.db"
kiln run //crates/lashctl:lashctl -- --json kernel-migration run --sqlite-path "$AGENT_WORKBENCH_DATA_DIR/lash-sessions.db"
kiln run //crates/lashctl:lashctl -- --json kernel-migration list --sqlite-path "$AGENT_WORKBENCH_DATA_DIR/lash-sessions.db"
kiln run //crates/lashctl:lashctl -- --json kernel-migration run --sqlite-path "$AGENT_WORKBENCH_DATA_DIR/lash-sessions.db"
```

Save `02-deployment-status.json`, `03-list-before.json`, `04-run.json`,
`05-list-after.json` and `06-run-again.json`. Each CLI result is the
`{schema_version, command, result, error}` envelope. Require successful exits
and no error before interpreting `result`.

`run` returns `result.woken.{processes,sessions}` plus `result.survey`: it wakes migration actors,
it does not report synchronous completed migrations. In a real upgrade, keep
an N+1 serving node alive, wait until the listing contains only typed refusals
or is empty, then run again. An already migrated store must report `woken={processes:0,sessions:0}`.
An actor refused for a known typed reason stays listed until resolved; never
re-arm it blindly, delete it, or count it as migrated. `stalled list` and
`stalled rearm` address delivery obligations, not kernel migration refusals.

On a current-build store, both surveys can already be empty and both sweeps
can wake zero: this proves CLI addressing/idempotency, not migration work.
Report that limit; the Phase 1 laws must supply the actual carry and refusal.

## Phase 4 — Restart and read the bindings

Use the non-destructive launcher restart on the same directories. Send
“Read the migration bindings.” The provider recognizes the committed seed
reply and scripts only a read of `double(21)` and `migrationProcess`, ending
through `chat.reply(JSON.stringify(...))` with marker `migration-sweep resumed`. Require value 42,
the same process id, the same session id and the parked process still present.
Save `07-resumed-{state,work}.json`, `07-resumed.png` and the trace.
Stop with `bash scripts/agent-workbench-dev.sh down --port 7537`; save
`08-teardown.log` and peak RSS. No paid provider or judge call is needed.

## Scorecard

Fill from the actual receipts and artifacts; a missing observation is Abort.

| Gate | Expected | Observed | Verdict/evidence |
| --- | --- | --- | --- |
| Idle carry law | executed=1, passed=1 | | |
| Closing refusal then startup law | executed=1, passed=1; typed Unmigrated | | |
| Refused-cell listing law | executed=1, passed=1; cell and reason retained | | |
| Owned initial state | idle session, one parked process | | |
| CLI envelopes | same SQLite path, exit 0, error absent | | |
| Survey after sweep | empty or only named typed refusals | | |
| Second sweep | both woken counts zero after carry settles | | |
| Restart read | same ids, double(21)=42 | | |
| Teardown | owned stack stopped | | |

Record source SHA, feature-witness label, judged build report, artifact root,
cost $0 and the single-build/two-build evidence distinction. A red companion
is a product finding; never weaken it to pass this scorecard.

[Recorded FIG-5987 scorecard](recorded-run.md).
