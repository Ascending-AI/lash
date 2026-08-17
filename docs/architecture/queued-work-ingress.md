# ADR: Durable Work And Pending Turn-Input Ingress

## Status

Accepted.

## Context

Lash has several sources of deferred runtime work: user turn input, early turn injection, process wakes, and maintenance commands. Triggers are a separate runtime-level ingress, and timers are host-owned scheduling. User input has UX-visible ownership requirements that queued background work does not: an active steer may be accepted into the live turn, deferred exactly once after interrupt, cancelled before claim, or dispatched as the next idle turn. Background work must not accidentally look like foreground user work.

## Decision

Lash uses two durable ingress paths through `RuntimePersistence`:

- **Pending turn input** stores runtime admission evidence for model-visible user input. Each row carries a stable input id and optional immutable `source_key`, plus a `TurnInputIngress`: `ActiveTurn { turn_id, min_boundary }` for input typed while a turn is running, or `NextTurn` for true follow-up work. A pending input is in exactly one state at a time: pending-active, accepted/claimed, deferred-next-turn, cancelled, or completed. Cancelled and completed rows remain as tombstones for idempotent admission and cancellation outcomes until host-scheduled `RuntimePersistence::vacuum()` prunes them for the session its store handle is bound to; pending-list APIs hide terminal rows and live claims.
- **Queued work** stores non-user background work. It contains `SessionCommand` mutations and turn-producing `ProcessWake` delivery. User `TurnInput` is not a queued-work payload.

`source_key` is the idempotency key for one submitted revision. Exact replay of the same source key and submitted ingress/input content returns the existing record, even if that record is now cancelled or completed. Reusing the source key with changed ingress or input content is a store conflict. Product edits create new source keys; Lash core does not infer product edit ordering.

Cancellation returns typed admission outcomes: newly cancelled, already claimed/accepted, already completed, already cancelled, or not found. Bulk cancellation and suffix cancellation are atomic store calls. Suffix cancellation uses same-session runtime admission order (`enqueue_seq`) from an input id or source-key anchor through all later pending-input records; product suffix concepts must be mapped by the host to input ids or source keys before calling core.

The CLI never treats local prepared drafts as authoritative queue records. It reconciles draft presentation data with pending-turn-input receipts from core, then renders active pending steers separately from queued next-turn input.

Queued payloads have two runtime classes derived from their existing payload variants:

- `SessionCommand`: session mutations such as `RefreshToolCatalog`.
- `TurnWork`: turn-producing non-user work, currently `ProcessWake`.

Core owns scheduling across those classes. A queued drain consumes ready leading `SessionCommand` batches in enqueue order before it claims the next ready `TurnWork` group. Selected batch-id drains follow the same ordering rule, but their public contract is idempotent: they ensure that every requested ID is done. An ID with no remaining durable row—because a leading session-command drain consumed it, an earlier attempt settled it, or it never existed—is reported as `AlreadySatisfied` and is excluded from the turn-work claim. A row acquired by this drain is reported as `ClaimedNow`. This distinction is carried in `SelectedQueuedWorkDrainOutcome::satisfied`, so a host may observe it without treating already-satisfied IDs as errors.

Rows that remain present are still validated as one exact composition. A present row that cannot join the selected claim is refused as `UnclaimableTogether`; a partial interrupted predecessor composition is refused as `InterruptedBatchRequiresFullComposition`; and a present selection whose session execution lane is held is refused as `ExecutionLaneBusy`. An absent ID never widens the operation into an ordinary drain, and unrelated queued rows remain untouched. Session commands never become prompt text.

The CLI projects durable ingress into separate user-visible surfaces:

- active-turn steer preview for pending `ActiveTurn` input,
- next-turn pending-input preview for pending `NextTurn` input,
- process dock for durable runtime process handles,
- diagnostics/status details for background counts and failures.

Slash commands remain CLI host commands and are never queued as model work. Tool Catalog membership is configured outside chat slash commands.

## Consequences

- `LashSession::queued_work()` is an admin/introspection view of non-user queued work.
- `LashSession::pending_turn_inputs()` is the user-input view used for queue and active-steer previews.
- Idle dispatch claims durable pending `NextTurn` inputs. Active checkpoints claim only pending `ActiveTurn` inputs anchored to the live turn and admitted by the checkpoint boundary.
- An active checkpoint claim materializes each accepted input as an ordinary user message in the in-memory turn tail. Later model calls see that updated prompt immediately, but no progress boundary mutates the session store; the final turn commit persists the complete message and protocol-event tail exactly once.
- Active inputs that miss the turn's final checkpoint are atomically deferred to `NextTurn` during terminal commit, including normal completion as well as interruption, and become the next turn's ordinary user input.
- In-flight turn recovery remains durable effect-host replay. Lash does not keep a submission journal for pending turn input; pending-input records cover admission, claim, cancellation, and terminal evidence.
- `RuntimePersistence::vacuum()` is the retention boundary for terminal pending-input evidence and reports how many pending-input tombstones it physically removed. It is session-scoped on every backend: it prunes only the session its store handle is bound to, and a handle with no session binding returns `StoreError::SessionNotBound` rather than sweeping the catalog.
- Process wakes and session commands never appear as queued user input.
- Triggers and timers never appear as queued work at all; a matched trigger occurrence may start a process, and only that process's wake enters queued work at a safe turn boundary.
- Background process work does not promote the foreground footer state to `Working`.
