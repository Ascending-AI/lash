# Logging and event practice

Libraries use structured `tracing` diagnostics. Hosts own subscriber setup,
filtering, export and shutdown. Console output belongs to host binaries,
verification tooling or an explicitly selected trace sink. Keep identities,
classes and counts in separate fields, rather than interpolating them into prose.

A fact has one owning Lash type and stable identity. Activity, diagnostics and
telemetry project that fact; they do not supply decisions. Protocol, provider and
plugin details belong in extra fields or additional diagnostic records. Do not
create parallel reporting types, presentation shapes, wire DTOs or core signals.
Process lifecycle uses `ProcessLifecycleFact`; custom extensions remain explicitly
namespaced and raw vendor evidence remains raw.

## Correlation fields

| Field | Meaning |
| --- | --- |
| `session_id` | Managed session identity. |
| `run_id` | Durable logical run identity. Host run metadata is separate. |
| `turn_id` | Physical turn identity. |
| `protocol_iteration` | Protocol iteration within a turn, never a turn identity. |
| `llm_call_id` | Lash model-call identity. |
| `tool_call_id` | Lash tool-call identity; provider call IDs remain separate. |
| `process_id` | Lash durable process identity. |
| `os_process_id` | Operating system child PID. |
| `event` | Stable diagnostic transition name. |
| `error_type` | Classification from the typed failure. |
| `error_code` | Stable code from the typed failure, when available. |

Carry known owner IDs on diagnostics of dropped work. Keep high-cardinality IDs
out of metric dimensions and span names. Map domain identifiers to OpenTelemetry
attributes centrally in the adapter; do not equate host metadata with durable
ownership. Host metadata from `TraceContext.run_id` appears as `host_run_id`
in diagnostics.

Hosts install a Rust `tracing` subscriber or logging export layer separately from
`LashCoreBuilder::telemetry`. That adapter projects retained domain observations;
it does not install a diagnostic subscriber. Configure the host layer to retain
structured fields and propagate the current Rust tracing context across tasks.
Join diagnostic owner fields to the corresponding domain attributes documented
in the [instrumentation contract](../../crates/lash/docs/instrumentation-contract.md).
Preserve selected admission anchors and emission permissions: never replace a
retained parent with the current diagnostic context, hold a live SDK span across
suspension or emit another logical completion on replay.

## Ownership and volume

One handling boundary owns the full diagnostic of a propagated failure. Returned
typed errors and span status may coexist. An inner layer that returns and traces
an error does not also log the complete failure. The MCP child guard owns reports
of unreaped children; its pool and lifecycle actor retain health and cleanup
context without repeating that error.

Classify the failed operation independently of a later successful enclosing turn.
Preserve typed classes, codes and source evidence through projections. Cancellation
and handled policy outcomes do not become unhandled failures. Fencing disagreement
may report an invariant breach separately from the caller's refusal.

- Error: an unhandled failure or invariant breach at the reporting boundary.
- Warn: actionable degradation or unexpected loss.
- Info: significant lifecycle transitions and one batch summary with counts.
- Debug/trace: budget refusals, configured limits, polling and per-item decisions.

Do not print full candidate vectors at normal levels. Repeated sink, recovery and
feed-read failures open one degradation episode: report the first failure, count
subsequent failures and report recovery with the episode's count. Keep episodes
local to their sink, lease or process/read operation. A successful absent or
released record is normal, not a store failure. A deliberately discarded fallible
effect identifies its loss contract; unexpected store/effect faults carry structured
evidence. Best-effort diagnostics do not strengthen delivery guarantees.

## Channels and content

Use session observations for app changes, process observations for process facts,
and host logging/passive tracing for diagnostics. A host collecting them together
preserves each channel's origin, identity, cursors, gaps and delivery guarantees.
Wake hints and diagnostics are not durable reliable event feeds. The
[process observation guide](../observing-processes.md) separates retained
process facts from node execution traces and shows how a host keeps a path
for replay.

One host telemetry content policy (`TelemetryContent`, default omitted) governs
prompts, responses, instructions, tool arguments and results, and diagnostic or
provider text on every built-in telemetry path: passive record sinks and the
OpenTelemetry adapter alike. Consent, detail level, sampling and byte bounds are
separate controls. With content omitted, records retain identities, outcomes,
counts and an omission marker, and the content is never built. With explicit
content capture, retain original provider text within documented bounds.
Correctness-required durable requests/results and app responses keep their
existing contracts. A new content-bearing trace field is emptied in
`TraceEvent::omit_content`, and a site that would clone or serialize content
builds it through `TelemetryContent::capture`. See
[durable tracing](../architecture/tracing.md#telemetry-content).

Custom trace payloads are plugin-authored output; Lash does not inspect or
classify them. Plugins must honour the current host policy, available through
`PluginSessionContext::telemetry_content()`. Hosts retain the final export
decision by wrapping their `TraceSink` to drop or redact custom records before
forwarding, as shown in the [host filtering example](../architecture/tracing.md#plugin-custom-payloads-and-host-filtering).

Tests pin a named rule or a demonstrated bug, not vocabulary snapshots or coverage
counts.
