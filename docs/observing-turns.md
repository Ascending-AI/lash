# Observing turns

Turn activities arrive on the session observation stream in cursor order.
Keep the cursor and use the bounded live replay described in
[ADR 0002](adr/0002-session-observation-uses-cursors-and-bounded-live-replay.md)
when reconnecting. A replay gap means the missing activities cannot be
reconstructed from the durable session view.

Start a feed from `session.observe().snapshot().await` (or
`recoverable_chat_snapshot().await`): the session's durable head with a
cursor bound to its revision. A feed judges its cursor against the durable
head, and a gap's replacement snapshot is the durable head too, so an
observer whose own handle never adopted a commit another process made still
gets a current snapshot. Which processes' activities reach the feed is the
configured live replay store's property: the in-memory default holds one
process's, and a shared store holds every process's.

`CheckpointRecorded { protocol_iteration }` marks an accepted checkpoint on the
turn's lane. Every non-retracted delta before the marker belongs to the
checkpointed state. A cancelled tool batch can itself be checkpointed, so the
marker decides the cut. The marker is live-only: like every other turn
activity, it is absent from durable history.

## Placing committed rows

A live activity is provisional. `Committed { rows, .. }` supplies the new
canonical records for that commit, including named suppressions; it never
repeats the earlier transcript. The remote event transports the same `rows`
and omits the local read view. Replace the preview for each record's typed
turn provenance, then style its neutral content. Read `durable.transcript()`
for a complete retained transcript after reconnecting across a replay gap.

```rust,ignore
if let lash::observe::SessionObservationEventPayload::Committed { rows, .. } = &event.payload {
    for row in rows.iter().filter(|row| row.suppressed.is_none()) {
        place_row(row); // the host's presentation adapter
    }
}
```

`row_id` supports equality and transport. A snapshot's `RowOrdinal` supports
ordering only: it is absent from the row record and cannot be persisted or
used as a cursor.

## A stopped turn's tail: the live stream is the contract

Lash keeps no durable record of what a stopped turn streamed after its last
checkpoint, and never feeds it back
([ADR 0122](adr/0122-a-stopped-turns-uncommitted-tail-lives-only-on-the-live-stream.md)).

- **Lash keeps no uncommitted tail.** An `Immediate` stop backtracks to the
  last checkpoint, so the next turn's context is that checkpoint. An
  `AfterStep` stop waits for its step's checkpoint and leaves no tail.
- **The tail is the deltas after the last marker.** Take the prose and
  reasoning deltas after the turn's last `CheckpointRecorded`, or after
  `TurnStarted` if none was recorded, and remove those whose correlation IDs
  a later `ModelAttemptReset` retracts. Resubmitting that text is your
  choice, as ordinary input. Tool arguments are not streamed, and a tool's
  output arrives only with `ToolCallCompleted`.
- **Keep up, or the tail is gone for good.** Follow the stream within the live
  replay window: 2,048 events or 120 seconds per session by default, on the
  replay of the process the turn runs in. A replay gap, a follower that lags
  its subscription and jumps to the head, or a turn that runs on another
  process without a shared live replay store loses the tail, and nothing can
  recover it. A slow sink loses nothing: deltas that queue more than 100
  events behind it merge into the queued delta of the same block, including
  across a cancellation.
- **`Stopped` is published only after its commit.** A stopped turn's terminal
  (the `Done` a cancel records, an `Error`, `TurnOutcome { Stopped }` and the
  final `Done`) reaches you only once the turn's commit is accepted. A commit
  that fails publishes none of it.

## Internal phase instrumentation

Workspace measurement and fault-injection tools use the hidden turn-phase
probe. Its vocabulary is explicitly unstable. See the
[turn-phase instrumentation contract](architecture/turn-phase-probe.md) for
callback, registration and naming rules.
