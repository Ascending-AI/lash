# Workbench crash-resume on the checkpoint graph

Read [the shared rules](../RULES.md) first. This is the deterministic
`workbench-crash-resume` row, TypeScript, `scripted-provider`, cost $0. SQLite is
the local rehearsal; the PostgreSQL store-axis lane owns that leg. The matrix
records both stores. Do not claim a store passed from another store's receipt.

A SIGKILL of the product host or its VM worker must lose no large binding,
repeat no completed Once effect, and resume the same cell from its committed
checkpoint. [ADR 0139](../../docs/adr/0139-the-lash-vm-is-a-dialect-free-kernel.md)
and [ADR 0110](../../docs/adr/0110-the-engine-owns-process-recovery.md) own those
rules. Graceful launcher restart alone does not prove them.

## Companion laws

Source `env.sh`. Run each line once, retaining the log, test report and final
`kiln: test passed executed=1 passed=1 failed=0` receipt. Zero executed is Abort.
These are existing laws, not substitutes for either product kill arm.

```sh
kiln test //crates/lash-durable-test:vertical_crash_proof__test --test_arg=--exact --test_arg=a_once_operation_in_a_cell_killed_after_its_work_resumes_without_replay_on_sqlite_memory
kiln test //crates/lash-durable-test:vertical_crash_proof__test --test_arg=--exact --test_arg=a_repeated_large_text_kept_across_awaits_is_written_once
kiln test //crates/lash-durable-test:vertical_crash_proof__test --test_arg=--exact --test_arg=a_fresh_large_text_per_await_is_written_once_per_await
kiln test //crates/lash-durable-test:cell_restore_laws__test --test_arg=--exact --test_arg=a_sleeping_cell_suspends_on_idle_eviction_and_wakes_once_at_its_deadline_on_sqlite_memory
kiln test //crates/lash-durable-test:worker_crashes__test --test_arg=--exact --test_arg=sqlite_memory::a_cell_start_worker_crash_is_never_a_recorded_host_cell_failure
kiln test //crates/lash-durable-test:round_crash_matrix__test --test_arg=--exact --test_arg=a_mixed_round_holds_once_and_repeatable_rules_across_every_cut
```

## Phase 0 — Boot an owned stack

Use a free port and fresh, separate data and run directories for each arm.
Keep evidence under `.kiln/<ticket>/run-root/workbench-crash-resume/typescript/sqlite/`.
The fixture is opt-in through the public environment selector below; startup
must warn about `crash-resume` and its trace must name that scenario, the
TypeScript dialect, the dev provider profile and the selected store.

```sh
. ./env.sh
export AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=crash-resume
export AGENT_WORKBENCH_DATA_DIR="$PWD/target/crash-host-data"
export AGENT_WORKBENCH_RUN_DIR="$PWD/target/crash-host-run"
(ulimit -v 8388608; /usr/bin/time -v bash scripts/agent-workbench-dev.sh up --port 7536)
```

Gate `/healthz`. Save `00-ready-state.json` from `/api/state` and `00-ready.png`.
This launcher builds the judged geometry through Kiln. A prebuilt judged host
may be passed as `AGENT_WORKBENCH_BIN` with its `LASH_VM_WORKER`; never launch
an output path directly. Record its build report and source revision.

## Phase 1 — Park one cell

Send one composer input: “Run the crash-resume checkpoint proof.” The scripted
cell in `examples/agent-workbench/src/crash_fixture.rs` binds a 204800-byte
string and an array of four distinct 32768-number chunks (131072 numbers,
1 MiB numeric payload before object overhead). Bounded fixture-tool results
construct the chunks without exceeding guest instruction or effect-value
bounds. It
calls `crash_fixture.increment` once, fingerprints both values, prints
`crash-resume parked`, and sleeps for 30000 ms. After the timer it fingerprints
the same bindings and ends through `chat.reply(JSON.stringify(...))` with marker
`crash-resume finished`, `before`, `after` and `count`.

Wait for the committed timer in `waits` (`kind='timer', state='pending'`) and
its cell row in `exec_snapshots`. `/api/work` is corroborating work evidence;
an empty process list does not mean the session has no parked cell. Capture
`01-parked-{state,work,snapshots,waits,store}.json`, `01-parked-trace.jsonl` and
`01-parked.png`. Read SQLite with a read-only connection. Record:

- session id from `/api/state.settings.session_id`, turn id from the accepted
  input, and cell identity from `exec_snapshots.exec_key`;
