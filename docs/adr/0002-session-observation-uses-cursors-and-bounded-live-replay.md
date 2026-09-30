# ADR 0002: Session observation uses cursors and bounded live replay

Status: accepted

## Context

Hosts need durable session state for reconciliation and live semantic activity for a running turn. `SessionReadView` supplies the durable projection; `TurnActivity` supplies prose, reasoning, tools, usage and errors. Reconnect needs one session-level identity rather than a request- or turn-local cursor.

## Decision

A `SessionObservation` combines the current read view with an opaque `SessionCursor`. Observation events can carry turn activity, committed replacements, resident replacements, frame switches, queue changes, process changes and replay gaps. Only `Committed` proves a durable revision advance and settles provisional transcript state. `ResidentChanged` records resident authority without claiming durability. Persisted revisions use the store head; an ephemeral runtime uses its turn index.

Live replay is bounded, best-effort freshness. The default `InMemoryLiveReplayStore` retains at most 2048 events or 120 seconds per session, 4096 resident sessions, and 64 MiB of charged retention across the store. The byte budget includes reserved publications, serialized payload size measured without allocating an encoded copy, inline descriptors and live-channel slot allowances. It is a retention accounting limit, not a bound on allocator RSS or event handles held by consumers. Oversized reservations fail before acquiring positions and invalidate existing continuity so a dropped publication cannot look like an empty replay.

Normal operations perform at most 64 global idle-entry expirations. A host calls `InMemoryLiveReplayStore::expire_idle_sessions` during traffic-free periods to release all expired entries without reading each session. `trim_session` also performs bounded global expiry after trimming its target. Access to a session refreshes its idle deadline; retained events still expire by their append age. Capacity pressure evicts the oldest idle entry. Invalidation, idle expiry and capacity eviction remove the entry, close its live channel and retire its reservations. A recreated entry starts above a store-wide position watermark, so old cursors return `Gap(Unavailable)` without retained tombstones. The replay incarnation remains store-scoped; positions are opaque and their initial value is not fixed. Live channels carry weak event references; a payload released before consumption produces subscriber lag rather than retaining it outside the replay budget. Hosts can supply a custom store through `LashCoreBuilder::live_replay_store`. This buffer is independent of durable session storage.

The cursor binds replay incarnation, session identity, revision and live position. Malformed or cross-session cursors are rejected. A different incarnation, trimmed history or unavailable interval produces a gap and an authoritative replacement observation. A store preserves an incarnation across restart only if it preserves the matching history too.

Publication reserves a batch through `prepare_publication`, installs the authoritative observation with the reserved tail cursor, then calls `publish_prepared`. Subscribers see ordered published batches. Reserved cursors are valid during installation; abandoning a reservation produces `Gap(Unavailable)`. Reservation and publication errors are logged without failing execution or durable commits.

At the runtime subscription boundary, a revision behind the authoritative observation requires a replayed `Committed` event bridging to that revision. Auxiliary events are insufficient evidence. Without the bridge the result is `Gap(Unavailable)`.

## Consequences

Turn streams and `TurnOutput.activities` are convenience APIs. Reconnect uses session observation. The remote protocol carries its observation DTOs and opaque cursor rather than a full read view; per-stream activity sequence numbers provide ordering only. Custom live replay stores implement reservation, abandonment and ordered publication themselves.

Durable activity logging is rejected for this interface because settled history already has a store and live transport failure must not affect a commit. The implementation is in [replay](../../crates/lash-core/src/runtime/observation/replay.rs) and [publication and revision reconciliation](../../crates/lash-core/src/runtime/observation.rs).
