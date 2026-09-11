# Pending Turn Input Is Admission Evidence

## Status

Accepted.

## Context

User turn input has runtime-visible states that ordinary background work does not: it may be admitted for an active turn checkpoint, deferred to the next turn, claimed into a running turn, cancelled before claim, or left as terminal evidence after completion. Treating it as queued work blurs user-input ownership with background process wakes and session commands. Treating it as product editing state would also put product ordering rules into Lash core.

This deliberately borrows Flue's explicit admission evidence and terminal-state clarity without copying its submission-journal model, because Lash already has a separate durable effect-host boundary for in-flight turn recovery.

## Decision

Pending Turn Input is runtime admission evidence for submitted user `TurnInput`.

- It is not queued work. `QueuedWork` remains for non-user background ingress such as `SessionCommand` and `ProcessWake`.
- It is not product editing state. Product edits create new source keys, and hosts map product suffix concepts to pending-input ids or source keys before calling Lash.
- `source_key` is the immutable idempotency key for a submitted revision. Exact replay of the same source key and submitted ingress/input content returns the existing record. Reusing the source key with changed ingress or input content is a store conflict.
- Cancellation returns typed outcomes: cancelled, already claimed/accepted, already completed, already cancelled, or not found.
- Cancelled and completed rows remain as tombstones so idempotency and cancellation outcomes stay observable until host-scheduled `RuntimePersistence::vacuum()` prunes them. Pending-list APIs hide terminal rows. They return every open row through a separate read projection: an unheld or reclaimable row is pending, while a claim matching the currently live Session Execution Lease generation is held and carries that lease's exact expiry. Held is not a persisted `TurnInputState` and does not infer that the holder process is alive.
- Runtime suffix cancellation is same-session admission order only: the anchor row's `enqueue_seq` and all later pending-input records.
- Lash does not copy Flue's submission journal. In-flight turn recovery remains durable effect-host replay; pending-input records cover admission, claim, cancellation, and terminal evidence.

## Consequences

Stores update pending-input rows to `Cancelled` or `Completed` instead of deleting them. Claim paths only claim pending states. Bulk and suffix cancellation must be atomic per store call because each result is evidence about one admission record at one point in runtime order. `vacuum()` is the bounded retention surface for terminal admission evidence and reports the removed tombstone count.
