# E2E Scenario: Process Operations on Durable Infrastructure

> **Read [../RULES.md](../RULES.md) first.** This is the agent-judged semantic layer over
> the deterministic `just process-operations-e2e` companion. Do not replace the companion's
> PostgreSQL assertions with manual SQL, and do not treat a green script as the judgment itself.

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
It emits `process-operations e2e passed: scenarios=7` only after all exact assertions pass. The
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
5. **Retention never guesses.** Trigger mutation receipts survive process pruning. Delivery rows
   are deleted only after their process tombstone proves pruning, and an outstanding delivery
   prevents tombstone compaction.

## Phase 0 — Boot and establish durable geometry

Run the deterministic companion. Require all of these before judging later phases:

- `00-live-services.json` contains running Restate and MinIO services;
- `00-postgres-service.json` identifies the service publishing the assigned port;
- `00-postgres.json` reports that same assigned port;
- `restate-deployments.json` is a successful Restate Admin response; and
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

## Phase 4 — Each-wake versus coalesced wake turns

**Setup.** Inspect `04-wake-turn-policy.log`, produced by the complete PostgreSQL runtime
persistence contract with a fresh store for each vector.

**Action.** Enqueue two adjacent process wakes first under `WakeTurnPolicy::each_wake` and then
under `WakeTurnPolicy::coalesce` with one group key. Claim ready work at the idle boundary.

**Expected observable evidence.** Each-wake mode yields two distinct single-batch claims.
Coalesced mode yields one claim containing both receiver batches, and settling it removes both.

**Judgment — FAIL if:** Each-wake collapses turns, Coalesce creates two turns for the same group,
coalescing crosses a delivery boundary/key, or settlement leaves one member behind.

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

**Setup.** Inspect `07-retention.log`, whose PostgreSQL fixture creates trigger mutation receipts,
terminal delivery processes, unrelated delivery rows, and a guarded tombstone interleaving.

**Action.** Run `Processes::prune` semantics, reconcile trigger deliveries whose process ids are
tombstoned, and invoke delivery-aware `compact_tombstones` before and after reconciliation.

**Expected observable evidence.** Mutation receipts replay unchanged; only deliveries belonging
to pruned processes are removed; unrelated/live deliveries remain; the guarded tombstone first
refuses compaction while its delivery remains, then compacts only after reconciliation deletes
that delivery.

**Judgment — FAIL if:** pruning deletes mutation receipts, removes an unrelated delivery,
recovery resurrects pruned trigger work, compaction orphans a delivery, or a trigger-store survey
failure permits compaction.

## Phase 8 — Teardown and score

Require the companion's final `panic gate: clean` and
`process-operations e2e passed: scenarios=7` lines. Confirm its compose project and named crash
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
| Teardown | panic gate clean; no owned containers or volumes remain | | `process-operations-e2e.log`, container inventory |

**Aggregate:** did the live durable substrate expose enough typed, actionable evidence for an
operator to redrive, retarget, recover, reuse, and retain process work without weakening the
model/host boundary or manufacturing success after a crash?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md). A
failing live scenario is a product finding: preserve the artifact directory and stop; never
loosen an assertion or rewrite the judgment criterion during that run._
