# RLM uses one provider-native code channel

## Status

Accepted. Supersedes ADR 0006 and the cell-channel and channel-selection parts
of ADRs 0005 and 0083 (FIG-7374).

## Context

RLM follows Prime Agent: the provider offers one code tool and the program calls
all other tools inside the persistent interpreter. Two transports duplicated
admission, prompt, streaming, history and fixture machinery.

## Decision

Every RLM session offers `execute_code({code})`. There is no channel setting.
One complete program runs per accepted response. Provider calls and their results
stay paired in history, with ordered reasoning items and replay signatures intact.
A semantic-only frame seed remains user context and cannot mint provider calls.

Natural termination ends on nonempty prose without a tool call. TerminalRequired
continues to require a declared finish tool called inside the program. Execution,
charge prices, checkpoint, restore and replay semantics are unchanged.

Remove the channel field from configuration and recorded session namespaces in
place as part of the unsealed 1.0 baseline. There is no compatibility reader or
migration for the removed cell transport.

## Consequences

Scripted providers must author native calls and results. Text-only models cannot
run RLM programs. Adding a text transport after 1.0 would be an additive design
decision; there is no dormant parser or option reserved for it.
