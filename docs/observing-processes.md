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

A process that released to wait reads `waiting`, and its record lists
everything it is blocked on (`ProcessLifecycleState::Waiting { waits }`,
oldest first): a deferred call (`WaitKind::Call`), a key its engine pinned
(`WaitKind::Key`), a sleep (`WaitKind::Sleep { until_ms }`) or another
process's terminal (`WaitKind::Process { process_id }`). Each `WaitState`
carries `since_ms` and, when the engine named one, the `site` (`node_id`,
`occurrence`) of the node that waits; the lashlang engine names the node of a
sleep. A wait never carries a completion key or a wait id: those resolve the
wait, and `Completions::parked` and `pinned_keys` hand them only to a host
that asks for them. The process reads `running` again once its last wait
ends. A process whose steps are still executing on a node is `running`
whatever its engine also waits for.

A park is its actor's fact, not a lifecycle state. While a process is parked
(no engine of its kind, an undecodable state, a refused transition, the
activation-loop budget), `ObservedProcess::park` carries its typed
`ProcessParkReason` beside a lifecycle that still says what the record says.
An operator's redrive or a cancel clears it.

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

## The process feed

Besides the durable facts and the trace records above, a host can follow one
process live. The feed pairs a durable snapshot with a bounded replay of what
was published after it. It carries committed lifecycle facts in sequence and
provisional language execution observations; it does not make node history
durable, and a host that needs a path beyond the replay window still keeps
the trace records as described above.

A process is observed the way a session is ([observing turns](observing-turns.md),
[ADR 0002](adr/0002-session-observation-uses-cursors-and-bounded-live-replay.md)):
a durable snapshot with a cursor, then a bounded live replay after it, with a
typed gap wherever the replay cannot continue. The objects are the process's
own. Process replay shares no store, window or budget with session live
replay, so a busy process never evicts a session's window.

```rust,ignore
let observed = core.processes().observe(&process_id);
let snapshot = observed.snapshot().await?;
apply_durable(&snapshot.read_view);
let mut feed = observed.subscribe_and_recover(snapshot.cursor);

while let Some(item) = feed.next().await {
    match item? {
        ProcessObservationStreamItem::Event(event) => apply(&event.payload),
        ProcessObservationStreamItem::Gap { observation, .. } => {
            replace_durable_and_reset_live(&observation.read_view);
        }
    }
}
```

### The snapshot

`snapshot()` reads the durable process: a `ProcessReadView` at one durable
event sequence, which is the revision of this contract (`ProcessSequence`).

- `Retained` carries the process's row and its effect evidence folded through
  that sequence. The evidence is bounded: retained occurrences and omission
  counts, with `coverage` saying whether the fold reached the sequence, ran
  out of its read budget, met an undecodable fact or started after a released
  prefix. It is never every call the process made.
- `Retired` is a pruned process's tombstone, and `Unknown` an id no row or
  tombstone names. Neither is an empty process at sequence zero.

The snapshot's cursor is the earliest position the replay store still retains
for the process, so a feed from it replays the retained window: an observer
that attaches late, or after the process ended, still receives the node
evidence the window holds. Completion does not shorten the window.

### The feed

A feed yields two kinds of event.

- `LanguageExecution` is provisional: what a language execution reported
  (node starts, waits, branches, completion), with the producer's `event_key`
  as its identity. It never proves a durable advance. An `ExecutionFinished`
  in it does not settle the process.
- `Committed { event }` is one committed lifecycle fact, in sequence. It
  extends the process at `event.sequence - 1`. The feed delivers it only to a
  consumer holding that sequence, and skips it for one that already holds it.
  Only a committed terminal fact, or a terminal read view, settles a process.

Node history is not durable. Starts, branches, loop occurrences, waits and
timings live only in the replay window; a committed effect occurrence proves
that effect's outcome and nothing else about the timeline.

### Gaps

A gap replaces the consumer's state. Its `observation` is the durable read
view read now, and the feed continues from the gap's cursor. On a gap, replace
the durable projection, discard provisional state, and fold what the feed
replays next: the retained window, from its start. What the window no longer
holds stays unknown; do not synthesize it.

`gap.cause` says why:

| Cause | Meaning |
| --- | --- |
| `Replay { reason: Trimmed }` | Retention dropped events after the cursor. |
| `Replay { reason: Unavailable }` | Another store incarnation, a position past the tail, or invalidated continuity. |
| `CommitUnbridged` | The replay holds no committed fact for some sequence between the consumer's and the process's. One later commit is not a bridge: every sequence is required. |
| `AheadOfDurableProcess` | The cursor names a sequence the process never reached. |
| `NotRetained` | No process is retained under the id. The replacement says pruned or unknown, and the feed ends. |

A cursor for another process is refused as an error, never retargeted.

### Delivery and identity

Delivery is at least once. Three identities stay distinct:

- **Delivery:** `ProcessObservationEventId` (process, replay-store incarnation,
  live position). The stream drops one it already delivered within a bounded
  window; seed the window with the identities your host applied
  (`with_applied_event_ids`). A gap clears it.
- **Committed fact:** the process and its sequence, in every incarnation. A
  republication after a takeover is the same fact.
- **Provisional observation:** the process and the producer's `event_key`.

The replay store drops a redelivery whose identity its window holds with the
same fact. The same identity with a different fact fails that publication and
invalidates the process's continuity, which observers see as a gap.

To resume after a restart, persist `feed.cursor()`: it carries the sequence
the consumer holds. An event's own cursor names the sequence the event was
published at, which for a provisional event may be older.

### The replay store

`LashCoreBuilder::process_replay_store` installs a `ProcessReplayStore`. The
default is `InMemoryProcessReplayStore`, sized by
`DataRetention::process_replay`: events, age and bytes per process, and a
process count and byte total across the store. The standard preset (2,048
events, 120 seconds and 8 MiB per process; 4,096 processes and 64 MiB) is
provisional and unmeasured. A window lasts about
`min(max_age, max_events / events per second, max_bytes / bytes per second)`:
at 1,000 events a second, 2,048 events are two seconds.

The store holds what was published to it. Cores that share one store share
every observation. Across OS processes, provisional node history crosses only
through a store they share; with separate in-memory stores a follower still
converges on the durable process through its snapshot and gaps, and the other
process's node events are absent, the same as for sessions.

A store implementation keeps the obligations on the `ProcessReplayStore`
trait; `lash_conformance::process_replay_tests!` certifies them with the same
replay laws the session store passes.
