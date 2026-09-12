# ADR 0093: Native reasoning retention is frame-scoped

## Status

Accepted.

## Context

Reasoning-capable providers accumulate opaque thinking state differently. Some
offer native sampling-context controls, while others expose no safe primitive.
A token budget or a model-name heuristic cannot translate those contracts
faithfully. Lash sessions also have a stronger semantic boundary than a
provider turn: the committed active agent frame.

## Decision

The active agent frame is the outer replay boundary. History remains durable,
append-only, and inspectable, but a request projects only the committed active
frame. Opening a frame record is not a reset; only a committed frame switch
changes the active projection. Provider replay state is additionally accepted
only for the exact provider, endpoint, and model route that minted it.

`ModelCapability.reasoning_retention` is the host-visible policy snapshot. Its
capability and selection are independent of reasoning effort and persist with
the session model. Providers validate the exact pair before network I/O. They
must not infer support from a model name or convert between provider units.

Native pruning is primary:

- OpenAI Responses and Codex map an explicitly supported selection to
  `reasoning.context`. The field is composed with `reasoning.effort`; OpenAI
  compaction is not enabled.
- Anthropic Messages maps an explicitly supported selection to native
  `clear_thinking_20251015` context management and its required beta header.
- A route whose protocol has no native retention primitive may advertise the
  client-side fallback. It retains a configured number of whole genuine-user
  segments. Only committed `TurnInput` messages (or direct API user messages)
  start segments. Tool results, runtime feedback, and RLM observations do not.
  A cut therefore preserves complete tool and opaque replay relationships.

Native controls reduce model-visible sampling context, not HTTP request size.
The client fallback cannot bound one endless user turn. Hosts should initiate
an RLM handoff before a long task reaches that condition. Handoff uses the
existing terminal `continue_as` path: the host chooses the task, seed, timing,
and carry-forward material; core commits one new frame and permits one logical
task continuation. The old interpreter state is not carried into the new
frame.

## Consequences

The policy behaves identically in standard, RLM cell, RLM native-tool, remote,
and cold-reopened execution because those paths share the durable model
capability and explicit message-boundary marker. Unsupported choices fail
deterministically before transport.

Default retention policies remain omitted from serialized capabilities and
decode as provider-default. A missing message-boundary marker decodes as
`false`: absence is never evidence of genuine user input and therefore can
never introduce a client-side retention cut. The explicit `true` marker is
written only for committed `TurnInput` and direct API user messages.

The new fields advance the remote protocol from 59 to 60, session-head metadata
from 9 to 10, and session-node bodies from 13 to 14. The remote and session-head
fences refuse their immediate predecessors; the head v10 fence is the durable
refusal point for the model capability and projected message marker. The node
generation bump is nominal because the node-body fence is forward-only and an
`LlmMessage` is not stored in a durable node body. These generations follow the
admitted-effect-identity cutover on `main`; no silent old-generation migration
is provided.

This decision does not introduce compaction, overflow recovery, byte budgets,
a universal HTTP bound, a new continuation abstraction, durable rewrites, or
retained interpreter state.
