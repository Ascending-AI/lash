# Turn cancellation is a first-party work-driver primitive

## Context

A foreground turn needs an exact, durable stop request without acquiring
Runtime Process identity. `TurnAddress { session_id, turn_id }` names that
turn. These are routing identities; Lash applies no authentication or
authorization policy to them.

## Decision

`TurnWorkDriver::request_cancel` races normal completion through a reserved,
first-writer-wins keyed promise. The accepted request carries a request id,
optional opaque origin and optional reason. A normal completion seals the
gate, and later requests report `CompletionWonRace`. A second reserved
promise publishes terminal evidence after commit. Semantic session and turn
identity keeps the keys stable across owner loss.

The configured effect host owns these promises. The request receipt describes
addressing the gate; it does not prove that execution has stopped. The
running or replayed owner observes the gate, honours cooperative cancellation
and commits `TurnStop::Cancelled { evidence }`. Durable request rows are
intent and receipt projections, not a stop channel or a second arbitration
result. No waiter polls those rows to discover cancellation.

Closing the reserved cancel, escalation and terminal promises is a
crash-completable protocol. Before closure, the current drive fence authorizes
one non-overwritable operation containing the control binding, admitted scope,
keys, proposed base terminal and observed intent revision. An identical retry
adopts it; a different operation conflicts. A successor may finish its exact
promise resolutions. Final publication depends on the session-head CAS,
settled cancellation facts and the authorization, not the authorizing fence's
epoch. Activation repair validates its current drive fence and drains pending
closures before doing new work.

Promise resolution and SQL mutation are separate authority domains. The
retained authorization bridges a crash between them. Unknown or revoked
promise evidence fails typed and leaves the authorization pinned. Destructive
session or scope cleanup refuses while matching closure pins remain. An intent
CAS refusal retries the refreshed predicate without rerunning model calls or
restaging usage.

Opaque origins are host data. Internal token cancellation synthesizes an
`internal:<turn_id>` request identity; a raw cancellation token records no
invented origin. Engine invocation cancellation or kill is host recovery under ADR 0110 and
does not prove a Lash `Cancelled` result. Cooperative stop cannot guarantee
that detached tasks or non-cooperative external work have stopped.

## Cancel modes: immediate abort and after-step stop

`Immediate` feeds accepted evidence into the cooperative token and can unwind
provider, tool and durable-wait work. Its uncommitted tail returns to the last
checkpoint. Journaled start, after-model and after-step gates preserve the
observed decision on replay.

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
Session-wide guessing and invocation-id stop are rejected because they can
address the wrong turn or destroy an owner without a Lash outcome. Turning a
foreground turn into a Process is rejected because its lifecycle is
session-owned. Store-polled cancellation is rejected because it adds a second
coordination protocol beside the authoritative promise gate.

## Implementation

- [Addressed control and arbitration](../../crates/lash-core-execution/src/runtime/turn_control.rs).
- [Closure authorization contract](../../crates/lash-core-store/src/store/mod.rs) and [closure evidence](../../crates/lash-core-store/src/turn_control_vocabulary.rs).
- [Cancel modes and input policy](../../crates/lash-sansio/src/session_model/mod.rs).
- [Promise-owner settlement](../../crates/lash-core-execution/src/runtime/effect/executor/turn_control_authority.rs).
