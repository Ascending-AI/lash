# Retention stays a parameterized host lever

## Decision

Hosts schedule differentiated retention through `prune_terminal_processes(cutoff, filter, watermark)`. `ProcessListFilter` selects provenance, identity and creation ranges; `ProjectionWatermark::UpTo(cursor)` limits deletion to acknowledged changes under ADR 0020, while `NoProjector` explicitly states there is no projector. The public `Processes::prune` forwards the filter and refuses filters selecting non-terminal work.

Eligibility protects pending deliveries, cleanup obligations and consumer holds. Pruning retains typed tombstone evidence and coordinates trigger retention under ADR 0021. Hosts retain process evidence beyond every still-replayable waiter; Lash supplies no finite maximum waiter lifetime.

## Running-process event release

Pruning reclaims retired processes only. A long-running process keeps
appending events across its segments, so hosts release the history of a
retained process, running or not, through
`ProcessRetention::release_process_events(process_id, through)`, which the
facade exposes as `Processes::release_events`. The host selects the prefix.
The store clamps it to the process's last event, never lowers a horizon an
earlier release raised and reports the horizon and the events the call
released, so repeated cleanup releases nothing new.

A release strips the payload of each event at or below the horizon and
keeps the row: sequence, type, replay key, invocation, timestamps and a
digest of the payload. A cancel request keeps its payload, because its
replays match on the cancellation rather than on payload bytes and a process
holds one. Sequence allocation and signal ordinals count rows, so they are
unchanged. A re-presented replay key of a released event coalesces when the
payload has the released digest and refuses as a durable-identity conflict
otherwise. Segment replays, tool children reattached across segments and
host signal retries can all re-present a key, and storage cannot observe when
the last of them has finished, so the release keeps every fence the way
trigger mutation receipts keep theirs. Release therefore reclaims payload
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
signal payloads travel through engine promises, and replay reads happen
inside recorded steps. A fresh event await below the horizon cannot recover
a released payload.

Release covers the event log only. Segment handovers stay bounded by the
retire step under ADR 0025. A continuation's size is live program data.
Restate invocation journals follow the engine's retention: each segment's
effect count is bounded by its budget, while result bytes have no such bound.
Restate's journal and workflow retention govern completed segment invocations.
Lash sets no retention on its
services, so the host configures them on its Restate deployment. Per-run TTLs
and automatic release policies are rejected for the same reason as producer
retention classes, and deleting released rows is rejected because it would
drop replay fences that no stored fact can prove dead.

The [release law](../../crates/lash-conformance/src/conformance/process_registry/event_release.rs)
runs on SQLite file, SQLite memory and PostgreSQL. The
[observation test](../../crates/lash/src/process_observation/tests.rs)
covers the typed read and snapshot gap.

The [growth inventory](../operations/process-history-release.md) records
retention owners, measured payload savings, remaining metadata growth and
the Restate configuration contract.

## Durable-core evidence retention

`SessionStoreFactory::reclaim_retained_evidence(RetentionBound)` is an explicit factory-wide lever with an exclusive commit-timestamp horizon. Only receipts belonging to durably deleted sessions are eligible. Permanent deleted-session identity evidence remains. Live receipts and usage deltas survive; terminal usage is reclaimed only when its matching receipt is absent.

SQLite and PostgreSQL delete eligible receipts and dependent usage in one fenced transaction. Reports describe committed counts; errors roll back the operation. Repetition after exhausting the eligible set removes nothing. Attachment liveness follows referrer and graph-retirement contracts under ADRs 0028, 0113 and 0124 rather than receipt age. SQL stores own no effect journals; Restate invocation journals use engine retention under ADR 0025.

`vacuum` cleans eligible tombstoned graph and terminal ingress rows without a receipt horizon. Blob GC uses its separate explicit policy. Trigger mutation receipts have a low-level pruning primitive but no public pruning facade or production schedule; age alone cannot prove a retry identity dead, so those receipts remain retained.

## Why and alternatives

Producer-declared retention classes are rejected because retention windows are host operational policy rather than an execution correctness declaration. A projector watermark is required because pruning unacknowledged state would destroy completeness evidence. The host chooses how much eligible evidence to keep; Lash owns eligibility and atomic deletion.

[Process retention contract](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs), [SQLite receipt retention](../../crates/lash-sqlite-store/src/retention.rs) and [PostgreSQL receipt retention](../../crates/lash-postgres-store/src/postgres/evidence_retention.rs) implement these levers.
