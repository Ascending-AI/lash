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

A successful tool output with a non-empty object or array may carry optional
plain text called `model_view`.
The program always receives the ordinary structured result. Lash keeps a
session table from the canonical hash of each tool result to its view. A later
result with the same content replaces the earlier view. The latest view wins
for identical content, so a view must be a pure function of the structured
value. If it shows a query or other context, that context must be in the value.
`print(x)` and a single-argument `console.log(x)` show the view while `x`
equals a whole tool result. An equal-content copy built by the program, such
as `{...r}`, also shows the view. Once the value changes, printing uses the
ordinary value. Scalars, null, and empty objects and arrays print normally.
Printing a container or a part of a result uses the ordinary value unless it
also equals a whole result from another tool call. Multi-argument console
calls keep their existing rendering. A result containing projected or
large-string fields uses ordinary printing too.

The table is filled from recorded tool replies, so cell replay uses the same
content and view. Completed cells do not replay when an execution snapshot is
restored. The snapshot therefore stores only the hash-to-view table, without
copying result bodies. The table has an entry limit and evicts the oldest view
first. Replacing a view makes it the newest entry. Recorded call records also
limit the size of each view.

Content matching keeps presentation outside Lashlang values. A projected
wrapper lost mutations through containers and function parameters, changed
single-argument tool calls, and merged identical results on restore. Ordinary
values preserve the runtime's existing aliasing, mutation, serialization,
durable change detection, and restore behavior.

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

The tool may control the text the model sees when it prints a whole unchanged result.
The usual print history string budget still applies. History records a flag
for each viewed output so text beginning with `{` or `[` is not parsed again
as JSON and text containing `omitted` or `truncated` is not labeled as a
preview. Mutation changes the content hash, so the changed value prints
normally. The execution snapshot stores a content hash and view for each
retained distinct recorded result, with the latest view winning for identical
content.
Single-argument `console.log` uses the same view and history flag as `print`.
