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
keyed by `WaitKind` in a non-empty `ProcessWaits` collection): a deferred call
(`WaitKind::Call`), a key its engine pinned
(`WaitKind::Key`), a sleep (`WaitKind::Sleep { until_ms }`) or another
process's terminal (`WaitKind::Process { process_id }`). Each `WaitState`
carries `since_ms` in signed store milliseconds, as is the sleep's `until_ms`,
and, when the engine named one, the `site` that waits, a
`WorkflowOccurrence`; the lash_vm engine names the site of a sleep and of an
awaited process. A wait never carries a completion key or a wait id: those resolve the
wait, and `Completions::parked` and `pinned_keys` hand them only to a host
that asks for them. The process reads `running` again once its last wait
ends. A process whose steps are still executing on a node is `running`
whatever its engine also waits for.

A park is its actor's fact, not a lifecycle state. While a process is parked
(no engine of its kind, an undecodable state, a refused transition, the
activation-loop budget), `ObservedProcess::park` is `ProcessParkState::Parked`
with its typed `ProcessParkReason` beside the lifecycle. `NotParked` says the
actor store was available and the process is not parked; `NotRead` says the
observer had no actor store. An operator's redrive or a cancel clears a park.

## One read shape

Every read of a process answers an `ObservedProcess`: `processes().get`,
the roster lists, each `ObservedProcessChange::Upsert` of
`processes().changed_since`, and the retained view of an observation
snapshot. It carries the process's identity, provenance, ancestry and
lifetime, its park, and `lifecycle`, the one `ProcessLifecycleState` its row
holds:

- `Running`;
- `Waiting { waits }`, with everything it is blocked on;
- `Terminal { outcome, occurred_at_ms }`, with the typed `ProcessTerminal`
  (the settled output of a success, failure or cancellation, or the
  abandonment evidence) and the time of the committed fact that ended it.

`occurred_at_ms` is the terminal fact's own time. A fact appended after the
process ended (an observer change, an external reference) moves
`updated_at_ms` and leaves it alone. `status()`, `waits()` and `terminal()`
read the state; nothing is stored beside it.

Lash formats nothing for display. A label for a process with none
registered, a status line, a failure message and a key for a graph view are
the host's to derive from these facts.

`ProcessEffectOccurrence` records where a settled effect ran (`at`), its
operation, outcome class and failure code when applicable. `at` is a
`WorkflowOccurrence`, the one value every layer names an occurrence by: the
`site` (`node_id` and `site_path`, the typed slot path to the expression that
ran, in the statement the node stands for, with the role of a synthetic site
such as a labeled step), the one-based `occurrence`, and the `loops` around
it, outermost first: each loop's site, its activation (unique within the run;
a loop entered again is a new one) and its position, `body` with the
one-based iteration or `check` with the one-based evaluation of a `while`
condition. An occurrence counts per site, so two calls in one statement each
start at 1. A site at the node's own statement omits `site_path`, and an
occurrence outside every loop omits `loops`.
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
start and finish, and `Node { at, fact }` for a fact about one occurrence,
where `fact` (`TraceNodeFact`) is a start, waiting, resumption, completion,
failure or cancellation, a branch selection or a child start. `at` is the
same `WorkflowOccurrence` a durable effect occurrence carries; a
continuation keeps its numbering and loops, so a run that parks and resumes
numbers on. The enclosing `TraceRecord` supplies the timestamp;
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
per record. Ordinary passive tracing can be installed through
`LashCoreBuilder::trace_sink` or `LashCoreBuilder::trace_jsonl_path`.
To keep the language execution records, install a sink with
`TraceRuntime::with_product_observer` and pass that runtime through
`LashCoreBuilder::trace_runtime`. This export is a recording. The graphs a
host shows live come from the feeds described below, never from it.

The concrete example is
[`examples/agent-workbench/src/main_sections/bootstrap.rs`](../examples/agent-workbench/src/main_sections/bootstrap.rs).
It writes language execution records to a JSONL sink.
`AGENT_WORKBENCH_LASH_VM_EXECUTION_TRACE` selects the file; by default it is
`lash-vm-execution.jsonl` beneath `AGENT_WORKBENCH_DATA_DIR`. Its separate
passive diagnostic trace is selected by `AGENT_WORKBENCH_TRACE` and defaults
to `trace.jsonl` in that directory. Keep the language execution file when
you need the process's node path. Flush host-owned sinks before orderly
shutdown (`TraceSink::flush`); a flush cannot recover records never emitted.

## Joining observations to the static graph

`WorkflowGraph` describes a module's static structure. Its `WorkflowNodeId`
comes from the structural owner and AST path. Node ids are meaningful within
one source identity, not as global identities across edits or modules.
Read the graph of what a process runs from Lash
(`core.processes().graph(&process_id)`, or
`host_artifacts().execution_document(&reference)` for the document an
execution names): it carries the artifact's `source_identity`, which
language traces also carry in `TraceLanguageExecutionIdentity`.

