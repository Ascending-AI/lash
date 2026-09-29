# 0120: Tool presentation is a recorded, composable step

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
  `AttachmentRef`; because the `put` runs inside the journaled boundary, a
  recorded presentation replays unchanged and never puts again, and the
  recorded `artifacts` list names what was retained. A crash after the `put`
  and before the outcome is journaled runs the chain again, and its `put`
  repeats harmlessly: the store is content-addressed, so it converges on the
  same blob (amended FIG-4095).

Amended 2026-09-28 (FIG-3932): standard protocol now registers one required
`ToolPresentationPresenter` before the ordered optional steps. Its
`ToolOutputRenderer` reads the renderer id and resolved per-tool parameters
recorded with the tool command, renders an authored view or the tool value,
and applies a shared character and line limit. A cut keeps head and tail,
records visible ranges, and retains the complete text through the artifact
seam above. An unavailable recorded renderer refuses presentation. The
superseded tool-output-budget plugin and its `SpillPolicy` are deleted.

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

## Amendment (FIG-3562, 2026-09-29): no orchestrating body re-runs

The reason given above for keeping durations out of the command still holds,
but its example is historical: [ADR 0116](0116-tools-are-opaque.md) deletes orchestrating bodies, and a redrive
now serves every tool child's journaled attempt. A declared start's launch
receipt and wait are recorded too, so nothing a presentation depends on
re-runs.

## Amendment (FIG-4095, 2026-09-29): the put may repeat, the record does not

Decision C said the retained `put` happens exactly once. One recorded
presentation can follow more than one `put`: the deployment can die after
the `put` landed and before the `PresentToolResult` outcome reached the
journal, and the redrive then runs the chain again. The repeated `put`
converges on the same content-addressed blob, so the store holds one blob,
the journal one presentation, and every later replay serves that record
without running a step. `crates/lash-restate-test/tests/crash_windows.rs`
crashes in that window on the server double and on a live `restate-server`.

## Amendment (FIG-4125, 2026-09-29)

Item 4: The pre-1.0 freeze changes shapes in place;
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md) governs the
1.0 cut. Item 25 was already corrected by the FIG-4095 amendment above: a
content-addressed put may repeat while the recorded presentation replays
unchanged.

## Amendment (FIG-1643, 2026-09-29): nothing oversized enters history

Retention is no longer a renderer option: `retain_full_output` is deleted,
and a cut output, a failure's included, is always retained. After the last
step, `present_tool_result` measures the return's text against the
session's `OutputRetentionPolicy` (an inline byte limit and a witness byte
bound, set on the host config). An oversized return is retained through
`retain_text` and replaced by one `ModelToolReturnPart::Retained` block: a
bounded witness and the `AttachmentRef` of the exact text. The policy is
recorded in the `ToolPresentation` beside the return, so a replay after a
threshold change serves the recorded decision. A retention that cannot put
is `RuntimeErrorCode::OutputRetentionFailed`, a typed failure that refuses
presentation and is retryable while nothing downstream derived from it; it
never becomes text for the model.

An RLM cell's prints and final value follow the same rule inside the cell's
journaled `{cell}:outputs` language value: an oversized value's JSON is put
as an attachment and history carries `OutputValue::Retained`, while the
host still receives the full final value. The retained attachment is owned
by the turn that put it, so that turn's commit roots it
([ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md)), and a
turn that crashes before its commit leaves an intent the next commit's sweep
reclaims.
