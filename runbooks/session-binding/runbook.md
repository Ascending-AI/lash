# Session binding runbook

Use this runbook when changing facade session storage selection, park/resume,
turn control, or session administration. The goal is to make noncoincident
owners visible: use different values for the exact root store, root catalog,
child catalog, and effect deployment so an accidental fallback cannot pass.

This is a developer checklist triggered by a diff, not a scenario a shard drives:
it names no scenario host, no prompt, no browser surface and no provider, and its
evidence is a list of commands. Read it that way.

## Fixture

The lettered fixture below is a **reading aid for the observations, not a fixture any
command here constructs**. Each regression test named under Evidence builds its own
equivalent; do not try to stand this up by hand and then run the tests against it, because
you will not reproduce the one they use.

Configure a core with catalog **B**, child catalog **C**, and control deployment
**control-B**. Open root session `binding-root` with an explicit store **A** and
run one accepted turn. Record each catalog open/create count, each store's
cancel rows, and each control deployment's gate resolutions.

## Required observations

1. Read, enqueue, cancel, park, resume, and close `binding-root`. Every store
   operation lands in **A** and every gate operation lands in the control owner
   captured when the session opened. Resuming through a core with different
   live provider or plugin configuration keeps those lifecycle owners.
2. Create a managed child. Its store comes from **C**. It never aliases **A**
   and never falls back to **B**.
3. Use the core's arbitrary-session driver against a session in **B**. It opens
   the target once, then retains that handle through cancellation recording and
   receipt readback.
4. These are two different drivers with two different outcomes; check both, and do not
   expect the second to behave like the first.
   - The **session-bound** driver over a revoked or omitted target gate: cancellation
     returns `UnknownOrRevoked`, performs zero catalog lookups, and writes no row.
   - The **catalog** (arbitrary-session) driver over a catalog that fails every lookup:
     it performs exactly **one** open and surfaces a typed `RuntimeStore` error. One open,
     not zero — the failure is reported after the open attempt, not instead of it.
5. Delete through an owner-issued native context. Inject a failure after the
   storage tombstone but before journal retirement, then retry using the same
   administration owner. The retry completes cleanup without reopening the
   session. Repeat through the Restate-installed administration inside a real
   handler; preserve its existing direct `DeleteSession` process command.
6. Fork a session's revision, delete the source session, and fork again from
   the surviving fork's creation revision. The owning catalog still creates the
   destination and applies its own admission and fences.

## Evidence

Run `kiln build` for the workspace compile and `kiln clippy` for the workspace
lint gate. Then run the focused binding and deletion regressions.

The facade regressions are one filtered run of the lash unit-test binary —
twelve filters, nineteen tests:

```bash
kiln test --test_output=all //crates/lash:lash__unit_test \
  --test_arg=session_lifecycle::session_binding:: \
  --test_arg=resumed_session_observe_wait_cancel_drive_keep_original_owners \
  --test_arg=related_session_opens_with_parent_and_runs_a_turn \
  --test_arg=core_store_factory_is_used_for_sessions_created_from_a_running_session \
  --test_arg=durable_acquisition \
  --test_arg=existing_session_apis \
  --test_arg=open_of_a_missing_id_is_unknown_session_and_writes_no_row \
  --test_arg=durable_operations_on_a_deleted_id_report_the_tombstone \
  --test_arg=a_session_close_releases_its_running_roots_execution \
  --test_arg=core_delete_session_retires_the_deleted_session_effect_journal \
  --test_arg=fork_distinguishes_collected_point_from_retained_orphaned_source \
  --test_arg=a_fork_captures_the_config_of_its_fork_point
```

Expect exactly nineteen tests and `19 passed; 0 failed`. Each `--test_arg` is a
substring filter against the test's full module path, so a renamed or deleted
law drops the count rather than failing quietly.

The keyed-promise contract the session-bound and catalog drivers both ride —
exact scope, revocation, `UnknownOrRevoked` after session deletion — is the
effect host's, and its executable conformance law lives one crate down:

```bash
kiln test --test_output=all //crates/lash-restate:lash-restate__unit_test \
  --test_arg=conformance_and_poison::turn_work_driver
```

Expect `1 passed; 0 failed`. It carries the required observation that has no
facade-side harness: a `request_cancel` issued after
`revoke_await_events_for_session` answers `TurnCancelOutcome::UnknownOrRevoked`
and the reserved terminal promise is torn down with it.

How the selections map to the required observations:

| Observation | Selected laws |
|---|---|
| 1. Park/resume keep the recorded owners | `session_lifecycle::session_binding::` (4 laws: the two `resume_` witnesses, the delete-retry law, and the parent-relation readback) and `resumed_session_observe_wait_cancel_drive_keep_original_owners`, which parks on one core and drives observe/wait/cancel on another while the source driver reads the cancel back |
| 2. The child's store is its own, never the parent's or a fallback | `related_session_opens_with_parent_and_runs_a_turn`, `core_store_factory_is_used_for_sessions_created_from_a_running_session`, and `session_binding`'s `parent_relation_is_read_back_and_a_conflicting_create_is_refused` |
| 3. The catalog driver opens the target once and keeps that handle | `durable_acquisition` (3 laws: once-per-handle, one retry across clones, the SQLite absent/metadata-only/checkpointed spread) |
| 4. Revoked or failing bindings answer typed, without side effects | `conformance_and_poison::turn_work_driver` for `UnknownOrRevoked`; `existing_session_apis` (3 laws counting zero admissions and one catalog open per verb over a failing catalog), `open_of_a_missing_id_is_unknown_session_and_writes_no_row`, and `durable_operations_on_a_deleted_id_report_the_tombstone` for the no-row/no-lookup halves |
| 5. Delete retries through its obligation and the installed administration | `session_binding`'s `a_failed_journal_retirement_is_retried_by_the_delete_obligation`, `core_delete_session_retires_the_deleted_session_effect_journal`, and `a_session_close_releases_its_running_roots_execution` (the Restate-installed administration deleting from inside a real handler) |
| 6. A retained point outlives its source session | `fork_distinguishes_collected_point_from_retained_orphaned_source` and `a_fork_captures_the_config_of_its_fork_point` |

Both live durable geometries — `just agent-workbench-restate-e2e` and
`just restate-postgres-workers-e2e` — are also required, but **not in one pass**: each
brings up its own compose stack, each costs tens of minutes on a cold fork, and they cannot
share a gate slot. Budget them as two serial runs.

Record the exact source SHA, command, exit code, and log path. This runbook has no
`LASH_*_ARTIFACT_DIR` of its own, so name a destination directory explicitly on the command
line and keep the pass record there with the run artifacts; otherwise the evidence is
whatever the runner happened to keep. A source-only grep or a coincident all-in-memory fixture is
not evidence for owner selection.
