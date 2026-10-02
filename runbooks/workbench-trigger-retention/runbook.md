# Workbench trigger retention

Read [the runbook rules](../RULES.md) first. This is an operator-only,
deterministic companion to [trigger lifecycle](../workbench-trigger-lifecycle/runbook.md).
Its gates are HTTP responses, typed refusals, and exact occurrence, delivery and
process counts. It makes no provider network call and has no browser score.

The workbench exposes two separate retention decisions through `core.processes()`:

- `POST /api/admin/trigger-occurrences/reclaim` returns the complete
  `lash::triggers::TriggerOccurrenceReclamationReport`. A cutoff can defer an
  eligible occurrence. It cannot reclaim a live delivery fan-out.
- `POST /api/admin/trigger-occurrences/forget-tombstones` calls
  `ProcessAdmin::forget_trigger_tombstones(written_before_epoch_ms)` and returns
  `forgotten` and the supplied `written_before_epoch_ms`.

Reclaim never deletes occurrence tombstones, even at `u64::MAX`. Forget selects
tombstones written **strictly before** the cutoff on the store's clock, rather
than occurrences ingested before it. A tombstone at or after the cutoff survives.
By forgetting, the host vouches that its source will no longer redeliver those
identities. A later redelivery runs again. There is no automatic expiry.
[ADR 0021](../../docs/adr/0021-trigger-deliveries-are-first-class-and-recoverable.md)
and [ADR 0067](../../docs/adr/0067-durable-rows-name-one-owner-and-one-reclaim-trigger.md)
own these contracts.

Both routes require `RunStoreMaintenance` authorization. The local example's
default authorizer allows every caller; a deployed host must enforce its
operator policy. Neither route has a UI button, a schedule, or a default cutoff.
The forget request rejects unknown fields. Store failures keep their typed cause;
a failed reclaim also returns its partial report. A failure never counts as an
empty successful pass.

## Phase 0: Start the disposable companion

Run from a Kiln fork with its environment loaded:

```sh
. ./env.sh
mkdir -p .buck2/trigger-retention-evidence
set -o pipefail
kiln test //examples/agent-workbench:agent-workbench__unit_test \
  --test_arg=tests::trigger_retention_tests \
  --test_arg=--nocapture --test_output=all \
  --test_sharding_strategy=disabled --runs_per_test=20 \
  2>&1 | tee .buck2/trigger-retention-evidence/operator.log
```

Require 4 executed tests per run, 20 runs, 80 passed and zero failed. Read the
invocation-specific report and `run-<k>` logs at the paths the command prints;
another test invocation can replace the default report links. Save these paths
and the revision with the scorecard. A successful exit with zero cases is a failure.

The companion in
[trigger_retention.rs](../../examples/agent-workbench/src/main_sections/tests/trigger_retention.rs)
starts the production admin router on an ephemeral loopback port. Each test owns
fresh SQLite memory stores and an in-process Restate server double. Its scripted
provider registers a button process through `send()`. The core records an explicit
`SessionSpec` with `max_tool_calls`; the test changes no live session config.
These are test fixtures, never a launch mode for a deployed workbench.

## Phase 1: Reclaim and recognise redelivery

`operator_reclaim_preserves_redelivery_fences` ingests one zero-match fired
occurrence and sends this operator request to the production route:

```sh
curl --fail-with-body -sS -X POST "$workbench_url/api/admin/trigger-occurrences/reclaim" \
  -H 'content-type: application/json' \
  -d '{"cutoff_epoch_ms":18446744073709551615}'
```

Read the typed report. Require `inspected_occurrence_count: 1` and
`reclaimed_occurrence_count: 1`. After advancing the store clock by a century,
the same request must report zero reclaimed occurrences. Redelivery must refuse
with `TriggerOccurrenceReclaimed` and create exactly zero occurrences, deliveries
and processes. The response and refusal counts appear in the operator log.

