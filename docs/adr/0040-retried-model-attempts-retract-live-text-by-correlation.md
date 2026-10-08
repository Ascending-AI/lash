# Retried model attempts retract live text by correlation

## Context

A provider retry starts a fresh generation. Text and reasoning emitted by the
failed attempt are provisional, while the committed transcript contains the
accepted attempt. Observers need an explicit retraction to render that same
result without guessing from text.

## Decision

`LlmStreamEvent::AttemptReset` produces `TurnEvent::ModelAttemptReset` carrying
the exact prose and reasoning correlation ids emitted by the abandoned
attempt. An observer removes text for those correlations and leaves other
activity alone. Tracking starts fresh after every reset.

Every re-generation is a visible generation boundary, including a reset before
any visible output. Empty correlation lists mean no retraction; they never
mean retract-all.

A call re-sent after a takeover ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md)
§4) is a re-generation too, by another owner. The call's pin records where
the session's live replay stood before its first attempt streamed. The new
owner reads the call's earlier attempts back from the live replay after that
pinned cursor, and publishes one reset naming their prose and reasoning
before the re-sent attempt streams. The re-sent attempt streams under an
observation key of its own (`{call}:stream@{attempt}`), so the store's
redelivery deduplication never takes it for the abandoned attempt's. When the
replay from the pinned cursor gaps (retention dropped some of it, or the
session's continuity restarted), the abandoned text cannot all be named: the
new owner invalidates the session's continuity instead, and observers recover
through a gap (FIG-5366). Nothing in the window proves completeness on its
own: a turn marker can be republished by the resume itself after retention
dropped the original (FIG-5399).

The reset shares the session observation activity path with deltas. Retained
replay re-applies both; a cursor after the reset needs no old retraction. A gap
recovers from the committed Session Read View. The host supplies observation
retention policy. Retractions target append-only text; structured tool
activity retains its own identity and collection rules.

The runtime notifies assistant-stream plugins of `AttemptReset` before
accepting retry output. Plugins discard partial delimiters and captured cell
bodies, and the runtime resets provisional output, usage and evidence. RLM
stores per-iteration assistant content as protocol history paired with its
trajectory, while the final product transcript is committed once.

The local observation feed carries typed `ModelAttemptReset` values and their
correlation-id lists. A host projects them into its own transport DTOs and owns
that transport's compatibility policy
([ADR 0136](0136-hosts-own-their-wire-contracts.md)); the workbench projection is
`examples/agent-workbench/src/main_sections/routes/observation_envelope.rs`.

## Consequences

Live and reconnecting hosts can remove failed preview text without content
heuristics. Adding attempt identity to every delta is rejected because existing
correlations already identify renderable blocks. Identical-text deduplication
is rejected because legitimate repeated output cannot be distinguished from
a retry by content.

## Implementation

[Reset collection and emission](../../crates/lash-core/src/runtime/turn_driver/streaming/support.rs),
[a takeover re-send's reset](../../crates/lash-core/src/runtime/turn_driver/abandoned_stream.rs),
[plugin reset and accumulator lifecycle](../../crates/lash-core/src/runtime/turn_driver/streaming.rs)
and [observation vocabulary](../../crates/lash-core-execution/src/runtime/vocabulary.rs)
define the boundary.
