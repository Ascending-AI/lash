# 0120: Tool presentation is a recorded, composable step

## Status

Accepted.

## Context

A settled tool output and the model-facing view of that output are different
facts. Several plugins can need to shape that view, and replay must recover
the same presentation even when a successor host has different local timing.
Retained full output needs storage shared by the runtime's attachment ports.

## Decision

### A. The presenter runs before ordered optional steps

A plugin registers a `ToolPresentationPresenter` through
`registrar.tool_results().presenter(..)`. Standard protocol supplies the
required renderer. Optional `ToolPresentationStep`s register through
`presentation_step(..)` and fold in registration order. Every step receives
the prior `ModelToolReturn`, the settlement and its projection context.
A retryable optional-step error aborts the uncommitted presentation derivation
and is never model-visible text. Other optional-step errors become fallback
text, and the chain continues.
A required-presenter error fails presentation.

The standard `ToolOutputRenderer` uses the renderer id and resolved per-tool
parameters recorded with the command. A missing or mismatched renderer
refuses presentation. The renderer handles authored output and tool values,
limits characters and lines, keeps head and tail, and records visible ranges.
Every cut retains full text, including failure output.

Sources: `crates/lash-core-execution/src/plugin/session_obj.rs:700`,
`crates/lash-core-execution/src/plugin/registrar.rs:229`, and
`crates/lash-protocol-standard/src/render.rs:419`.

### B. Presentation is a journaled effect

Scalar completion and group-child completion execute
`RuntimeEffectCommand::PresentToolResult` through their scoped controller,
under `{call_id}:present`. The command contains the call and tool identities,
name, recorded render configuration, arguments and settled output. Duration
is a local-executor observation, not part of the command identity.

The outcome is `ToolPresentation { version, model_return, artifacts,
retention }`. Its reader validates `TOOL_PRESENTATION_VERSION` and refuses
unknown fields. A recorded presentation replays without running its presenter
or optional steps. The settlement uses its recorded model return; realized
intent outcomes supply the call's model addenda.

A journal or envelope failure is not fallback model text. It fails the scalar
completion or refuses the group child. Presentation-step fallback applies to
optional plugin failures, not to failure of the effect that records them.

Sources: `crates/lash-core-execution/src/session/tool_execution.rs:653`,
`crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs:1643`, and
`crates/lash-core-execution/src/runtime/effect/tool_presentation.rs:38`.

### C. Retention uses content-addressed attachments

`ToolPresentationInput::context.artifacts.retain_text(label, text)` stores
full text through the runtime's `RuntimeAttachmentStore` and records the
`AttachmentRef` in the presentation's artifacts list. Recorded replay puts
nothing. A crash after a put and before journaling the outcome can repeat the
put and the chain. Content addressing converges on the same blob.

After all steps and attachment-materialization notices, the boundary measures
the folded text against `OutputRetentionPolicy`. An oversized return becomes
one `ModelToolReturnPart::Retained` block containing a bounded witness and the
exact text's attachment reference. The outcome records the policy, so replay
serves the same retention decision after a threshold change.

A required retention failure anywhere in the chain carries its typed
attachment-store cause, even when a step catches the put error and returns
text. `AttachmentStoreError::is_retryable()` is the retry authority:
`OutputRetentionFailed` retries the uncommitted derivation only for a
transient cause. A permanent refusal records `OutputRetentionRefused` and
ends presentation. Byte limits and ended referrers never retry.

RLM cell prints and final values follow the same policy in the journaled
`{cell}:outputs` value: history carries `OutputValue::Retained` for oversized
JSON, while the host receives the full final value. Referrer acquisition and
boundary commits follow [ADR 0124](0124-attachments-are-kept-alive-only-by-their-referrers.md).
A session turn holds its puts through `Execution`; a process holds its puts
through `ProcessRecord`. A session boundary acquires the retained ids it names.

Sources: `crates/lash-core-execution/src/plugin/session_obj.rs:737`,
`crates/lash-core-execution/src/runtime/effect/tool_presentation.rs:106`, and
`crates/lash-protocol-rlm/src/executor/mod.rs:236`.

## Consequences

Several plugins can compose the model-facing view, but their order matters.
A recorded outcome fixes that view across replay. Content-addressed retention
survives a worker and tolerates repeated writes in the unrecorded window.
A worker-local spill file cannot provide either guarantee. An unrecorded
presentation hook would let replay change what the model sees.

The pre-1.0 freeze changes shapes in place. The reader still validates the
presentation format it declares; [ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
governs the 1.0 upgrade contract.

## Links

- [ADR 0099 §6](0099-tool-children-of-effect-groups-are-live-closing-settled.md)
  defines the group-child presentation boundary.
- `crates/lash-core-execution/src/runtime/effect/tool_settlement.rs` defines
  the settlement presentation and incorporation share.
- `crates/lash-restate-test/tests/crash_windows.rs` exercises the crash between
  the retained put and the journaled presentation on the server double and
  live Restate.
