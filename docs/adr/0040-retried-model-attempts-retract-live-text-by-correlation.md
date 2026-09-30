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

The remote mirror carries `model_attempt_reset` and its correlation-id lists.
Remote negotiation follows the protocol's supported version contract.

## Consequences

Live and reconnecting hosts can remove failed preview text without content
heuristics. Adding attempt identity to every delta is rejected because existing
correlations already identify renderable blocks. Identical-text deduplication
is rejected because legitimate repeated output cannot be distinguished from
a retry by content.

## Implementation

[Reset collection and emission](../../crates/lash-core/src/runtime/turn_driver/streaming/support.rs),
[plugin reset and accumulator lifecycle](../../crates/lash-core/src/runtime/turn_driver/streaming.rs)
and [observation vocabulary](../../crates/lash-core-execution/src/runtime/vocabulary.rs)
define the boundary.
