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
Lashlang receives the structured result and can index it as before. Printing
the whole result shows the tool's text, bounded only by the existing print
history string budget. Printing one of its fields or items uses the existing
rules. The recorded tool call retains the structured result and the optional
view. The value's durable projection reference carries both across a code
cell replay or resume.

Tools know which fields and excerpts a reader needs. Letting them write that
text preserves their intended presentation without changing the general
projector's limits for unrelated program values.

## Alternatives

- Change the structural rules or make them budget aware. Deferred: those rules
  also apply to program-created data and need a separate decision.
- Return text instead of structure. This loses program navigation and a
  structured recorded result.

MCP already separates model-facing `content` from program-facing
`structuredContent`. Lash's MCP plugin currently retains `structuredContent`
when both exist and drops `content`. Mapping the latter to `model_view` is a
follow-up; this decision does not change MCP behavior.
