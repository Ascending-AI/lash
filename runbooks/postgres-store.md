# PostgreSQL axis for judged workbench rows

The G4 store axis applies the **same scorecard** to session-resume,
process-lifecycle and crash-resume. Their tiers remain deterministic,
economy pro and deterministic respectively. A pending dialect stays pending
on both stores. Use a fresh data directory, session, ports and evidence
bundle per scenario/dialect/store; attachments remain in SQLite at
`<data-dir>/attachments/attachments.db` even when session/process storage is PG.

Run service commands inside `kiln gate lash <fork> -- …`. For the managed
launcher set `AGENT_WORKBENCH_POSTGRES=1` and give its container a unique
`KILN_GATE_ID`-derived name. Keep session-resume's
`AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=replay-route-change` and
`OPENROUTER_MODEL=dev/replay-route-a`; process-lifecycle uses its matrix model,
`deepseek/deepseek-v4-pro`, with the real provider. Keep the managed database
through the restart. Record the startup event as `00-startup.json` and require
`store_backend == "postgres"`; requesting PG in the environment alone earns
no credit.

An externally owned private service is also supported: wrap the rehearsal in
`scripts/ci/with-service.sh pg -- …`, set `AGENT_WORKBENCH_DATABASE_URL` to
its `LASH_POSTGRES_DATABASE_URL`, and leave managed PG off. The gate owns the
service until the rehearsal returns. This permits the post-host-teardown
preflight before the database is removed. Never use a shared/live database.

## Companion laws

These existing laws run unchanged against the explicitly selected store;
without the selector they retain SQLite memory for developer proof. In the
private service gate export `LASH_RUNBOOK_POSTGRES_URL` from
`LASH_POSTGRES_DATABASE_URL`, then run each selector once:

```sh
kiln test //examples/agent-workbench:agent-workbench__unit_test \
  --local-test-execution --no-test-cache \
  --test_env=LASH_RUNBOOK_POSTGRES_URL \
  --test_arg=--exact \
  --test_arg=tests::session_resume_tests::committed_transcript_and_provider_history_survive_web_process_reconstruction
kiln test //examples/agent-workbench:agent-workbench__unit_test \
  --local-test-execution --no-test-cache \
  --test_env=LASH_RUNBOOK_POSTGRES_URL \
  --test_arg=--exact \
  --test_arg=process_work_tests::work_api_keeps_orphaned_process_visible_and_routes_cancel_globally
```

Require `executed=1 passed=1 failed=0` for each; save the receipt and log.
The process companion certifies runtime-wide visibility and cancellation
routing for a detached process, not the runbook's complete browser journey.
Crash-resume uses these existing hermetic PostgreSQL targets, each by its
own exact selector once (the action owns its PostgreSQL 18 server):

```sh
kiln test //crates/lash-durable-test:vertical_crash_proof__test \
  --test_arg=--exact \
  --test_arg=a_once_operation_in_a_cell_killed_after_its_work_resumes_without_replay_on_postgres
kiln test //crates/lash-durable-test:cell_restore_laws__test \
  --test_arg=--exact \
  --test_arg=a_sleeping_cell_suspends_on_idle_eviction_and_wakes_once_at_its_deadline_on_postgres
kiln test //crates/lash-durable-test:worker_crashes__test \
  --test_arg=--exact \
  --test_arg=postgres::a_cell_start_worker_crash_is_never_a_recorded_host_cell_failure
```

These laws do not replace the crash row's two judged product-host kill arms;
keep missing browser evidence explicitly dependency-owned.

## Store evidence and teardown

All PostgreSQL session/process tables have the `lash_` prefix: use
`lash_graph_nodes`, `lash_session_meta`, `lash_session_head`,
`lash_pending_turn_inputs`, `lash_queued_work_batches`, `lash_deleted_sessions`,
`lash_processes`, `lash_process_events` and `lash_process_observers` wherever
the scenario asks for their SQLite counterparts. Preserve the same filters,
normalized JSON and expected facts. Do not query `lash-sessions.db` for these
facts in PG mode. Use the service's recorded port in every psql connection.

Stop the host while retaining the owned service, then run the materialized
`//crates/lashctl:lashctl` binary with `LASH_POSTGRES_DATABASE_URL` set to
that same database:

```sh
"$lashctl" --json preflight > 05-post-host-preflight.json
```

Require exit 0 and save its JSON; schema/capacity preflight does not certify
that session/process tables are empty. Remove the service through its owner,
then confirm host and service teardown. Fill the scenario scorecard plus:

| Item | Objective gate | Verdict | Evidence |
| --- | --- | --- | --- |
| PostgreSQL served | startup `store_backend` is `postgres` | | `00-startup.json` |
| Companion | exact PG law executed and passed | | test log and receipt |
| Scenario | every original scorecard gate judged unchanged | | scenario bundle and filled scorecard |
| Post-host preflight | same database, host stopped, exit 0 | | `05-post-host-preflight.json` |
| Isolation and teardown | owned host/service removed | | command log |

Record the served model, paid cost (zero for deterministic), and any Abort or
product finding. Do not substitute a weaker companion or change lifecycle
expectations to obtain a pass.
