# Process change feed is a record-level cursor read

## Decision

A host projector reads `ProcessRegistry::processes_changed_since(cursor, limit)` for completeness. Process-row mutations advance a per-store monotonic change sequence. Pages return ordered `ProcessChange` values and the next cursor: `Upsert` carries the current record; `Deleted` carries a payload-free pruning tombstone. Event detail is read separately through `event_page`.

The cursor is opaque outside its issuing store. This host-level read does not filter by session observer edges. After tombstone compaction, a cursor behind the compaction horizon returns `ProcessChangeCursorPruned` rather than silently losing deletion evidence.

## Why and alternatives

A record cursor keeps truth in state reads while the sink under ADR 0017 supplies freshness. A store-wide event sequence is rejected because it imposes total ordering on every process event when projectors need changed state. A durable push sink duplicates retained state recovery and is rejected.

## Consequences

Projectors acknowledge a cursor and supply `ProjectionWatermark::UpTo(cursor)` when pruning. Hosts with no projector explicitly choose `NoProjector`. Mutation feeds can coalesce repeated changes to a record; they do not replace per-process event history. [Change-read contract](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs) and [SQL cursor and tombstone projection](../../crates/lash-sqlite-store/src/process_registry_change.rs) implement the feed.

## Trigger subscriptions

Hosts reconcile source provisioning through
`LashCore::triggers().changed_since(cursor, limit)`. Subscription commands,
session deletion and retention publish each subscription's latest desired
source state in the accepting transaction. Repeated edits coalesce, ordered by
the last change sequence. The source state includes its admitted provider route.
A tombstoned lifecycle asks the host to remove the source and survives physical
subscription deletion. No store wrapper or engine acknowledgement is needed.

Hosts apply changes idempotently by subscription id and reject older revisions
within the same incarnation. A new incarnation replaces the prior source.
They persist the cursor after applying the page, or atomically with their own
records. Re-reading an older cursor repeats the latest state safely.

`compact_subscription_tombstones(cutoff_epoch_ms)` removes deletion evidence
older than the host's retention horizon. A cursor behind that evidence returns
`PluginError::TriggerSubscriptionChangeCursorPruned`. The host resyncs through
`subscriptions_snapshot()`, an atomic read of all live subscriptions and its
continuation cursor, and removes its sources whose ids are absent. Normal
filtered subscription listings do not supply a safe continuation cursor.
