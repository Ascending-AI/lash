# 0084: Separate initial instructions from positional runtime feedback

Status: Accepted (FIG-2505, 2026-09-08)

## Context

The configured prompt and runtime messages previously shared `LlmRole::System`.
Provider adapters guessed which messages were initial instructions: Responses and
Google hoisted all System messages, while Anthropic hoisted the first. An
output-limit retry could therefore move before the partial answer that caused it.

## Decision

`LlmRequest.instructions: Option<Arc<str>>` carries initial instructions.
Projectors never insert them into `messages`. Every System message in the
conversation is runtime feedback. Direct requests expose the same field and
refuse a leading System message with `DirectLlmError::LeadingSystemMessage`,
whose diagnostic names `instructions`; they never infer the caller's intent.

Runtime feedback is emitted at its conversation position on every provider. It uses the provider's native instruction form when that form is legal at that position and the `<runtime_feedback>` tagged user form otherwise. It is never moved across conversation turns or folded into the initial instructions.

| Provider | Initial instructions | Runtime feedback |
| --- | --- | --- |
| Responses and Codex | `instructions` | Ordered input item with host instruction role |
| Chat Completions | Leading message with host instruction role | Message with that role at position |
| Anthropic | Top-level `system` | Native System at a legal slot when enabled; tagged user block otherwise |
| Gemini and Code Assist | `systemInstruction` in the Gemini request | Tagged user part at position |

`None` omits the initial-instruction element except on Codex, which retains its
existing always-present `instructions: ""` string. Projectors trim configured
prompts and map whitespace-only prompts to `None`; explicitly supplied request
instructions and feedback text are preserved byte for byte.
The fallback wraps the complete feedback text in one
`<runtime_feedback>…</runtime_feedback>` pair. It carries user-level authority,
not native instruction authority. Adjacent user blocks coalesce in Anthropic
and Google, retaining each tagged block and its order. Consecutive native
Anthropic System messages coalesce into one section. Empty feedback and feedback
with attachments use tagged fallback; attachments remain separate user blocks.
On Responses, Codex, and Chat, only an attachment-bearing feedback message
downgrades to a user item/message containing the complete tagged text and its
attachments. Other feedback retains the host instruction role. If an attachment
cannot be projected legally, the provider returns a typed validation error naming
the original message index; it must never silently discard the attachment.

The single intra-message placement rule is: tool results precede feedback in the
same user turn. Feedback injected between an assistant tool call and its result,
or between consecutive result messages, remains before the next assistant turn.
Anthropic coalesces that turn as `[tool_result…, tagged feedback…]`; this slot
uses fallback even with native System enabled. Gemini and Code Assist likewise
keep function-response parts first in the coalesced user content. Responses,
Codex, and Chat emit the turn's tool-output items/messages first, followed by its
feedback items/messages. Result order and feedback order are each preserved;
this does not move feedback to a different conversation turn.

The host supplies `ModelCapability.instruction_role` (`System` by default,
`Developer` when specified) and `native_mid_conversation_system` (false by
default). No model-name heuristic selects either field. The external lash-cli
catalog adopts these fields separately.

Anthropic native sections must follow a user turn or an assistant turn ending
in a server tool result, and precede an assistant turn or end the array.
Leading feedback, retry-after-partial, user–System–user, and feedback after an
ordinary assistant use fallback at their original positions. A single request
can contain both forms. Lash has no server-tool-result block representation;
its client `ToolResult` does not qualify for the assistant exception. The
ordinary native feature needs no beta header. Tool changes and turn-scoped
clearing are separate features and are not enabled here. See the
[provider placement rules](https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages).

Pi keeps `context.systemPrompt` separate from conversation messages; its
[Anthropic converter](https://github.com/badlogic/pi-mono/blob/main/packages/ai/src/api/anthropic-messages.ts)
uses that field for the top-level prompt. The FIG-2505 comparison also discussed
system-update fallback. We adopt the field separation, but deliberately do not
copy model-name heuristics or the comparison's `<system_update>` spelling.
The upstream converter inspected for this decision does not contain that tag;
it is not the wire oracle for Lash's positional feedback. Lash owns the runtime
feedback vocabulary and the host owns capability selection.

## Consequences

Remote request DTOs, their conversions, and durable effect request specs carry
the explicit field; resuming a call cannot drop its instructions. Remote
protocol window 55 replaces 54. Composition tracing hashes the
instructions field, its presence and host instruction role, plus tools under
`lash-model-facing-composition/v3`; the prompt snapshot remains text plus tools. Runtime feedback remains conversation evidence.
Cache-prefix regression comparisons retain feedback rather than filtering
System messages out. Native Anthropic projection can change an already-sent
prefix when later conversation makes the same slot illegal: `[User, System]`
is native, but `[User, System, User]` is tagged and coalesced. This cache cost is
an unavoidable consequence of per-request legality and absolute position. A
whole-request transition witness pins it; truncating both requests before
serialization would hide it. The default-off capability avoids this native
form transition. Initial-instruction and explicit conversation cache markers
remain separate, including on native or tagged runtime feedback.

The version gate also requires session-node body version 11: capabilities are
serialized inside `ModelSpec` in session policy. Session-head metadata separately
advances from 6 to 7 because its config carries the same capability. The strict
head decoder refuses v6; the node-body decoder remains a forward-only fence.
Process environment identities advance from `process-env:v4` / `lash-process-env/v4`
to v5 because their policy bytes also carry the capabilities. The remote reference
parser refuses v4, and local loading recomputes the v5 identity and refuses older
or mismatched references. Existing process environments must be recreated;
there is no cross-family translation. The inline identity corpus now includes
non-default capabilities, and explicitly pins the new JSON and hash bytes.

These logical-format changes resolve the lane's no-store-schema instruction
against the actual persisted reach of host capabilities. Physical store
`SCHEMA_VERSION` values do not change. These logical fences make the committed
current-reader fixtures stale, so fixture version 53 replaces 52 and the repository
generators refresh the PostgreSQL and SQLite durable-read corpus. The separate
PostgreSQL component-refusal fixture refreshes only its enclosing catalog/head;
its old checkpoint remains the refusal witness. This is the necessary exception
to the original no-fixture instruction under the resume amendment's contradiction
rule. No compatibility shim or false identifier-only baseline is added. Old
families remain reserved.

Deferred-tool activation and tool-surface deltas (FIG-142) are outside this
decision.
