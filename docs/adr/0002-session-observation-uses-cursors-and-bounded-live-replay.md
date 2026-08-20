# ADR 0002: Session Observation Uses Cursors And Bounded Live Replay

Status: accepted

## Context

Lash has two different observation needs:

- Durable session state for settled UI and host reconciliation.
- Live semantic activity for one running turn.

`SessionReadView` already represents the durable projection. `TurnActivity` already represents per-turn semantic activity such as prose deltas, reasoning, tools, usage, and errors. Reconnect/resume should not make `TurnActivity` a durable history API, and it should not require a cursor scoped to a request or turn.

## Decision

Reconnect is session-level. A host observes a `SessionObservation`: the current `SessionReadView` plus an opaque `SessionCursor`.

`SessionObservationEvent` advances that cursor. Events may wrap `TurnActivity`, but they can also represent durable `Committed` replacements, revision-stable `ResidentChanged` replacements, Agent Frame switches, queued-work changes, process changes, or replay gaps. `SessionRevision` names the durable committed point: the store `head_revision` for persisted sessions and a process-local revision for in-memory sessions. Only `Committed` proves that revision advanced and may settle provisional transcript state. `ResidentChanged` makes changed resident authority replayable within one store incarnation without claiming durability.

Live replay is best-effort and bounded. `LiveReplayStore` is not `RuntimePersistence`, not durable history, and not required to survive process loss. The default `InMemoryLiveReplayStore` keeps at most 2048 events or 120 seconds per session. Hosts that need a deployment-specific buffer can pass a custom store through `LashCoreBuilder::live_replay_store`.

`SessionCursor` is opaque outside core and binds replay incarnation, durable revision, live position, and session identity into one token. Malformed cursors are invalid input. Cursors for a different session are rejected. A replay-incarnation mismatch returns `Gap(Unavailable)`, never clean empty, unless a host store genuinely preserved both replay history and incarnation across restart. A store that cannot prove both must present a fresh incarnation. Stale or trimmed cursors return a fresh `SessionObservation` plus `LiveReplayGap`.

## Consequences

`TurnBuilder::stream_to`, pull-style `stream`, `run`, and `TurnOutput.activities` remain turn convenience APIs. They are not the reconnect surface.

Remote protocol turn requests no longer carry a turn-level activity cursor field. `RemoteTurnActivity.sequence` remains only per-stream ordering. Remote session observation uses `RemoteSessionCursor`, `RemoteSessionObservation`, `RemoteSessionObservationEvent`, and `RemoteLiveReplayGap`; the protocol does not serialize a full `SessionReadView`.

Live replay reservation or publication failures must not fail turn execution or durable commits. They are logged, and later reconnect falls back to gap recovery from durable state.

Publication reserves a cursor batch with `LiveReplayStore::prepare_publication`, installs the authoritative `RuntimeObservation` carrying the reserved tail cursor, then calls `publish_prepared` to make the batch replay-visible and notify subscribers in cursor order. Reserved cursors are valid during the install-to-publish window. Dropping a prepared batch retires the missing interval as `Gap(Unavailable)`; it can never become a clean empty replay.

This is a host-facing trait break. Custom stores must replace the removed one-step `append` implementation with `prepare_publication` plus `publish_prepared`, including reservation abandonment and ordered visibility. There is no compatibility adapter or dual publication path.

Revision reconciliation happens at the public runtime subscription seam, where the authoritative projection is available. After positional replay or subscription setup, a cursor whose revision trails the current observation may continue only when the returned replay contains a `Committed` event bridging to that observation revision; otherwise the runtime returns `Gap(Unavailable)` with the authoritative replacement and latest cursor. Auxiliary events carrying the newer revision are not commit evidence, so a failed commit-event append cannot become a clean empty replay even when the session remains idle.
