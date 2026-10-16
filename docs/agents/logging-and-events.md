# Logging and event practice

Libraries use structured `tracing` diagnostics. Hosts own subscriber setup,
filtering, export and shutdown. Console output belongs to host binaries,
verification tooling or an explicitly selected trace sink. Keep identities,
classes and counts in separate fields, rather than interpolating them into prose.

A fact has one owning Lash type and stable identity. Activity, diagnostics and
telemetry project that fact; they do not supply decisions. Protocol, provider and
plugin details belong in extra fields or additional diagnostic records. Do not
create parallel reporting types, presentation shapes or wire DTOs. Hosts own
event routing and scheduling ([ADR 0137](../adr/0137-the-host-owns-events-routing-and-scheduling.md)).
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
ownership. Hosts keep their own metadata outside Lash and map it through
`TraceIds` when enriching spans.

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

One host `ContentPolicy` governs every trace consumer, including the OTel
exporter. Its fourteen categories are omitted by default. Opt in with
`ContentPolicy::with(ContentCategory::InputMessages, Capture::Captured)` or
`ContentPolicy::captured()`. Reasoning requires both its direction's consent
and `Reasoning`; nested tool parts follow their message's direction.

Builders use `policy.capture(category, || build())`, so omitted content is
never cloned, rendered or serialized. `LashCoreBuilder::capture_selector`
installs a synchronous host callback keyed by `TraceIds` that can narrow this
ceiling before builders run, including skipping optional leaf kinds. Skip
children first; a skipped parent cannot leave a child enabled. A selector
panic omits content and counts `SelectorFailed`.

One `ContentHook`, installed with `content_hook`, sees every captured field
through `RecordContent::for_each` before any sink. It can redact, omit a
category, or enqueue an upload and add a bounded reference. It must not block,
do I/O or re-enter the engine. A panic clears all captured fields and discards
all references from that call, with `ContentMarks::hook_failed` set. References
share the span's contribution budget with annotations and enrichment.

`ContentLimits` bounds prose parts, whole serialized fields and attributes,
and provider request bodies. Truncation preserves JSON structure; old messages
are dropped whole, and a still oversized field is omitted whole. Schemas,
provider bodies and response metadata are atomic. Sinks read typed fields and
`ContentMarks`; spans carry the corresponding structured GenAI semconv JSON
attributes. There is no event payload blob.

Plugin factories read the deployment ceiling through
`PluginSessionContext::content_policy()`. Telemetry consent is host policy on
reopen and is never persisted as session execution configuration. See the
[tracing contract](../architecture/tracing.md).

## Hosts that read this

A commit that stops a record kind carrying content says so in its body under
**Hosts that read this**, naming the record and where readers find that content
now. Since FIG-6302, `LlmCallStarted` always carries an omitted request: hosts
read request content from `LlmCallCompleted` or `LlmCallFailed`. Openers carry
no content. `ResponseMetadata` is its own consent category and is not exported
by the OTel exporter; it matters to record sinks.

An OpenTelemetry dependency upgrade has an **OTel version** paragraph in its
commit body naming the API, SDK and OTLP versions and what hosts must align.
The current pins are `opentelemetry`, `opentelemetry_sdk` and
`opentelemetry-otlp` 0.33. Keep the instrumentation contract generated from the
registry and the tracing guide's Configuration section generated from policy
rustdoc; their exact-path laws and recorder commands live in
`crates/lash/tests/otel_trace_evidence.rs`.
