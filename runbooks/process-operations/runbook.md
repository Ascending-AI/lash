# E2E Scenario: Process Operations on Durable Infrastructure

> **Read [../RULES.md](../RULES.md) first.** This is the agent-judged semantic layer over
> the deterministic `just process-operations-e2e` companion. Do not replace the companion's
> PostgreSQL assertions with manual SQL, and do not treat a green script as the judgment itself.


> **Workbench process replacement (FIG-1164, FIG-3035).** The non-destructive
> same-configuration restart is `scripts/agent-workbench-dev.sh restart` (no flag), wrapped by
> `just agent-workbench-restart <port>`. It keeps the Restate journals and the application data
> and is no longer blocked: FIG-3035 landed it, and it is the required path for every
> worker-replacement step below. See the
> [central lifecycle constraint](../RULES.md#agent-workbench-lifecycle-constraint-fig-1164);
> never substitute the destructive `agent-workbench-reset`.

**Purpose.** Prove that an operator can inspect and act on the process-operations surface on
real Restate, PostgreSQL, and MinIO geometry: typed wake failures, redrive, retargeting,
visibility policy, wake-turn policy, crash recovery, process-id reuse, and retention all remain
truthful in durable state.

**Deterministic companion.** Run with a fresh artifact directory:

```sh
LASH_PROCESS_OPERATIONS_ARTIFACT_DIR=<fresh-dir> just process-operations-e2e
```

The companion owns isolated Restate, PostgreSQL, and MinIO ports derived from the worktree slug,
kills a real worker container at the named crash checkpoint, and removes every container and
volume it owns on exit. PostgreSQL uses the worktree block's `+46` offset unless
`LASH_PROCESS_OPERATIONS_POSTGRES_PORT` overrides it.
It emits `process-operations e2e passed: scenarios=8` only after all exact assertions pass. The
artifacts are the backend truth for this judged runbook.

## Scenario-specific golden rules

1. **Typed terminals stay inspectable.** `TargetGone`, `Expired`, `Retargeted`, and
   `SequenceRewound` are durable discard reasons, not log strings inferred from retries.
2. **Redrive is explicit.** Only the delivery id named by `blocked_groups[].redrive_delivery_id`
   may be redriven. Retrying a later sequence or editing durable rows is a failure.
3. **Policy lenses do not erase the host rail.** `ProcessToolVisibilityFilter` narrows only model
   tools. Host list, signal, cancel, wake delivery, and retention remain complete.
4. **Crash evidence names both sides of the seam.** The sender must be `enqueuing` while one
   receiver batch already exists before the kill; after restart the same receiver batch remains
   singular and the sender becomes `enqueued`.
5. **Retention never guesses.** Trigger mutation receipts survive process pruning while their
   owners remain live. Delivery rows are deleted only after their process tombstone proves
   pruning, and an outstanding delivery prevents tombstone compaction. Once a session is
   permanently deleted and its final delivery is gone, its subscription and receipt cascade is
   expected reclamation, not a retention failure.
6. **Migrated tools have one full-tier contract without host-affecting authorities.**
   Judged runbooks must never hand agents host-affecting authorities (`shell.*`).
   Instead, deterministic world-tool and process authorities (`process.start`,
   `process.signal`, `processes.cancel`, `spawn_agent`) run against the fullest-authority
   deterministic host (Agent Workbench with its deterministic mock mail world and process
   engine), while protocol-standard `batch` runs against the host where the standard
   protocol is native (`slack-clone`). Each tool must succeed through its leaf-intent or
   process-replay shape on Restate exactly as it does on in-memory and PostgreSQL. Any
   FIG-1127 ordinal-tier refusal from those public tools is a regression. The legacy
   journal-capable `ToolContext` routes remain fenced until their aggregate removal.
7. **Parent teardown is a registry ledger row, not terminal-state cleanup.** An ended parent
   scope leaves exactly one unsettled `parent_end_plans` row keyed by `(parent_kind, parent_id)`.
   The row carries no action list: the process worker's sweep derives the work by querying the
   children that name that Parent Scope with a `Cancel` policy, requests `ParentEnded` cancel on
   each, and only then stamps the row settled. A crash anywhere in that sweep redrives without
   duplicating a child event, because a child already carrying a cancel request is no longer
   returned by the children query. Concurrent sweeps may race, but only one durable
   cancellation may remain for each child.
8. **Selected drains are closed over the requested batches.** A successful selected drain may
   settle only its exact batch set. Unselected pending rows remain pending, absent selected ids
   report `AlreadySatisfied`, and a present row that cannot join the exact composition retains its
   typed refusal without provider execution or queue mutation.

## Parent-end ledger preflight

Before a live process-operations judgment, run the registry conformance laws against a disposable
PostgreSQL database with the production-required gate enabled:

```sh
LASH_POSTGRES_DATABASE_URL=<disposable-url> LASH_REQUIRE_POSTGRES=1 \
  cargo test -p lash-internal-postgres-store --features testing --test conformance \
  parent_end --locked -- --nocapture --test-threads=1
```

The PostgreSQL tier is the one this preflight judges: `lash-internal-conformance` runs the
laws on the in-memory tier only, so the fail condition below can only fire from the store's own
conformance binary. The `parent_end` filter selects **three** test functions
(`terminal_completion_atomically_retains_parent_end_plan`,
`committed_turn_receipt_answers_the_parent_end_recovery_read`,
`settled_parent_end_plans_are_reclaimed_by_retention`); expect `3 passed`, not one test per law —
the laws below are assertions inside those three. The laws must show: a terminal parent writes exactly one unsettled ledger
row; the children query is index-served and returns only the `Cancel` children that still owe a
cancel; a `Cancel` child that registers after the row exists is refused `ParentEnded`; a child
that already carries a cancel request leaves the page; settlement is idempotent and stamps the
row rather than deleting it; and the row outlives the retention prune of the parent process row,
because the ledger is keyed by parent scope and not by process id.

**Fail if:** PostgreSQL is skipped, the ledger row is written without the terminal append, a
child is cancelled before the parent fact is durable, settlement deletes the row instead of
stamping it, or any redrive appends a second `process.cancel_requested` event.

## FIG-1293 migrated-tool atomicity judgment

Run these rows against the Agent Workbench's Restate-backed RLM tier before Phase 0. The
`process-start-detach` and `process-signal` rows exercise the RLM process language surface;
`processes-cancel` and `spawn-agent` use the Workbench's deterministic process/world tools.
The `protocol-batch` row exercises the RLM resource-operation batch over two deterministic
Workbench leaf tools. No judged row uses a host-affecting `shell.*` authority or the
standard-protocol `tools.batch` call.

Use a fresh session for each row and save the rendered transcript, `/api/state`, Restate
invocation/journal inspection, and `trace.jsonl` extract under a row-named artifact directory.
Submit the row's named tool call or RLM program and wait until its turn is active and its first
durable child command is visible. The next step requires a target-host restart that preserves the
same run/data directories and Restate container: run `scripts/agent-workbench-dev.sh restart`
(or `just agent-workbench-restart <port>`) from a shell that has sourced the fork's `env.sh`, so
the launcher can find the judged binary. Never use the destructive `agent-workbench-reset` and
never use Restate Admin kill as a substitute. After recovery,
reconcile DOM,
API/durable messages, trace executions, and the literal outcome below.

| Row | Call/program and literal oracle | Required Restate journal shape after worker replacement |
|---|---|---|
| `process-start-detach` | In TypeScript, define a deterministic process, invoke its `start` construct once, retain the returned process handle, and await that handle after worker replacement. Require the exact returned value and the same process id before and after recovery. The historical `-detach` row name is retained for artifact continuity; the RLM surface has no detach mode, so do not assert detached ownership or an externally owned terminal status. | One direct `StartProcess` command for the prepared process id and one direct await of that same handle; recovery retains the single launcher identity and result, with no duplicate launch, no `ToolAttempt`-nested process command, and no FIG-1127 refusal. |
| `process-signal` | In TypeScript, author a process literal that awaits a `ping` signal, start it with `await processes.start({ process })`, and send literal payload `"ping"` with `await processes.signal({ handle, name: "ping", payload: "ping" })`. Require the process's returned value to be the one observed payload; do not expect a public `process.signal` result wrapper. | One `processes.start` leaf call and one `SignalProcess` command for the same prepared process id, with signal `ping` and payload `"ping"`; recovery delivers the signal once and the process returns the observed payload once. |
| `processes-cancel` | Start a long-lived tracked process, call `processes.cancel` for its exact id, and require `{"process_id":<started-id>,"status":"cancelled"}`. | One `CancelProcess` command and one `process.cancel_requested` event for the exact id; redrive emits neither a duplicate event nor an ordinal-tier refusal. |
| `spawn-agent` | Call `spawn_agent` with a schema requiring `{"answer":"str"}` and require one matching child result. | The orchestration body has no enclosing `ToolAttempt`; its one start and one await are direct process-replay children for the same prepared child id, and recovery creates one child session/result. |
| `protocol-batch` | On the Agent Workbench Restate/RLM profile, ensure the deterministic mock `work` and `personal` Inbox authorities are present, then issue two literal side-effect-free `list({})` calls in one aggregate (`await Promise.all([inbox.work.list({}), inbox.personal.list({})])`). Require the ordered two-element result vector. This is the RLM resource-operation batch, not a `tools.batch` call. | The RLM resource-operation batch has no enclosing `ToolAttempt`; only the two Inbox leaves have attempt frames, and recovery retains their source order and exactly one result per leaf. |

**Pass only if:** all five rows recover to their literal outcomes, the three-layer counts and ids
agree, each Restate journal has the required shape, no old ordinal-tier refusal appears, and the
replacement PID differs while the Restate container id and session id remain unchanged. Any
timeout, duplicate command/result/event, missing frame relationship, or layer mismatch is an
Abort/RCA under `RULES.md`.

## Phase 0 — Boot and establish durable geometry

Run the deterministic companion. It prints `process-operations e2e passed: scenarios=8` on
success; that is the index of the last scenario, and nine scenarios (0 through 8) actually run —
require one `scenario <n> evidence:` line for every index 0-8, not eight lines. Require all of
these before judging later phases:

- `00-live-services.json` contains running Restate, PostgreSQL and MinIO services;
- `00-postgres-service.json` identifies the service publishing the assigned port;
- `00-postgres.json` reports that same assigned port;
- `restate-deployments.json` is a successful Restate Admin response. The companion registers no
  service deployment of its own, so `{"deployments": []}` is the expected passing body; gate the
  successful response, never a non-empty list; and
- `00-minio-conformance.log` reports a passing S3-store conformance run.

**Fail if:** any service is absent, PostgreSQL is exposed on another host port, MinIO object
round-tripping fails, or the script leaves its compose project running after exit.

## Phase 1 — Typed discard outcomes and redrive

**Setup.** Inspect `01-wake-delivery.log` from the PostgreSQL wake-delivery crash matrix. The
fixture creates a permanently deleted target, an expiring delivery, and a two-delivery ordering
group whose head is discarded.

**Action.** Follow the asserted transition: list the wake-delivery report, select the exact
`redrive_delivery_id` named for the blocked group, and invoke `redrive_wake_delivery` for that id.

**Expected observable evidence.** The deleted target is durably `discarded/target_gone`; the
expiry-bound delivery is `discarded/expired`; the report first names one blocked group and its
actionable head, then the group disappears after redrive.

**Judgment — FAIL if:** a missing target that never existed is reported as `TargetGone`, either
typed reason is reduced to a retry/log message, a later sequence bypasses the discarded head, or
redrive does not clear the named block.

## Phase 2 — Retarget a wake subscription

**Setup.** The PostgreSQL worker fixture registers a process against an old target, appends an
old-target wake, and creates the replacement target.

**Action.** Retarget the subscription, inspect the audit event and old delivery, then append and
drive a new wake. Also inspect the in-flight retarget race retained by the wake crash matrix.

**Expected observable evidence.** The old pending delivery is `discarded/retargeted`, the event
tail contains `process.subscription_retargeted`, and the next wake is queued only for the new
target. A delivery already claimed before the retarget settles truthfully to its claimed old
target exactly once rather than being relabeled.

**Judgment — FAIL if:** old pending work is delivered, the new wake targets the old session, the
audit event is missing, or the in-flight race duplicates or rewrites historical delivery truth.

## Phase 3 — Model-tool visibility versus the host rail

**Setup.** Inspect `03-tool-visibility.log`. The fixture installs a
`ProcessToolVisibilityFilter` that hides process operations from the model while observed
processes remain registered.

**Action.** Compare the model-tool list and typed model operation outcomes with the session host
process list; then use the host rail to signal one hidden process and cancel the other.

**Expected observable evidence.** Model-tool operations return the typed visibility miss, while
the host list contains both processes and their event tails contain `signal.ready` and
`process.cancel_requested` respectively.

**Judgment — FAIL if:** the filter hides a process from the host rail, widens model visibility,
changes a hidden operation into an untyped failure, or suppresses host signal/cancel evidence.

## Phase 4 — Per-event merge eligibility and default wake batching

**Setup.** Inspect `04-wake-turn-policy.log`, produced by the complete PostgreSQL runtime
persistence contract with a fresh store for each vector.

**Action.** Enqueue two adjacent turn-work rows without merge keys, then enqueue two adjacent
process wakes carrying `PROCESS_WAKE_MERGE_KEY`. Claim ready work at the idle boundary.

**Expected observable evidence.** The absent-key rows yield two distinct single-batch claims.
The wakes yield one claim containing both receiver batches, and settling it removes both.

**Judgment — FAIL if:** absent keys collapse turns, compatible default-key wakes create two
turns, batching crosses a delivery boundary/key/authority gate, or settlement leaves one member
behind.

## Phase 5 — Crash between receiver enqueue and sender mark

**Setup.** Read `05-crash-prepare.jsonl`. It names one process, one target session, and one wake
sequence. Read `05-crash-window.jsonl` and require checkpoint
`receiver_enqueued_sender_unmarked`, including its claim token and receiver batch id.

**Action.** Confirm `05-killed-exit-code.txt` records the forced container kill, then inspect the
fresh worker's `05-crash-recovered.jsonl` result.

**Expected observable evidence.** Recovery reports `floor_absorbed: 1`, sender state `enqueued`,
at least two delivery attempts, the original receiver batch id, and `receiver_turn_count: 1`.

**Judgment — FAIL if:** the killed worker marks the sender first, recovery creates a second
receiver turn, the receiver row disappears, the sender remains non-terminal, or a stale claim
token can settle after the new worker owns the claim.

## Phase 6 — Reuse a process id after prune

**Setup.** In `01-wake-delivery.log`, locate the frozen-clock prune/re-register and restored-store
rewind vectors.

**Action.** Deliver and settle the old incarnation's wake, complete and prune the process,
re-register the same process id, append a new wake, and drive it. Separately seed the receiver
floor above a restored sender and drive the forced rewind followed by a healthy later sequence.

**Expected observable evidence.** The re-registered process allocates a strictly greater wake
sequence and reaches the receiver. The forced rewind is durably `discarded/sequence_rewound`,
does not block its ordering group, and the later healthy wake is delivered.

**Judgment — FAIL if:** process-id reuse collides with the old delivery id, reuses or lowers the
sender sequence, silently absorbs the new wake, blocks behind `SequenceRewound`, or loses the
healthy successor.

## Phase 7 — Process and trigger retention

**Setup.** Inspect `07-retention.log`, whose PostgreSQL fixture creates live-owner and
deleted-session trigger mutation receipts, terminal delivery processes, unrelated delivery rows,
and a guarded tombstone interleaving.

**Action.** Run `Processes::prune` semantics, reconcile trigger deliveries whose process ids are
tombstoned, and invoke delivery-aware `compact_tombstones` before and after reconciliation.

**Expected observable evidence.** Live-owner mutation receipts replay unchanged through PROCESS
pruning; only deliveries belonging to pruned processes are removed; unrelated/live deliveries
remain; the guarded tombstone first refuses compaction while its delivery remains, then compacts
only after reconciliation deletes that delivery. After the ADR 0049 frontier confirms permanent
session deletion and the final delivery is gone, reconciliation also deletes that session's
subscriptions and receipts; this cascade is the successful retention outcome.

**Judgment — FAIL if:** PROCESS pruning deletes a live owner's mutation receipts, removes an
unrelated delivery, recovery resurrects pruned trigger work, compaction orphans a delivery, a
trigger-store survey failure permits compaction, or deleted-session receipt cascading occurs
before permanent deletion and the final-delivery fence.

## Phase 8 — Selected-drain scope isolation

**Setup.** Inspect `08-selected-drain.jsonl`. The PostgreSQL fixture enqueues selected batch A
followed by unselected batch B, then executes only A through the public selected-drain facade.

**Action.** Require checkpoint `selected_drain_scope_isolated`, replay A by the same durable batch
id, then inspect the deliberately unclaimable two-row selection separated by another merge key.

**Expected observable evidence.** A reports `ClaimedNow` with exactly one provider call; B remains
pending after A. Replaying A reports `AlreadySatisfied` without another provider call. The later
selection reports `UnclaimableTogether`, names the unclaimed row, and leaves B plus every refusal
fixture row pending in original order.

**Judgment — FAIL if:** A's selected turn settles B, replay executes a turn, a present-row refusal
is converted to satisfaction, the refusal reaches the provider, or any refusal-path row moves or
disappears.

## Phase 9 — Teardown and score

Require the companion's final `panic gate: clean` and
`process-operations e2e passed: scenarios=8` lines. Confirm its compose project and named crash
container no longer exist.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Durable geometry | Restate/PostgreSQL/MinIO live on assigned ports; S3 conformance green | | `00-*`, `restate-deployments.json`, `00-minio-conformance.log` |
| Typed discard + redrive | exact `TargetGone`/`Expired`; named block clears | | `01-wake-delivery.log` |
| Retarget | old pending `Retargeted`; audit; next wake reaches new target | | `02-retarget.jsonl` |
| Visibility lens | model narrowed; host list/signal/cancel complete | | `03-tool-visibility.log` |
| Wake-turn policy | two Each claims versus one two-batch Coalesce claim | | `04-wake-turn-policy.log` |
| Worker crash recovery | kill at named seam; one receiver turn after restart | | `05-crash-*.jsonl`, `05-killed-exit-code.txt` |
| Process-id reuse | fresh monotone sequence delivered; rewind typed and non-blocking | | `01-wake-delivery.log` |
| Retention | receipts retained; delivery reconciliation; guard blocks compaction | | `07-retention.log` |
| Selected-drain isolation | A claimed alone; B pending; replay/refusal typed and non-mutating | | `08-selected-drain.jsonl` |
| Teardown | panic gate clean; no owned containers or volumes remain | | `process-operations-e2e.log`, container inventory |

**Aggregate:** did the live durable substrate expose enough typed, actionable evidence for an
operator to redrive, retarget, recover, reuse, and retain process work without weakening the
model/host boundary or manufacturing success after a crash?

---

## History

- **FIG-1402**: Fixed FIG-1292 parent-end preflight prose to claim exactly what the PostgreSQL
  law file (`process_parent_atomicity.rs`) tests: a `ToolCall` parent with a single child
  cancellation settled across concurrent startup scanners. Recorded the honest gap that tests for
  segmented Lashlang parents, crash intervals, and post-clear redrive are not yet implemented in
  that law suite.
- **FIG-1402**: Re-targeted FIG-1293 migrated-tool atomicity judgments away from host-affecting
  `shell.*` authorities to deterministic world-tool and process authorities on the Agent
  Workbench, and targeted protocol `batch` on `slack-clone` where the standard protocol is
  native, upholding the ruling that judged runbooks must never hand agents host-affecting
  authorities.

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md). A
failing live scenario is a product finding: preserve the artifact directory and stop; never
loosen an assertion or rewrite the judgment criterion during that run._
