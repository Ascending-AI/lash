# Retention stays a parameterized host lever

## Decision

Hosts schedule differentiated retention through `prune_terminal_processes(cutoff, filter, watermark)`. `ProcessListFilter` selects provenance, identity and creation ranges; `ProjectionWatermark::UpTo(cursor)` limits deletion to acknowledged changes under ADR 0020, while `NoProjector` explicitly states there is no projector. The public `Processes::prune` forwards the filter and refuses filters selecting non-terminal work.

Eligibility protects pending deliveries, cleanup obligations and consumer holds. Pruning retains typed tombstone evidence and coordinates trigger retention under ADR 0021. Hosts retain process evidence beyond every reader that still awaits it; Lash supplies no finite maximum waiter lifetime.

## Running-process event release

Pruning reclaims retired processes only. A long-running process keeps
appending events while it runs, so hosts release the history of a
retained process, running or not, through
`ProcessRetention::release_process_events(process_id, through)`, which the
facade exposes as `Processes::release_events`. The host selects the prefix.
The store clamps it to the process's last event, never lowers a horizon an
earlier release raised and reports the horizon and the events the call
released, so repeated cleanup releases nothing new.

A release strips the payload of each event at or below the horizon and
keeps the row: sequence, type, idempotency key, timestamps and a digest of
the payload. A cancel request keeps its payload, because its retries match on
the cancellation rather than on payload bytes and a process holds one.
Sequence allocation and signal ordinals count rows, so they are unchanged. A
re-presented idempotency key of a released event coalesces when the payload
has the released digest and refuses as a durable-identity conflict otherwise.
Recomputed process phases and host signal retries can re-present a key, and
storage cannot observe when the last of them has finished, so the release
keeps every fence the way trigger mutation receipts keep theirs. Release therefore reclaims payload
bytes; the retained row of an event with a small payload stays roughly its
size.

Reads strictly after the horizon are unchanged. A page read starting below
it answers `ProcessEventHistoryRetention::Released` with the horizon, never a
short page, and the facade's event cursor resumes after it. An await for an
event after a released position refuses with `ProcessEventsReleased`. The
recent-event tail never returns a released event. An observation snapshot
folds the retained events and reports its durable summary incomplete with
`HistoryReleased`. The host must acknowledge its projections and decide that
event readers may expire before selecting the prefix. Stored waits and
outcomes remain on the process row, wake deliveries carry their own content,
and signal payloads travel on their wait rows
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §6). A fresh event await below the horizon cannot recover
a released payload.

Release covers the event log only. A VM snapshot replaces its predecessor
revision, and its size is live program data (ADR 0132 §8). A turn's phase
rows are pruned at its commit. Per-run TTLs and automatic release policies
are rejected for the same reason as producer retention classes, and deleting
released rows is rejected because it would drop idempotency fences that no
stored fact can prove dead.

The [release law](../../crates/lash-conformance/src/conformance/process_registry/event_release.rs)
runs on SQLite file, SQLite memory and PostgreSQL. The
[observation test](../../crates/lash/src/process_observation/tests.rs)
covers the typed read and snapshot gap.

The [growth inventory](../operations/process-history-release.md) records
retention owners, measured payload savings and remaining metadata growth.

## Durable-core evidence retention

`DeploymentStore::reclaim_retained_evidence(RetentionBound)` is an explicit factory-wide lever with an exclusive commit-timestamp horizon. Only receipts belonging to durably deleted sessions are eligible. Permanent deleted-session identity evidence remains. Live receipts and usage deltas survive; terminal usage is reclaimed only when its matching receipt is absent.

SQLite and PostgreSQL delete eligible receipts and dependent usage in one fenced transaction. Reports describe committed counts; errors roll back the operation. Repetition after exhausting the eligible set removes nothing. Attachment liveness follows referrer and graph-retirement contracts under ADRs 0028, 0113 and 0124 rather than receipt age. Turn phase rows are pruned at the turn commit and hold no retained evidence (ADR 0132 §4).

`vacuum` cleans eligible tombstoned graph and terminal ingress rows without a receipt horizon. Blob GC uses its separate explicit policy. Trigger mutation receipts are durable evidence under the same lever (FIG-4108): receipts older than the bound are reclaimed when ownerless (host/platform) or when their session owner is durably deleted and no outstanding delivery still names it, and a resent mutation then re-evaluates rather than returning the reclaimed receipt. The lever's bound is what proves a retry identity dead; the deleted-owner requirement is what makes that proof safe for session receipts. On both backends the sweep's receipt arm runs in the sweep's transaction, since SQLite keeps every table in one database file (ADR 0132 §12).

The host tool-intent submission ledger is evidence under the same lever (FIG-1509). Each row is the first-outcome idempotency fence of one host-submitted intent identity and belongs to the identity's owner session; the store stamps its admission time. A row admitted before the bound is reclaimed once its owner session is durably deleted, and the same transaction fences that owner in `tool_intent_retired_owners`. The fence is permanent identity evidence: every later submission under the owner answers `Reclaimed`, so a reclaimed identity is refused instead of realized again. A live owner's rows are never eligible, whatever their age. The proof is a join with `deleted_sessions` inside the sweep's transaction on both backends. On PostgreSQL submissions take the sweep's advisory key shared, so a claim cannot slip between the fence and the delete; SQLite serializes them on its one writer.

## Why and alternatives

Producer-declared retention classes are rejected because retention windows are host operational policy rather than an execution correctness declaration. A projector watermark is required because pruning unacknowledged state would destroy completeness evidence. The host chooses how much eligible evidence to keep; Lash owns eligibility and atomic deletion.

[Process retention contract](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs), [SQLite receipt retention](../../crates/lash-sqlite-store/src/retention.rs) and [PostgreSQL receipt retention](../../crates/lash-postgres-store/src/postgres/evidence_retention.rs) implement these levers.
