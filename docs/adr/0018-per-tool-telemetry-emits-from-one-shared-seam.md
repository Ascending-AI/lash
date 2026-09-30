# Per-tool telemetry emits from one shared seam

## Status

accepted

## Decision

Per-tool reporting originates in shared tool execution. `emit_tool_call_started_trace` and `emit_tool_call_completed_trace` emit typed trace events; the same tool-execution path emits `TurnEvent::ToolCallStarted` and `ToolCallCompleted`. Standard native calls and tools invoked during code execution use that path, producing one start/completion pair for each completed call. Identity, provider correlation and issuing language-node containment remain explicit.

Consumers use typed events. `TraceEvent::kind()` defines trace tag strings, the OpenTelemetry sink maps typed events to spans, and exhaustive remote conversion handles each `TurnEvent` variant. `TurnEvent` is closed so a new variant requires updating its exhaustive consumers. There is no exhaustive turn-event-to-trace conversion; the trace and turn vocabularies have their own producers.

## Why and alternatives

Per-protocol emission is rejected because it duplicates detailed reporting and risks dropping or double-counting tools called inside code. Independently authored tag strings and consumer field lists are rejected because they drift without exhaustive typed matches.

## Consequences

Adding a reporting channel happens at the common tool path. Telemetry emission is separate from the protocol decision to invoke the tool; containment ids record that relationship. `ProtocolStep` diagnostics can carry per-tool detail inside their payload, while versioned trace and wire contracts follow their own rules under ADR 0100. The typed JSONL schema and sinks supply trace consumption.

[Trace emission](../../crates/lash-core-execution/src/session/execution_context.rs) and [tool execution and turn events](../../crates/lash-core-execution/src/session/tool_execution.rs) own the shared implementation.
