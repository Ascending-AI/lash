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


## Turn and session terminals

`LashCore::turns_changed_since(cursor, limit)` reads the deployment's durable
turn and session terminals in one bounded snapshot. `TurnChangeCursor` is
opaque and store-scoped. Each terminal receipt has an indexed `change_seq`.
Its accepting transaction raises `turn_change_clock`, so a later position
cannot commit ahead of an earlier one. Replaying a receipt raises no clock
and creates no second change. Plain state commits consume a position but
carry no terminal and are excluded by the partial index.

`TurnChangeKind::Committed` carries the operation and the existing typed
`TurnCommitOutcome`. `SessionFault` keeps the typed code, cause and origin of
each newly recorded fault, including episodes subsequently cleared by an
operator. `SessionDeleted` records physical deletion. These session records
share the clock and transaction with their accepting mutation, and have no
foreign key that would remove evidence when the session is deleted. They
retain failure facts, not live output or protocol activity. No push sink,
retry ledger or second live stream is introduced.

Pages include `retained_after` and the cursor after the last returned record.
An empty page advances to the snapshot's current clock. Cursors behind the
retention horizon refuse as `StoreError::TurnChangeCursorPruned`; future
positions refuse as `TurnChangeCursorAhead`. The host persists a cursor only
after applying its page. The live session cursor remains the latency path;
its gap triggers reconciliation through this durable record read.

`RetentionBound::turn_watermark` is explicit. `UpTo(cursor)` protects every
unacknowledged terminal; `NoProjector` is the host's deliberate opt-out.
Reclamation requires both acknowledgement and a durably deleted session,
and applies the exclusive timestamp horizon. Its transaction advances the
cursor horizon to the highest terminal actually removed. Standing fault
clears and session deletion never erase unread terminals. Permanent session
identity tombstones remain independent of this evidence sweep.

The SQL conformance laws `unread_turn_terminals_survive_retention` and
`terminal_feed_is_ordered_and_replay_stable`, and the Restate-double law
`a_disconnected_host_reconciles_a_failed_turn_after_live_replay_trims`, own
this contract. Each backend runs the same store laws.
