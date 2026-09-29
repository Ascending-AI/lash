# Observing turns

Turn activities arrive on the session observation stream in cursor order.
Keep the cursor and use the bounded live replay described in
[ADR 0002](adr/0002-session-observation-uses-cursors-and-bounded-live-replay.md)
when reconnecting. A replay gap means the missing activities cannot be
reconstructed from the durable session view.

`CheckpointRecorded { protocol_iteration }` marks an accepted checkpoint on the
turn's lane. Every non-retracted delta before the marker belongs to the
checkpointed state. To find the streamed tail of an `Immediate` stopped turn,
take the deltas after its last `CheckpointRecorded`, or after `TurnStarted` if
none was recorded. Remove prose and reasoning deltas whose correlation IDs a
later `ModelAttemptReset` retracts. A cancelled tool batch can itself be
checkpointed, so the marker decides the cut.

The marker is live-only. Like every other turn activity, it is absent from
durable history and may be lost when the live replay window expires. Hosts that
need the tail after a stop must collect the activities while they are
available. A host that lags enough for deltas to be discarded cannot recover
the missing text from the marker.
