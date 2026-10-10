# 0120: Tool presentation is a recorded, composable step

## Status

Accepted.

## Context

A settled tool output and the model-facing view of that output are different
facts. Several plugins can need to shape that view, and resume must recover
the same presentation even when another node with different local timing claims the turn.
Retained full output needs storage shared by the runtime's attachment ports.

## Decision

### A. The presenter runs before ordered optional steps

A plugin registers a `ToolPresentationPresenter` through
`registrar.tool_results().presenter(..)`. Standard protocol supplies the
required renderer. Optional `ToolPresentationStep`s register through
`presentation_step(..)` and fold in recorded registration order.
The recorded `PresentationBinding` names the optional singleton presenter and
each step by callback key and owning plugin revision. `presenter: null` with
`steps: []` is an explicit empty plan. An owed derivation resolves every
identity before invoking any callback. Missing keys or revisions park with
`PluginExecutionRefusal`; installed substitutes never replace recorded work. Every step receives
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

Sources: `crates/lash-core-execution/src/plugin/session_obj/tools.rs`,
`crates/lash-core-execution/src/plugin/registrar.rs`, and
`crates/lash-protocol-standard/src/render.rs`.

### B. Presentation is a recorded phase

K1 admission records the complete `PresentationBinding` before execution. The
Run's V step consumes that binding after D has protected the final and every
lower protected rank has seated. Declarations finish before presentation;
presentation precedes incorporation. V retains only bytes distinct from the
canonical A/X material and records incorporation in the same boundary.

The presentation commits as its own phase of the Run
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §5). The
presentation reader validates its stored format. Resume loads the recorded
model return without resolving callbacks or running presenters and steps.
Realized intent outcomes supply the model addenda. Duration is an observation,
not identity. An unavailable callback needed for an owed presentation refuses
typed; a completed presentation does not consult the live registry.

A declared presentation refusal records the original result as fallback with its
typed `HookCause`. An infrastructure fault leaves V uncommitted, and it is recomputed from
committed state; a recorded-state or material refusal is never fallback model
text. Optional-step fallback remains the plugin-composition policy, not a
repair for a resume failure.

Sources: `crates/lash-core-execution/src/runtime/actor/round/lifecycle.rs`,
`crates/lash-core-execution/src/tool_dispatch/production/settlement.rs`
and `crates/lash-core-execution/src/runtime/effect/tool_presentation.rs`.

### C. Retention uses content-addressed attachments

`ToolPresentationInput::context.artifacts.retain_text(label, text)` stores
full text through the runtime's `RuntimeAttachmentStore` and records the
`AttachmentRef` in the presentation's artifacts list. Resume of a committed
presentation puts nothing. A crash after a put and before the outcome commits
recomputes the put and the chain. Content addressing converges on the same blob.

After all steps and attachment-materialization notices, the boundary measures
the folded text against `OutputRetentionPolicy`. An oversized return becomes
one `ModelToolReturnPart::Retained` block containing a bounded witness and the
exact text's attachment reference. The outcome records the policy, so resume
loads the same retention decision after a threshold change.

A required retention failure anywhere in the chain carries its typed
attachment-store cause, even when a step catches the put error and returns
text. `AttachmentStoreError::is_retryable()` is the retry authority:
`OutputRetentionFailed` retries the uncommitted derivation only for a
transient cause. A permanent refusal records `OutputRetentionRefused` and
ends presentation. Byte limits and ended referrers never retry.

code mode cell prints and final values follow the same policy in the recorded
`{cell}:outputs` value: history carries `OutputValue::Retained` for oversized
JSON, while the host receives the full final value. Referrer acquisition and
boundary commits follow [ADR 0124](0124-attachments-are-kept-alive-only-by-their-referrers.md).
A session turn holds its puts through `Execution`; a process holds its puts
through `ProcessRecord`. A session boundary acquires the retained ids it names.

Sources: `crates/lash-core-execution/src/plugin/session_obj/tools.rs`,
`crates/lash-core-execution/src/runtime/effect/tool_presentation.rs`, and
`crates/lash-protocol-rlm/src/executor/mod.rs`.

## Consequences

Several plugins can compose the model-facing view, but their order matters.
A recorded outcome fixes that view across resume. Content-addressed retention
survives a worker and tolerates repeated writes in the uncommitted window.
A worker-local spill file cannot provide either guarantee. An unrecorded
presentation hook would let a resumed turn change what the model sees.

The pre-1.0 freeze changes shapes in place. The reader still validates the
presentation format it declares; [ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)
governs the 1.0 upgrade contract.

## Links

- [ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md)
  defines protected drain, presentation and incorporation.
- `crates/lash-core-store/src/tool_run/run_event.rs` defines their recorded fold.
- The crash matrix of ADR 0132 §14 cuts between the retained put and the
  presentation's commit.