Join a node trace to the graph using that source identity and the
`at.site` of the fact. Join a durable effect occurrence the same way: its
`at.site.node_id` names the node, and its `at.site.site_path` is the
`site_path` of one of that node's `execution_sites` in the graph of the
artifact its process executes. Use `at.occurrence` and `at.loops` to
distinguish repeated visits to the same site. Its
call identity links the effect evidence with the tool-call trace. Preserve
the process-to-artifact association with a recording; the occurrence alone
does not carry a source identity. A draft claims no runtime source identity
and should not be used as an admitted execution overlay.

## Editing a workflow

A host edits the typed document through `lash::workflow::WorkflowDraft` and
publishes the result as a new definition; no source language is involved.
After an edit, a run reports against the new definition's own document and
source identity. [Editing, publishing and showing workflows](workflow-hosts.md)
covers the document, typed edits, node identity across edits, publication and
TypeScript as an optional view.

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

- `Retained` carries the process's row, its effect evidence folded through
  that sequence, and the document it runs. The evidence is bounded: retained
  occurrences and omission counts, with `coverage` saying whether the fold
  reached the sequence, ran out of its read budget, met an undecodable fact
  or started after a released prefix. It is never every call the process
  made. The read budget is the core's
  `ObservationWorkLimits::process_effect_fold_pages` pages of
  `process_effect_fold_page_size` events (64 pages of 256 in the standard
  preset), stated with `LashCoreBuilder::observation_work_limits`.
- `Retired` is a pruned process's tombstone, and `Unknown` an id no row or
  tombstone names. Neither is an empty process at sequence zero.

The snapshot names the workflow document, and never carries the graph.
`RetainedProcessView::document` is a `ProcessDocumentIdentity`:

- `Available(WorkflowDocumentRef)`: the document's `source_identity`, the
  `module_ref` it is read from, the `entry` the process starts at
  (`WorkflowDocumentEntry::Process { process_ref }`) and the `ir_version` it
  is written under. The reference never changes for a process.
- `ArtifactUnavailable { artifact }`: nothing retains an artifact the
  definition reads. The process is still observed.
- `Unsupported`: the process's engine has no workflow document.

Read the graph with `processes().graph(&process_id)` and cache it under the
reference. Join provisional node evidence to it only where the observation's
`source_identity` is the reference's.

The snapshot's cursor is the earliest position the replay store still retains
for the process, so a feed from it replays the retained window: an observer
that attaches late, or after the process ended, still receives the node
evidence the window holds. Completion does not shorten the window.

### The feed

A feed yields three kinds of event.

- `StepBodyStarted` is provisional: the admitted body of one step started,
  at its site, under its call and attempt.
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

### Folding an execution overlay

Lash keeps no graph of a running process. The workflow document is the
static truth: its nodes, their labels and kinds, the arms of a branch. What
an execution did is an overlay a host folds over that document, with the pure
reducer:

```rust,ignore
let mut overlay = WorkflowExecutionOverlayAccumulator::default();
overlay.set_document(document.overlay_document()); // the document the execution names
overlay.observe(&observation)?;          // a LanguageExecution event
overlay.step_body_started(&started)?;    // a StepBodyStarted event
overlay.settle(settlement);              // a committed Terminal, or a terminal read view
overlay.reset_live();                    // a gap
let view = overlay.snapshot();
```

An execution's `ExecutionStarted` names its document
(`WorkflowDocumentRef`), and so does a process's snapshot. Read it with
`host_artifacts().execution_document(&reference)`: it answers a session
cell's main body as well as a process's definition. Lash holds a process's
document as long as it holds the process's definition. It holds a cell's
only while the cell's execution is unsettled, unless a global of the frame
names a process the cell declared: read a cell's document when its start
arrives, and keep your own copy if you archive its observations. A document
Lash no longer holds reads as `WorkflowDocumentRead::Unavailable`, and the
overlay's `coverage.document_loaded` stays `false`. Events carry no labels,
kinds or edges; look those up in the document by the event's site
(`at.site`). A `BranchSelected` fact names the typed arm
it took (`then` or `else`); which nodes the other arm holds is the document's
to say.

The overlay holds one state per observed execution site: its latest
occurrence, the arm a branch site chose, the call an admitted step ran
under, and a bounded history. It never lists a site that was not observed,
and never grafts one the document lacks: a site outside the document is a
typed `WorkflowOverlayMismatch`. `coverage` says what the overlay rests on.
An observer that attached after the execution started still loads the right
document from the snapshot's reference, and `coverage.start_observed` is
`false` until the start is replayed.

`StepBodyStarted` reports that the admitted body of a step is starting: the
process, the exact site and occurrence, the admitted call and the attempt. A
step that was refused reports none, and a retried body reports again with the
same occurrence and call and the next attempt.

`settle` takes the process's durable end
(`WorkflowOverlaySettlement { terminal, occurred_at }`): from a committed
`Terminal` fact its `occurred_at_ms`, from a terminal read view the
lifecycle's `occurred_at_ms`. A committed terminal settles the overlay with
no `ExecutionFinished` at all. A settled overlay cancels only the
occurrences it observed in flight, never starts an unobserved site, and is
not reopened by evidence replayed after it. `reset_live` discards the
provisional history at a gap and keeps the document and the durable end.