The matched process in Phase 3 is also fired, awaited to terminal success, and
pruned through `ProcessAdmin`. Require one pruned process and one pruned delivery.
That reconciliation reclaims its occurrence and writes its tombstone atomically;
an occurrence pass does not itself delete a retained delivery.

## Phase 2: Forget reports an exact count

`operator_forget_reports_exact_exclusive_write_time_count` ingests two occurrences
before `T = 4000000000000`, reclaims them at `T`, then writes one tombstone at
`T + 10`. This separates occurrence ingestion from tombstone write time.

The operator request is:

```sh
curl --fail-with-body -sS -X POST "$workbench_url/api/admin/trigger-occurrences/forget-tombstones" \
  -H 'content-type: application/json' \
  -d '{"written_before_epoch_ms":4000000000010}'
```

These curl forms document the route; the companion sends them to its own private
listener. Use them against a deployed stack only with an independently justified
cutoff. The fixture's timestamps and maximum cutoff are rehearsal values.

Require the following response counts, with the supplied cutoff echoed each time:

| Request cutoff | `forgotten` | Evidence |
| --- | --- | --- |
| `T` | 0 | The cutoff is exclusive |
| `T + 10` | 2 | Exactly the two older writes are forgotten |
| `T + 10`, repeated | 0 | Forget reports committed deletions |
| `u64::MAX`, fixture cleanup | 1 | Only the retained tombstone remained |

Before cleanup, both forgotten identities ingest again. Redelivering the identity
at the cutoff must still refuse, with zero new occurrences, deliveries and processes.

## Phase 3: Forgotten redelivery runs; a later fence still suppresses

`operator_redelivery_after_forget_runs_and_retained_tombstone_suppresses` registers
one button subscription, fires it and awaits its process. After terminal retention,
redeliver the same source, payload, session and idempotency key in a **fresh handler
journal**. Require `TriggerOccurrenceReclaimed` and zero new rows or processes.
Replaying the original handler would return its recorded receipt and prove nothing
about tombstones.

Forget before a finite cutoff greater than that tombstone's write time. Require
`forgotten: 1`. Fire a second identity, settle and reclaim it after the cutoff.
Repeat forget with the same cutoff and require `forgotten: 0`.

Redeliver the forgotten identity through another fresh journal. Require one new
occurrence, one new delivery and one new process id distinct from the original;
await terminal success. Redeliver the newer retained identity before and after
that run. Both attempts must refuse with `TriggerOccurrenceReclaimed`, with
exactly zero new occurrences, deliveries or processes.

## Phase 4: Refuse an implicit cutoff or a non-operator

`operator_forget_requires_an_explicit_cutoff_and_operator_authorization` requires
`422` for a missing cutoff and for an unknown field, and `403` when the fixture's
authorizer denies `RunStoreMaintenance`. Require one tombstone still present after
the denied request. No successful deletion count may accompany a denial.

## Scorecard and teardown

| Claim | Gate | Evidence |
| --- | --- | --- |
| Tombstones survive reclaim | Phase 1, 1 reclaimed then 0; redelivery creates 0 rows/processes | Per-run HTTP and refusal log |
| Forget selects exact write times | Phase 2, counts 0, 2, 0, 1 | Cutoff echoes and response bodies |
| Forgotten identity runs again | Phase 3, 1 forgotten and 1 new successful process | Original/new ids and delivery counts |
| A later tombstone still suppresses | Phase 3, 0 forgotten; both redeliveries create 0 rows/processes | Typed refusals and before/after counts |
| Operator decision is explicit | Phase 4, 422, 422, 403; 1 retained tombstone | Authorization/request test output |

Each test tears down its listener and owned double. No shared service or container
is started. Keep the operator log and invocation reports before removing the fork.
Any missing case, count mismatch or untyped refusal fails the scorecard.

FIG-4636's end-phase smoke report predates this companion. It supplies no retention
verdict; the four executed tests above supply this runbook's evidence.
