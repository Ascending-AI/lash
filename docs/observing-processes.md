# Observing processes

Lash durably keeps a process's lifecycle, outcome and bounded effect
occurrences. Its node-level execution path is telemetry, as it is for
session cells. A host that needs to replay that path keeps the language
execution trace records it receives. The static graph describes possible
work; the trace describes observed work.

## Durable process facts

`ProcessLifecycleFact` records execution starts, process-level waits and
resumptions, cancellation requests, observer changes, external references
and the terminal outcome. A process-level wait is a lifecycle fact; the
individual language node's wait and its timing belong to the trace.
The terminal outcome records how the process ended and its result or typed
failure evidence. Reading those facts does not reconstruct every branch,
loop iteration or node timing.

`ProcessEffectOccurrence` records a settled effect's `node_id`, one-based
`occurrence`, operation, outcome class and failure code when applicable.
For a tool effect, its typed `call_id` identifies the logical Lash call
and joins the effect evidence to its `ToolCallRecord` and language node
start and terminal traces that carry the same call. It is absent for an
effect that is not a tool call. Keep that identity for correlation rather than treating a provider's
call id as Lash's call identity. The occurrence's `replay_key` makes a
repeated append idempotent; it is not a substitute for the typed call id.

Individual effect records have a per-node ceiling of eight
(`PROCESS_EFFECT_OCCURRENCE_CAP`). A host may lower the retained cut with
`TraceLimits::process_effect_occurrences`; zero summarizes all occurrences,
and values above eight resolve to eight. The process pins its cut when it
first advances. Effects beyond the cut still execute: only their individual
evidence is omitted. `ProcessEffectOmissions` records the effective
`occurrence_cap` and each node's omitted success, failure and cancellation
counts in the terminal transaction. An omitted occurrence cannot be
expanded from those counts into its individual call or path.

These facts are committed business evidence. The engine also keeps the
state it needs to continue a process; recovery state is not a host-readable
node history. [ADR 0110](adr/0110-the-engine-owns-process-recovery.md)
describes recovery ownership.

## Node execution is telemetry

Language execution records use `TraceLanguageExecutionPayload`: execution
start and finish, node start, waiting, resumption, completion, failure or
cancellation, branch selection and child start. Repeated nodes carry an
occurrence number. The enclosing `TraceRecord` supplies the timestamp;
node and wait timings come from the observed records, not a durable path
log. A child link names related execution; it does not imply that the host
has received the child's records.

Each serving Lash node emits the execution records for work it observes.
After an owner change, later records can come from another node. Collect
from every node that can execute the process if the host needs its path
across ownership changes. Loading a durable outcome cannot fill in a
missing trace interval. Trace delivery is best effort: a crash or sink
failure can leave a gap, and host persistence only preserves what arrived.
See the [tracing contract](architecture/tracing.md) for lifecycle export
and emission rules.

Content capture is separate from path identity and retention. FIG-5530
specifies one host telemetry content policy, default off, for built-in
telemetry paths. With content off, retain identities, outcomes, counts and
omission evidence; explicit capture keeps original text within its bounds.
Consult the [logging and event practice](agents/logging-and-events.md#channels-and-content)
for the selected sink's content contract. Telemetry policy does not change
durable requests/results or the app's responses.

## Keeping a path for replay

Persist the complete trace envelope, including its execution identity,
event identity, timestamp and payload. Keep records separated by process,
execution attempt and source identity so a redrive or a different module
cannot silently become part of an earlier path. Replay the recorded
observations to render the path; do not execute the process to reconstruct
it. The host owns retention, storage failures and any indication that its
recording is incomplete.

A `TraceSink` receives records, and `JsonlTraceSink` writes one JSON line
per record. `TeeTraceSink` lets a host keep records while also updating a
live graph. Ordinary passive tracing can be installed through
`LashCoreBuilder::trace_sink` or `LashCoreBuilder::trace_jsonl_path`.
For language graph records, the workbench installs its tee with
`TraceRuntime::with_product_observer` and passes that runtime through
`LashCoreBuilder::trace_runtime`.

The concrete example is
[`examples/agent-workbench/src/main_sections/bootstrap.rs`](../examples/agent-workbench/src/main_sections/bootstrap.rs).
It tees language execution records to a live graph store and a JSONL sink.
`AGENT_WORKBENCH_LASHLANG_EXECUTION_TRACE` selects the file; by default it is
`lashlang-execution.jsonl` beneath `AGENT_WORKBENCH_DATA_DIR`. Its separate
passive diagnostic trace is selected by `AGENT_WORKBENCH_TRACE` and defaults
to `trace.jsonl` in that directory. Keep the language execution file when
you need the process's node path. Flush host-owned sinks before orderly
shutdown (`TraceSink::flush`); a flush cannot recover records never emitted.

## Joining observations to the static graph

`WorkflowGraph` describes a module's static structure. Its `WorkflowNodeId`
comes from the structural owner and AST path. Node ids are meaningful within
one source identity, not as global identities across edits or modules.
Use `lash_typescript::workflow_graph::workflow_graph_from_artifact` for the
graph of the admitted artifact: it carries the artifact's `source_identity`,
which language traces also carry in `TraceLanguageExecutionIdentity`.

Join a node trace to the graph using that source identity and `node_id`
(or `parent_node_id` for a child start). Join a durable effect occurrence's
`node_id` against the graph of the artifact its process executes, and use
`occurrence` to distinguish repeated visits to the same static node. Its
call identity links the effect evidence with the tool-call trace. Preserve
the process-to-artifact association with a recording; the occurrence alone
does not carry a source identity. A source-only draft graph claims no runtime
source identity and should not be used as an admitted execution overlay.

## Editing canonical TypeScript

The graph is a semantic view. Comments and authored formatting are discarded.
`lash_typescript::workflow_graph::workflow_graph_to_source` validates and
renders canonical TypeScript, and a node's `source_span` addresses that
canonical output. Hosts own graph edits, drafts, layout and versioning.
Keep the original authored text separately if the editor needs to preserve
comments or formatting. After an edit, admit the resulting program and use
its own graph and source identity for subsequent execution records.
