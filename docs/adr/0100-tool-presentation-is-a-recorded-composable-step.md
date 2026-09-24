# 0100: Tool presentation is a recorded, composable step

## Status

Accepted 2026-09-22 (FIG-3420). Implements the presentation boundary of
[ADR 0099 §6](0099-tool-children-of-effect-groups-are-live-closing-settled.md).

## Context

A tool result crossed one boundary before the model saw it: a single
`ToolResultProjector` hook folded the settled output into a `ModelToolReturn`.
Three properties of that boundary had drifted from the rest of the runtime:

* **Singleton.** The registrar owned `model_observation` exclusively, so a
  session could carry exactly one projector. A second plugin needing to shape
  the model's view — the budget plugin and the aggregate oracle being the
  first real pair — could not coexist; registration refused the second.
* **Unrecorded.** The projector ran at settlement time outside any journaled
  effect. A replayed child re-ran whatever projector happened to be registered
  on the successor host, so the model-facing text depended on caller timing
  and on which build answered the redrive.
* **Worker-local spill.** The budget plugin's `SpillPolicy` wrote retained
  full output to a filesystem directory on whichever worker ran the
  projector — invisible to the journal, unreachable from a second host, and a
  side effect no replay could dedupe.

## Decision

* **A. Ordered steps, not a singleton.** `ToolResultProjector` is replaced by
  `ToolPresentationStep`, a plain `Fn(ToolPresentationInput) ->
  PluginFuture<ModelToolReturn>` a plugin registers through
  `registrar.tool_results().presentation_step(..)`. Steps compose in
  registration order: each folds the previous step's return with the recorded
  settlement. A step error yields the fallback text return and the chain
  continues from it.
* **B. The chain runs inside a journaled effect.** `RuntimeEffectCommand::
  PresentToolResult` carries the settled output once; its outcome is a
  versioned `ToolPresentation { version, model_return, artifacts }` guarded by
  `TOOL_PRESENTATION_VERSION`. Both call sites — the scalar
  `complete_tool_call` path (replay key `{call_id}:present`) and the
  group-child driver under the child's bound controller — execute it through
  the scoped controller, so replay serves the recorded outcome and no step
  ever re-runs. The settlement's `model_return` is the recorded presentation;
  incorporation consumes it unchanged. The command names only recorded facts
  (call id, tool, arguments, settled output). How long the call took is an
  observation the steps read from the local executor and never part of the
  command: a redrive serves a journaled attempt at once and re-runs an
  orchestrating body, so a duration in the envelope would refuse a healthy
  redrive with a replay hash conflict. A failure of the presentation effect
  itself — a replay divergence against its record, a journal fault — is no
  presentation: it never becomes the call's model-facing return. On the turn
  path it aborts the turn, so a divergence parks it like any recorded
  effect's (FIG-3587); in a code cell it stops the run at the call's command;
  in a group child it refuses the child (FIG-3679).
* **C. Retention is a journaled artifact, not a file.** `SpillPolicy` is
  deleted. A step that keeps full output calls
  `ToolPresentationInput::context.artifacts.retain_text(label, text)`, which the
  runtime binds to the session's content-addressed `SessionAttachmentStore`
  (manifest-referenced, so mark-and-sweep GC retains it). The hint names the
  `AttachmentRef`; because the `put` runs inside the journaled boundary it
  happens exactly once, and the recorded `artifacts` list names what was
  retained.

## Consequences

* Any number of plugins shape the model-facing result; ordering is the
  registration order the session was built with, and that order is part of
  the recorded environment — it cannot be reinterpreted on replay.
* A crash between the child settling and the model reading the result replays
  the recorded presentation verbatim; a changed step chain on a successor
  host cannot alter what the model was shown.
* Retained output survives the worker that produced it and is readable
  through any facade over the same attachment store; there is no `dir` to
  configure, clean, or lose.
* `ToolPresentation` is a new durable surface: a field added, retired or
  retyped anywhere in its serialized closure bumps `TOOL_PRESENTATION_VERSION`.

## Links

* [ADR 0099 §6](0099-tool-children-of-effect-groups-are-live-closing-settled.md)
  — the presentation boundary this ADR implements.
* `ToolSettlement` (`runtime/effect/tool_settlement.rs`) — the record the
  steps fold against and the record incorporation consumes.
