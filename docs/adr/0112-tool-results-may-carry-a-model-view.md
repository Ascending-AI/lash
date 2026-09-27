# 0112: Tool results may carry a model view

## Status

Proposed 2026-09-27.

## Context

In RLM code mode, the model sees what the program prints. A printed structured
tool result passes through the history projector, which limits nested depth,
list items, record fields, and string length. A useful search excerpt can
therefore disappear behind a `max depth 6 reached` marker even though the
program still holds the complete result.

## Decision

A successful tool output may carry optional plain text called `model_view`.
Lashlang receives the structured result and can index it as before. The view
follows the whole value through variables, containers, and function returns.
`print(x)` and single-argument `console.log(x)` show the view when `x` is the
whole result. Printing the container itself uses its structured shape.
Multi-argument console calls retain their existing rendering.
Member reads turn the result into an ordinary structured value and drop the
view. Mutations also drop it because the tool's text no longer describes the
changed value. `JSON.stringify` and Bound Variables
use the structured value.

The recorded tool call retains the structured result and the optional view.
The live projection keeps one copy of the structured value. Its durable
reference stores compact JSON for that value and the view, then rebuilds the
projection on restore. RLM snapshot v27 follows the pre-1.0 bump-and-refuse
rule because history now records which printed outputs came from model views.

Tools know which fields and excerpts a reader needs. Letting them write that
text preserves their intended presentation without changing the general
projector's limits for unrelated program values.

## Alternatives

- Change the structural rules or make them budget aware. Deferred: those rules
  also apply to program-created data and need a separate decision.
- Return text instead of structure. This loses program navigation and a
  structured recorded result.

MCP already separates model-facing `content` from program-facing
`structuredContent`. Lash's MCP plugin keeps the structured value when both
exist. If the content has attachments, it keeps both under `structured` and
`content`. Mapping text content to `model_view` is a follow-up.

## Consequences

The tool may control the text the model sees when it prints a whole result.
The usual print history string budget still applies. History records a flag
for each viewed output so text beginning with `{` or `[` is not parsed again
as JSON and text containing `omitted` or `truncated` is not labeled as a
preview. Mutation removes the view from the changed value. A viewed result
costs the tool's text plus one compact serialized copy of its structured value
in the durable reference; the reference restores that value after a checkpoint.
Single-argument `console.log` uses the same view and history flag as `print`.