Keep one accumulator per `identity.graph_key()` and bound the cache: the
reducer bounds the occurrences of one site, not the number of processes. The
cache is a projection; it is never what a feed recovers from.

A session's cells run the same language. Their observations arrive on the
session's feed as `SessionObservationEventPayload::LanguageExecution`
([observing turns](observing-turns.md)), with the effect's identity in the
payload, and fold the same way. Lash publishes each observation once: a
cell's on its session's feed, a process's on the process feed.
[`examples/agent-workbench/src/execution_feeds.rs`](../examples/agent-workbench/src/execution_feeds.rs)
follows both into one bounded cache, and
[`execution_view.rs`](../examples/agent-workbench/src/execution_view.rs)
draws each overlay over its document.

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

A gap carries its requested cursor once. Its replacement is either `Replaced`
with the retained view, one continuation cursor and a cause above, or `Ended`
with a retired tombstone or unknown id. An ended replacement has no replay
cause or continuation cursor; its read view has no durable sequence.

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

### Convergence across cores

An open feed converges on the durable process whoever committed to it. A
commit on the feed's own core reaches the replay store after the commit. A
commit by another core over the same stores does not pass through this
core's publisher, so the feed looks for it: the registry's change signal
ticks when a commit grew the process's log, on this node directly and on
another through the backend's node wakes. At a tick the feed compares the
durable sequence with the one its consumer holds, publishes the retained
facts between to the replay store and delivers them in order.

The feed bridges a short distance that way, at most
`ObservationWorkLimits::process_reconcile_bridge_events` facts (256 in the
standard preset). A consumer further behind gets a `CommitUnbridged` gap
with the durable read view at once, and the feed publishes nothing, so a
stalled consumer never pushes other observers' node events out of the
window. Facts the feed can no longer read (released or pruned), and a fact
whose publication was dropped or refused, are the same gap. The feed stays
open after it; only a failed read of the durable process ends a feed with an
error.

An idle feed reads nothing: it does not poll the durable process. A node
whose wake listener lost its connection ticks every followed process when it
resubscribes, so a wake missed in between is made up then. No host timer,
event-page loop or resubscribe is needed; a host only reads the feed.

What crosses cores this way is the committed facts. Provisional node events
cross only through a shared replay store, as the next section says.

`processes().await_output` waits on the same change signal: a terminal
committed by any core over the stores wakes it, with the work cadence as the
fallback for a lost wake.

### Durable history pages

`processes().events(from, limit, mode)` pages the committed log for
deliberate inspection. `from` is a `ProcessHistoryContinuation`: a process
and the last sequence the reader holds (`ProcessHistoryContinuation::start`
before the first event). Each read returns `next`, where the following page
starts; a read inside a released prefix answers the typed release and
continues after it. A continuation is a position in the durable log and
nothing else. It is not a feed cursor and establishes no live continuity.

### The replay store

`LashCoreBuilder::process_replay_store` installs a `ProcessReplayStore`. The
default is `InMemoryProcessReplayStore`, sized by
`DataRetention::process_replay`: events, age and bytes per process, and a
process count and byte total across the store. The standard preset (2,048
events, 120 seconds and 8 MiB per process; 4,096 processes and 64 MiB) is
provisional and unmeasured. A window lasts about
`min(max_age, max_events / events per second, max_bytes / bytes per second)`:
at 1,000 events a second, 2,048 events are two seconds. A window nobody
follows is released once it has been idle for `max_age`; a window with a
live subscriber is not, so a process that waits longer than that does not
gap its connected followers.

The aggregate process count and byte bounds are hard limits: capacity pressure
evicts the least recently touched window even with live subscribers (subscribing
and subscriber upkeep refresh that touch), gaps its cursors and closes its
subscriptions.

The store holds what was published to it. Cores that share one store share
every observation. Across OS processes, provisional node history crosses only
through a store they share; with separate in-memory stores a follower still
converges on the durable process through its snapshot and gaps, and the other
process's node events are absent, the same as for sessions.

`lash::postgres::PostgresProcessReplayStore` is that shared store: every
replica of a host publishes to and reads from the same PostgreSQL tables, so
an observer on one replica sees the node events another replica's execution
produced, in one position order, without contacting it. Configure it with the
`process_replay` section of the host configuration
([`operations/postgres.md`](operations/postgres.md#process_replay)) and
install `PostgresHost::process_replay`. Its tables, incarnation and budgets
are its own, apart from the session live replay store's. They are unlogged:
after a crash recovery or failover the store starts a new incarnation and
every older cursor gaps. On SQLite there is no shared replay store, so node
events stay with the OS process that produced them.

A store implementation keeps the obligations on the `ProcessReplayStore`
trait; `lash_conformance::process_replay_tests!` certifies them with the same
replay laws the session store passes.
