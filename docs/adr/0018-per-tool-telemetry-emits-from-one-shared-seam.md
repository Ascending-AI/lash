# Per-tool telemetry emits from one shared seam and consumers derive from the typed model

## Status

accepted

> Historical note (2026-08-21): The workspace-only `lash-trace-viewer` crate was removed; tracing remains available through the typed JSONL schema and sinks.

## Decision

Per-tool reporting is emitted once, from the shared tool-execution seam, and
every consumer schema is derived from the typed event rather than re-authored by
hand.

- **One emission seam.** `session::execution_context::emit_tool_call_started_trace`
  and `emit_tool_call_completed_trace`, called from `session::tool_execution`,
  are the only place per-tool telemetry originates. The same seam emits both the
  app-facing `TurnEvent::ToolCallStarted` / `ToolCallCompleted` and the durable
  `lash_trace::TraceEvent::ToolCallStarted` / `ToolCallCompleted`. Protocol
  drivers do not emit their own per-tool records.
- **Both modes route through it.** A standard native tool call and a tool
  invoked inside a code-block (Lashlang) execution both go through
  `tool_execution`, so each call produces exactly one Started + one Completed
  pair on every channel. Containment metadata — `graph_key` (the enclosing code
  block) and `parent_call_id` (the parent `batch` dispatch) — is attached at the
  seam, so consumers read containment from the events instead of reconstructing
  it from emission order.
- **Consumers derive from the typed model.** `TraceEvent::kind()` is the single
  source of truth for the `type` tag strings; the trace viewer builds a typed
  `RenderModel` *(Superseded on this point by the 2026-08-21 removal note.)*
  by matching `TraceEvent`; the OpenTelemetry sink maps the same
  typed events to spans (`lash.tool` for tool calls in both modes, the
  `lash.exec_code` family for exec diagnostics with the precise phase carried on
  the `lash.protocol.diagnostic_phase` attribute); and the remote wire mirror is
  a compile-forced exhaustive `From<TurnEvent> for RemoteTurnEvent`. `TurnEvent`
  is deliberately **not** `#[non_exhaustive]` so those matches fail to compile
  until a new variant is handled everywhere.

## Why

The tempting design is for each protocol driver to emit its own per-tool
telemetry: the standard driver records its tool calls, the RLM / code-exec path
records its own. That duplicates the most detail-heavy emission in every driver
and drifts immediately — one path forgets the trace record, another double-counts
a tool that runs inside a code block, a third omits the containment key. The
tool-execution module is the one place every tool call already passes through
regardless of mode, so it is the correct seam: emitting there makes
pair-exactness and identical containment metadata structural rather than a
per-driver convention. A regression test
(`standard_runtime_emits_single_tool_call_trace_pair_per_call`) pins the
one-pair-per-call guarantee.

Deriving consumer schemas from the typed model is the other half. The trace
viewer *(Superseded on this point by the 2026-08-21 removal note.)*, the OTel span
names, the JSONL `type` tags, and the wire DTO all describe the same events; if
each re-derived its own strings and field lists, they would disagree the first
time an event changed. Routing every consumer through `TraceEvent::kind()`, an
exhaustive `TraceEvent` match, or an exhaustive `From<TurnEvent>` turns "keep the
consumers in sync" into a compile error instead of a code-review hope.

## Consequences

- **Adding a tool-reporting channel is one edit at the seam,** not one per
  protocol driver. Everything downstream inherits the new record.
- **A new `TurnEvent` variant is compile-forced** into `TraceEvent::kind()`, the
  trace-viewer `RenderModel` *(Superseded on this point by the 2026-08-21 removal note.)*,
  and `From<TurnEvent> for RemoteTurnEvent`. The exhaustive match is the drift
  guard; there is no version number on `TurnEvent` itself.
- **Exec-diagnostic detail stays additive.** The `exec_code_completed`
  diagnostic carries its per-tool `tool_calls` list inside the free-form
  `ProtocolStep` payload, so richer per-tool reporting shipped without bumping
  `TRACE_SCHEMA_VERSION`.
- **Emission is decoupled from the protocol driver that owns the turn loop.**
  That is a deliberate cost: the code that emits a tool's telemetry is not the
  code that decided to call the tool. The seam is worth it because the alternative
  is per-driver duplication, and the containment keys carry the relationship the
  emission site would otherwise have to imply.

## Considered Alternatives

- **Per-protocol emission (each driver records its own tool telemetry).**
  Rejected: it duplicates emission across the standard and code-exec paths, drifts
  between them, and either double-counts or drops tools that run inside a code
  block. The one guarantee that matters — exactly one Started/Completed pair per
  call on every channel — becomes unenforceable.
- **String-keyed consumer schemas (each consumer re-derives its own tag strings
  and field lists).** Rejected: nothing forces the trace viewer *(Superseded on
  this point by the 2026-08-21 removal note.)*, the OTel span names, and the
  JSONL tags to agree. A renamed field or new variant drifts silently.
  `TraceEvent::kind()` plus exhaustive matches make the same drift a build failure.
