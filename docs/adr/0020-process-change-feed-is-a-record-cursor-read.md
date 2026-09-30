# Process change feed is a record-level cursor read

## Decision

A host projector reads `ProcessRegistry::processes_changed_since(cursor, limit)` for completeness. Process-row mutations advance a per-store monotonic change sequence. Pages return ordered `ProcessChange` values and the next cursor: `Upsert` carries the current record; `Deleted` carries a payload-free pruning tombstone. Event detail is read separately through `event_page`.

The cursor is opaque outside its issuing store. This host-level read does not filter by session observer edges. After tombstone compaction, a cursor behind the compaction horizon returns `ProcessChangeCursorPruned` rather than silently losing deletion evidence.

## Why and alternatives

A record cursor keeps truth in state reads while the sink under ADR 0017 supplies freshness. A store-wide event sequence is rejected because it imposes total ordering on every process event when projectors need changed state. A durable push sink duplicates retained state recovery and is rejected.

## Consequences

Projectors acknowledge a cursor and supply `ProjectionWatermark::UpTo(cursor)` when pruning. Hosts with no projector explicitly choose `NoProjector`. Mutation feeds can coalesce repeated changes to a record; they do not replace per-process event history. [Change-read contract](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs) and [SQL cursor and tombstone projection](../../crates/lash-sqlite-store/src/process_registry_change.rs) implement the feed.
