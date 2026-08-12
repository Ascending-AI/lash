# Retried model attempts retract live text by correlation

Provider retries restart an LLM request from scratch. Prose and reasoning emitted before a
retry are therefore superseded preview activity, while the runtime's reset stream accumulator
already ensures that only the successful attempt reaches the committed transcript. Publishing
the failed attempt's deltas without publishing that supersession made the host-facing stream
disagree with runtime truth and caused downstream chat surfaces to render duplicated narration.

We decided that `LlmStreamEvent::AttemptReset` emits a replayable
`TurnEvent::ModelAttemptReset`. The event carries the exact correlation ids of
`AssistantProseDelta` and `ReasoningDelta` activities emitted by the superseded provider attempt.
A compliant observer removes text for those correlations and leaves every other activity alone.
Each attempt starts fresh correlation tracking, so any number of consecutive resets applies the
same rule without comparing text or inferring retry boundaries.

FIG-1275 further establishes every provider re-generation as a host-visible generation boundary,
including one that occurs before the superseded attempt emits visible output. Such a reset has
empty prose and reasoning correlation lists. Observers retain it as boundary evidence, perform no
retraction, and must never interpret an empty list as "retract all." Retraction remains strictly
correlation-based.

The reset is appended to the same bounded Live Replay log as the deltas. A live observer sees
failed deltas followed by their reset. A late observer whose cursor precedes the failed attempt
replays both and reaches the same state; one whose cursor follows the reset sees neither
superseded state nor an unexplained retraction. A replay gap continues to recover from the
committed Session Read View. The reset targets only prose and reasoning because those are the
in-flight append-only text surfaces; tool activity retains its existing structured collection and
deduplication semantics.

`AttemptReset` is also an assistant-stream lifecycle boundary. The runtime notifies stream
plugins before it accepts retry output, so protocol segmenters discard partial delimiters and
captured cell bodies from the abandoned attempt along with the runtime accumulator. Otherwise a
plugin can reconstruct a contaminated final response even when the runtime's text accumulator is
clean.

RLM keeps per-iteration assistant reasoning and prose as protocol-owned `RlmAssistantContent`
events paired with trajectory entries, not as conversation messages. The RLM history projector
still folds that content into later provider requests, while the host commits the final product
transcript exactly once. This separation prevents protocol bookkeeping and the host transcript
from becoming duplicate durable chat messages, including after reload.

The remote observation mirror adds `model_attempt_reset` with string correlation-id lists and
bumps `REMOTE_PROTOCOL_VERSION` from 11 to 12. Remote protocol validation requires an exact
version match, so older hosts are rejected at negotiation instead of being asked to deserialize
and silently ignore a semantic event they do not understand. In-workspace `TurnEvent` consumers
remain compiler-guarded by exhaustive matches.

We rejected keying every prose and reasoning delta by a new attempt identity and later marking
the attempt aborted. That model can represent the same truth, but it expands all hot-path delta
payloads, requires every observer to retain attempt lifecycle state, and duplicates the existing
correlation identity that already names the renderable blocks. We also reject identical-text
deduplication: legitimate repeated output is valid model output, and content heuristics cannot
distinguish it from a retry.
