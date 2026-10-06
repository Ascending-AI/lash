# Turn cancellation is a first-party work-driver primitive

## Context

A foreground turn needs an exact, durable stop request without acquiring
Runtime Process identity. `TurnAddress { session_id, turn_id }` names that
turn. These are routing identities; Lash applies no authentication or
authorization policy to them.

## Decision

`TurnWorkDriver::request_cancel` writes a cancel request row to the session
actor's mailbox and wakes the actor, including a parked one
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §3 and
§11). The accepted request carries a request id, optional opaque origin and
optional reason. It races normal completion at one fenced point: the turn
commit and the cancellation decision are both owner transactions of the
session actor, so exactly one commits first. After a turn commit wins, later
requests report `CompletionWonRace`. Semantic session and turn identity keeps
the request address stable across owner loss.

The request receipt describes addressing the turn; it does not prove that
execution has stopped. The owner, the current one or the one that claims the
actor after it, reads the request with its mail, honours cooperative
cancellation and commits `TurnStop::Cancelled { evidence }`. The mailbox row
is the stop channel and the turn commit is the arbitration result; the owner
learns of a request through its mail flag, and nothing polls the rows.

The cancellation decision, its terminal evidence, the settled cancellation
facts and the session-head compare-and-set commit in the turn commit's one
transaction (ADR 0132 §4). No crash separates them, so no closure
authorization bridges two authority domains. An intent CAS refusal retries the
refreshed predicate without re-sending model calls.

Opaque origins are host data. Internal token cancellation synthesizes an
`internal:<turn_id>` request identity; a raw cancellation token records no
invented origin. Killing a node is host recovery: another node claims the
actor after the reap (ADR 0132 §3), and the kill does not prove a Lash
`Cancelled` result. Cooperative stop cannot guarantee that detached tasks or
non-cooperative external work have stopped.

## Cancel modes: immediate abort and after-step stop

`Immediate` feeds accepted evidence into the cooperative token and can unwind
provider, tool and durable-wait work. Its uncommitted tail returns to the last
checkpoint. The start, after-model and after-step gates commit the decision
they observe with the phase they end, so a resumed turn keeps it.

`AfterStep` lets the current protocol iteration finish, including its tool
calls and accepted checkpoint, then honours the request at the step boundary.
It does not fire the cooperative token or backtrack the completed step. Both
modes refuse a turn at its start gate when the request is already present.
A durable wait can finish for `AfterStep`; a later timing escalation can
interrupt it.

The base gate permanently owns undelivered-input policy. An `Immediate`
request may escalate an accepted `AfterStep` request through
`TurnCancelEscalation` only with the same policy. A different policy reports
`PolicyConflict` without changing the accepted intent or escalation. Weaker
or equal requests report `AlreadyRequested`. The effective evidence uses the
escalating request's identity, origin, reason and timing, but retains the
base winner's undelivered disposition. Hosts own escalation timers.

The disposition applies to host-authored items addressed to the cancelled
turn that it never delivers. Deferred items retain their immutable addressing;
an ended turn's items are next-turn items by rule. Held process wakes defer
with their position preserved. A cancellation cannot drop a wake without an
explicit host withdrawal. Affected items carry closed reasons (ADR 0101).

An accepted checkpoint publishes `CheckpointRecorded` after its included
activity and before an after-step stop at that boundary. It is live
observation, not durable history. Hosts use it with ADR 0040's correlation
retractions to identify an immediate stop's uncommitted tail.

The remote cancellation DTOs carry the same request mode and terminal
checkpoint evidence. Their conversions preserve `Immediate`, `AfterStep`,
`honoured_after_step`, and the distinct `Escalated` receipt outcome through
JSON transport. These shapes change in place under the pre-1.0 version
freeze (FIG-3846).

## Terminal product-event ownership

The turn execution publisher owns the observer-facing terminal event. A stop
handler attaches to terminal evidence and returns a receipt; repeated requests
do not publish another terminal event. Cancellation traces use the recorded
winner's request id.

## Consequences

A host offering stop-all retains exact turn ids and submits exact requests.
Session-wide guessing and node-kill stop are rejected because they can
address the wrong turn or destroy an owner without a Lash outcome. Turning a
foreground turn into a Process is rejected because its lifecycle is
session-owned. A cancel channel outside the session actor's mailbox is rejected because it
adds a second coordination protocol beside the fenced commit.

## Implementation

- [Addressed control and arbitration](../../crates/lash-core-execution/src/runtime/turn_control.rs).
- [Cancellation evidence](../../crates/lash-core-store/src/turn_control_vocabulary.rs).
- [Cancel modes and input policy](../../crates/lash-sansio/src/session_model/mod.rs).
