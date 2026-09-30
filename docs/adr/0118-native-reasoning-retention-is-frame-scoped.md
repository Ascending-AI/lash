# ADR 0118: Native reasoning retention is frame-scoped

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

The durable model snapshot carries the selected policy across reopen and
remote execution. The pre-1.0 version freeze changes shapes in place;
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md) governs
upgrade read contracts.

This decision does not introduce compaction, overflow recovery, byte budgets,
a universal HTTP bound, a new continuation abstraction, durable rewrites, or
retained interpreter state.

## Implementation

- `crates/lash-core-store/src/session_state.rs:1115` refreshes the active
  frame projection; `crates/lash-core/src/runtime/turn_boundary.rs:542`
  records the outcome frame switch.
- `crates/lash-sansio/src/llm/capability.rs:43` defines the independent
  retention policy; `:350` validates the exact capability and selection.
- `crates/lash-sansio/src/llm/types.rs:1031` cuts only at explicit user
  segment markers, refuses orphaned tool results, and strips foreign replay
  state; `:443` makes an absent marker false.
- `crates/lash-sansio/src/session_model/message.rs:1628` marks committed
  turn input during message projection.
- `crates/lash-provider-openai/src/responses.rs:117` and
  `crates/lash-provider-openai/src/codex.rs:339` emit native context control.
- `crates/lash-provider-anthropic/src/request.rs:585` emits native thinking
  retention.

A token budget measures a different quantity from a provider's thinking-turn
or context policy. Guessing from a model name cannot establish route support.
Whole user segments keep call/result relationships intact; removing isolated
reasoning blocks cannot provide that guarantee.