- every checkpoint node from `blobs WHERE encoding=1`: hash, envelope kind,
  byte length and SHA-256 of its bytes. These start with `LASH`; the kind after
  the magic is a varint. `graph_nodes` is the transcript graph, not this store;
- the one line in `<data-dir>/crash-once.jsonl` and its call id. The fixture
  appends and syncs on every body invocation; it performs no deduplication.

Kill before that pending timer resolves. If it resolved first, Abort this
arm: a kill after completion is no crash-resume evidence.

## Phase 2 — Two independent kill arms

**Arm A, host.** Read the exact PID and start time from the launcher's
`<run-dir>/workbench-*.pid`. Check `/proc/<pid>/stat` start time and command
against that owned receipt, then send `kill -9 <pid>` to that PID alone.
Save `02-killed-process.json` with PID, start time, command and observed death.
Use the non-destructive launcher `restart --port 7536` with the same data,
run directories and scenario. Do not reset or resend the input.

**Arm B, worker.** Start a fresh stack with `target/crash-worker-data` and
`target/crash-worker-run`; repeat Phase 1. Find a `lash-vm-worker` whose
`/proc/<worker>/status` PPid is the owned host PID, and verify its start time
and command. Kill only that exact child. Keep the host alive; allow its pool
to replace the worker at resume. Record both host and worker PIDs, death and
replacement. Restarting the host in this arm would obscure the worker proof.

## Phase 3 — Judge resume

Allow the original timer and dead-owner lease to expire. Within 120 seconds,
require the exact scripted terminal reply and an empty `active_turns` list.
Do not cancel the turn: that would judge cancellation instead of same-cell
resume. Capture `03-resumed-{state,work,store}.json`, `03-resumed-trace.jsonl`,
`03-binding-proof.json`, `03-once-ledger.jsonl`, and `03-resumed.png` after a
browser reload. Require:

1. Same session and admitted turn; one transcript code row and one successful `exec_code_completed` match
   the parked cell's identity. Compare start observations by session, turn,
   turn index, protocol iteration and code; their distinct identity count is
   one. `exec_code_started` is emitted before durable invocation lookup in
   `turn_driver/handlers.rs`, so it can repeat on handler re-entry after either
   kill. Retain that raw count; it is not a second admitted cell.
2. `before == after`, including the text SHA-256 and all four chunk SHA-256 values, `text_bytes=204800` and
   `array_items=131072`; `count=1`.
3. The invocation ledger has exactly one line and the trace exactly one
   `tool_call_completed` for `workbench_crash_increment`, with the same call id.
4. One user input and one turn reply, unique `transcript[].row_id` values,
   and the rendered terminal reply agrees after reload.
5. Shared node hashes and bytes remain equal; all parked large TextLeaf and
   BytesLeaf payloads remain, and at least 1 MiB of large StructureBlock bytes
   is reused for the array. Cell retirement collects obsolete frame/control
   nodes, so report added, retained and retired node/byte counts separately.
   No new large TextLeaf or BytesLeaf
   (kind 1 or 2, over 8192 bytes) may duplicate the retained payload. Large
   values use `INLINE_TOKEN_MAX=256` and `LEAF_PAYLOAD_MAX=65520`, not old
   MessagePack component thresholds. The companion growth laws additionally
   pin installation work, which a SQL snapshot alone cannot measure.

Launch Chromium uncapped and record its peak RSS with `/usr/bin/time -v`;
Chromium reserves over 32 GiB of virtual address space. Keep the host and
tests under the 8 GiB virtual-memory cap. Capture the host's `/proc/<pid>/status` VmHWM in addition to `/usr/bin/time -v`.
Always stop the owned stack with `bash scripts/agent-workbench-dev.sh down
--port 7536`, including on Abort; save the teardown receipt.

## Scorecard

Fill one row per arm. A missing gate is Abort; an observed mismatch is Fail.
A companion red is a product finding and never permission to change its law.

| Arm | Companion executed/passed | Exact owned SIGKILL | Same session/turn/cell | Hashes and lengths | Once body/completion counts | Graph reuse/growth | Transcript and reload | Teardown | Verdict/evidence |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Host | | | | | | | | | |
| Worker | | | | | | | | | |

Record source SHA, judged build report, store, fixture startup, artifact root
and cost ($0). Overall Pass requires both arms and all six companion laws.

[Recorded FIG-5987 scorecard](recorded-run.md).
