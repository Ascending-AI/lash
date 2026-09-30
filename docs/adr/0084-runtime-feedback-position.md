# 0084: Separate initial instructions from positional runtime feedback

## Context

Initial instructions configure a request. Runtime feedback responds to a
conversation event, such as an output-limit retry, and must remain beside the
conversation that causes it. Hoisting both into one initial prompt loses that
position.

## Decision

`LlmRequest.instructions` carries initial instructions separately from
`messages`. Projectors trim configured prompts and map whitespace-only prompts
to `None`. Explicit request instructions and feedback retain their text bytes.
Direct calls expose the same field and refuse a leading System message with
`DirectLlmError::LeadingSystemMessage` instead of inferring caller intent.

Every conversation System message is positional runtime feedback. Adapters use
native instruction form where legal and the tagged user form otherwise. They
do not fold feedback into initial instructions or move it across turns.

| Provider | Initial instructions | Runtime feedback |
| --- | --- | --- |
| Responses and Codex | `instructions` | Ordered input item with host instruction role |
| Chat Completions | Leading message with host instruction role | Message with that role at position |
| Anthropic | Top-level `system` | Legal native System slot when enabled; tagged user block otherwise |
| Gemini and Code Assist | `systemInstruction` | Tagged user part at position |

Absent instructions omit the initial field, except that Codex emits its required
empty instruction string. Tagged fallback wraps the complete text in one
`<runtime_feedback>...</runtime_feedback>` pair and carries user-level authority.
Attachments remain separate user blocks. An unencodable attachment produces
a typed validation error naming its original message index.

The host supplies `ModelCapability.instruction_role`, System by default or
Developer explicitly, and `native_mid_conversation_system`, false by default.
No model-name heuristic selects either setting.

### Placement and coalescing

Anthropic native feedback requires text-only, nonempty content, a legal preceding
user/System section, and a following assistant turn or end of array. Leading
feedback, feedback after an ordinary assistant response, user-System-user,
empty feedback, and attachment-bearing feedback use fallback. Consecutive native
sections coalesce. Adjacent user blocks coalesce in Anthropic and Google while
retaining tagged feedback blocks and their order.

OpenAI wires retain host instruction authority for text-only feedback. Feedback
with attachments becomes a user item or message containing complete tagged
text and the attachments.

Within one tool-result user turn, tool results precede feedback. Their respective
orders remain stable. Anthropic and Google reorder within the coalesced user
content; OpenAI wires place separate tool-output items before feedback items.
This rule does not move feedback into a different conversation turn. A client
tool result cannot authorize Anthropic's server-tool-result exception.

## Alternatives considered

Inferring initial instructions from message roles lets an adapter hoist a retry
before the partial answer it concerns. Native instruction authority at every
position is unavailable on some wires. Explicit instructions and tagged positional
fallback preserve a usable contract without pretending the fallback has native
authority. Model-name capability heuristics cannot describe a host's route.

## Consequences

Remote and durable request carriers preserve the explicit instruction field.
Composition tracing includes instruction presence, text, and the host instruction
role. Runtime feedback remains conversation evidence and participates in cache
prefix comparison.

Native Anthropic legality can change with later conversation. A trailing
`[User, System]` section can be native, while `[User, System, User]` requires
tagged fallback and coalescing. That can change a serialized cache prefix; the
default-off capability avoids this native-form transition. Cache markers on
initial instructions and conversation feedback remain separate.

Logical versions and process-environment identities follow their owning format
guards in ADRs 0106 and 0115. This placement contract does not define a separate
physical-schema migration rule.

## Code references

- `crates/lash-core-execution/src/direct.rs:331-335` refuses leading System input.
- `crates/lash-provider-openai/src/chat.rs:117-123,203-210` separates instructions and ordered feedback.
- `crates/lash-provider-openai/src/responses_shared/input.rs:90-156` preserves attachments and tool-output ordering.
- `crates/lash-provider-anthropic/src/request.rs:173-221,295-311` checks native legality and coalesces results.
- `crates/lash-provider-google/src/request.rs:178,317-361` preserves tagged feedback and initial instructions.
- `crates/lash-sim/src/runtime_feedback.rs` supplies wire-placement and legality witnesses.
