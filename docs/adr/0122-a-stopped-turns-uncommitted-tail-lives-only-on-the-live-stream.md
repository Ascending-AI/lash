# 0122: A stopped turn's uncommitted tail lives only on the live stream

## Status

Accepted.

## Context

A turn checkpoints at protocol iteration boundaries. A host can want the
streamed tail after the last checkpoint when an Immediate stop backtracks.
The committed graph and the live observation stream have different retention
contracts; the host needs an explicit marker between their contents.

The stop modes follow [ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md),
and bounded live replay follows [ADR 0002](0002-session-observation-uses-cursors-and-bounded-live-replay.md).

## Decision

1. **Lash keeps no uncommitted tail.** An `Immediate` stop backtracks to the
   last checkpoint, and an `AfterStep` stop waits for the step's checkpoint,
   so its completed step is committed (ADR 0039). After an `Immediate` stop
   the next turn's context is the last checkpoint. Nothing a stopped turn streamed
   after it enters the graph, the turn's outcome, history or the store.

2. **The live stream is the contract.** Every host-visible observation is
   published in order on its lane. Stream deltas arrive in frames (ADR 0002):
   a host that lags gets later deltas of a block coalesced into the frame it
   has not taken yet, never dropped, including across a cancellation: a
   stopped turn delivers every delta it queued. The observer holds that
   backlog for as long as the host's sink takes to drain it; a sink that
   blocks delays the turn's `published()` barrier, until the backlog drains.

3. **The tail is found by the marker.** The tail of an `Immediate` stop is
   the prose and reasoning deltas after the turn's last
   `CheckpointRecorded { protocol_iteration }`, or after `TurnStarted` when
   no checkpoint was recorded, less the deltas a later `ModelAttemptReset`
   retracts. Resubmitting it is the host's choice, as ordinary input. Tool
   arguments are not streamed, and a tool's output arrives only with
   `ToolCallCompleted`.

4. **Keeping up is the host's job.** A host follows the session observation
   stream by cursor within the live replay window (ADR 0002: 2,048 events or
   120 seconds per session by default, both configurable), on the replay of
   the process the turn runs in. A `Gap`, a lagged follower that jumps to the
   head, a follower's pre-adoption buffer that evicts its oldest activity
   past 4,096, or a turn that runs on another process without a shared live
   replay store, loses the tail for good; so does a live publication failure,
   which lash only logs. Lash promises delivery of the observations it
   admitted to the stream. It does not promise provider events still queued
   when a cancellation wins the stream loop, nor observations a lost worker
   never published.

5. **A stop publishes after its commit.** A stopped turn's terminal is
   recorded when the turn stops, held on the observer, and published only
   after its commit is accepted: the `Done` a recorded cancel writes, the
   machine's `Error`, the `TurnOutcome { Stopped }` and the final `Done`. A
   commit that fails publishes none of it, so a host never sees `Stopped`
   for a turn that did not commit.

6. **Stops stay typed.** `TurnStop`, its cancellation evidence with mode and
   iteration, and `RunTerminalCause` (including `Refused`) carry why a turn
   or run stopped.

## Consequences

- A host that wants a stopped turn's tail collects it while it streams. A
  host that reconnects after the window, or follows a turn another process
  runs, cannot recover it; neither can lash.
- A lagging host sees a cancelled turn's result only after the backlog ahead
  of it. Framing joins consecutive deltas of one block up to a size cap;
  alternating blocks and non-delta events each keep their own entry, so the
  backlog is bounded by what the turn published, not by a fixed size.
- The publication law of the turn round crash matrix (a refused commit
  publishes nothing it held, at every cut) and the observer laws
  (a frame delivered whole before a cancellation's terminal, frames cut at
  every other event, hold and release, abandon) enforce decisions 2 and 5.
- `docs/observing-turns.md` states the contract for hosts.

## Implementation

- `crates/lash-core/src/runtime/turn_observer/framing.rs` frames each lane's
  deltas; `crates/lash-core/src/runtime/turn_observer.rs` holds, releases and
  abandons terminal observations.
- The phase runner (`crates/lash-core/src/runtime/durable/phases.rs`) calls
  `TurnDrive::committed` once `turn.commit` is acknowledged; the production
  drive (`crates/lash-core/src/runtime/turn_driver/durable_drive.rs`)
  releases its held terminals there and drains its publisher. A drive whose
  commit was refused is dropped without it, which abandons them.
- `crates/lash-core/src/runtime/observation/replay.rs:22` defines the
  configurable default event and time window.
- `crates/lash/src/send/follow.rs:52` bounds the pre-adoption buffer.
- `crates/lash-durable-test/tests/turn_round_crash_matrix.rs` pins
  publication after an acknowledged commit only, at every cut;
  `crates/lash-core/src/runtime/turn_observer/tests.rs` pins framing, hold,
  release and abandon.

A separate durable tail would need another write and retention contract for
output the host already receives live. This design keeps the last committed
checkpoint as the next turn's context and lets the host choose whether to
resubmit its live tail.
