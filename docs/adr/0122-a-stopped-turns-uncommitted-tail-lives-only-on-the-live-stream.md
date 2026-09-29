# 0122: A stopped turn's uncommitted tail lives only on the live stream

## Status

Accepted 2026-09-29 (FIG-4113). Supersedes
[ADR 0114](0114-a-stopped-turns-partial-output-is-sealed-durably-and-returned-to-the-host.md)
and keeps its publish-after-commit decision (its §4.3, step 5). It builds on
the stop semantics of
[ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md),
the bounded live replay of
[ADR 0002](0002-session-observation-uses-cursors-and-bounded-live-replay.md)
and the `CheckpointRecorded` marker that FIG-4114 added to the turn
activity lane.

Sam's ruling on audit G14 (2026-09-29) binds this decision:

- Checkpoints are frequent. A host that wants output streamed after the last
  checkpoint consumes the live stream and may resubmit it as ordinary input.
  Lash keeps no durable record of it and never feeds it back.
- The capture of ADR 0114 is removed, with its follow-ups FIG-4069 and
  FIG-4071, and so is the observer's discard of lagging deltas on a
  cancellation.

## Context

ADR 0114 sealed a stopped turn's uncommitted output into a durable, typed
partial. It needed a fourth store segment with four tables on each backend,
a capture writer on every model call and tool attempt that persisted each
batch before publishing it, a seal fenced into the stop's commit, a tool
progress sink, provider tool-input events, a lost-root seal and host reads.
The tail it kept is short, because a turn checkpoints at every protocol
iteration, and a host already receives every byte of it live. After FIG-4114
a host can also find it exactly: it is everything after the last
`CheckpointRecorded` marker.

## Decision

1. **Lash keeps no uncommitted tail.** An `Immediate` stop backtracks to the
   last checkpoint, and an `AfterStep` stop waits for the step's checkpoint,
   so nothing is uncommitted (ADR 0039). After an `Immediate` stop the next
   turn's context is the last checkpoint. Nothing a stopped turn streamed
   after it enters the graph, `AssistantOutput`, history or the store.

2. **The live stream is the contract.** Every host-visible observation is
   published in order on its lane. A host that lags more than 100 queued
   events behind gets later deltas of the same block merged into the queued
   one, never dropped, including across a cancellation: a stopped turn
   delivers every delta it queued. The observer holds that backlog for as
   long as the host's sink takes to drain it; a sink that blocks delays the
   turn's `published()` barrier, as it always did.

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
   iteration, and `RootTerminalCause` (including `SubstrateLost {
   cancelled_by }` and `Refused`) carry why a turn or root stopped. ADR
   0114's `StopReason` projection is deleted.

## What is deleted

With no shims, and with shapes changed in place under the version freeze
(the SQLite 99 and PostgreSQL 141 DDL versions stay):

- the `TurnCaptureStore` segment, its in-memory, SQLite and PostgreSQL
  implementations and reducer, and its four tables on each backend;
- the model-call and tool-attempt capture writers, the capture base and its
  advance, the seal, `RuntimeCommit.stopped_partial` and the
  capture-watermark on recorded outcomes;
- the progress sink (`AttemptContext::progress` and its traits);
- the provider `ToolInputStart`, `ToolInputDelta` and `ToolInputEnd` events;
- `StoppedPartial`, `StoppedPartialAvailable`, `ToolOutputProgress` and
  every event, protocol and host mirror of them, including the host reads
  and the workbench panel;
- the lost-root seal and its announcement;
- the capture store errors (with their arms in `StoreError::is_transient`,
  which attachment retries still use) and
  `RuntimeErrorCode::TransientCaptureWrite`;
- the observer's discard of lagging deltas on a cancellation.

The store once more has the ten segments ADR 0112 names.

## Consequences

- A host that wants a stopped turn's tail collects it while it streams. A
  host that reconnects after the window, or follows a turn another process
  runs, cannot recover it; neither can lash.
- A lagging host sees a cancelled turn's result only after the backlog ahead
  of it. Merging joins consecutive deltas of one block; alternating blocks
  and non-delta events each keep their own entry, so the backlog is bounded
  by what the turn published, not by a fixed size.
- The laws `runtime::stop_publication` (a provider failure's error, an
  `Immediate` cancel's terminal and a refused commit) and the observer laws
  (merge across a cancellation, an alternating-block backlog, hold and
  release, abandon) enforce decisions 2 and 5.
- `docs/observing-turns.md` states the contract for hosts.
