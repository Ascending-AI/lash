# Observing turns

Recovery and external-effect guarantees follow [What survives a crash](../README.md#what-survives-a-crash).

Turn activities arrive on the session observation stream in cursor order.
Keep the cursor and use the bounded live replay described in
[ADR 0002](adr/0002-session-observation-uses-cursors-and-bounded-live-replay.md)
when reconnecting. A replay gap means the missing activities cannot be
reconstructed from the durable session view.

Start a feed from `session.observe().snapshot().await`: the session's
durable head with a cursor bound to its revision, then follow
`subscribe_and_recover(cursor)`. A feed judges its cursor against the durable
head, and a gap's replacement snapshot is the durable head too, so an
observer whose own handle never adopted a commit another process made still
gets a current snapshot. Which processes' activities reach the feed is the
configured live replay store's property: the in-memory default holds one
process's, and a shared store holds every process's. Lash ships one shared
store, `lash::postgres::PostgresLiveReplayStore` (the `postgres` feature):
every replica connected to one PostgreSQL database sees every replica's
events, with the same window, cursors and gaps as the in-memory store. Its
tables come from the published `crates/lash/postgres-live-replay-schema.sql`:
the store creates them itself by default, or a host applies the file and runs
the store with `schema_mode: verify_only`, which runs no DDL and refuses tables
that differ (see the [host-provisioned schema runbook](../runbooks/host-provisioned-schema/runbook.md)).

`CheckpointRecorded { protocol_iteration }` marks an accepted checkpoint on the
turn's lane. Every non-retracted delta before the marker belongs to the
checkpointed state. A cancelled tool batch can itself be checkpointed, so the
marker decides the cut. The marker is live-only: like every other turn
activity, it is absent from durable history.

Delivery is at least once. Every event has a `SessionObservationEventId`
(its session, replay-store incarnation and cursor), safe to persist across a
process restart. The stream drops an identity it already delivered within a
bounded window of 4,096; seed the window with the identities your host applied
(`with_applied_event_ids`) so a reconnect from a trailing cursor redelivers
nothing twice. A gap or a commit clears the window. Dropping the stream only
disconnects observation; it never cancels work.

A replay store's `invalidate_all` retires every session window and wakes all
open subscribers. Each feed reports an unavailable gap with a durable replacement
snapshot, then continues from its replacement cursor. Store-wide invalidation is
available when bounded publication ingress loses continuity for more subjects
than it can remember individually. It discards live evidence; durable history
remains authoritative.

## Language execution evidence

`LanguageExecution` carries the language, its canonical execution identity and
payload, and the observation time. Routing uses the execution's typed subject:
process executions and process-scoped effects reach process replay, while
turn and session effects reach session replay. The language name does not
decide admission. Provisional observations move the live position without
proving a durable revision advance.

Language ingress admits at most 4 MiB of charged serialized payload per class
and a bounded count of events, including its publications in flight: 256 for
the session class, and for the process class two rounds of what its replay
store writes at once (`ProcessReplayStore::publish_limits`: concurrency times
batch size, so 4,096 events for the PostgreSQL store's defaults). Process and
session classes drain independently outside VM execution. The process class's
worker takes everything pending and publishes it one batch per process, the
processes side by side. A process's observations and after-commit facts share
a FIFO: a terminal follows every accepted preceding
observation. Recovery publishes through the same FIFO and awaits its
completion before checking the committed bridge; execution only enqueues and
never awaits that completion. Overflow clears that class's pending queue and coalesces one
store-wide invalidation, so loss cannot silently preserve an old cursor's
continuity.

A host folds the typed observations with the pure graph accumulator. Only a
committed terminal or terminal durable snapshot settles a process graph;
`ExecutionFinished` is provisional. Cancellation settles observed in-flight
occurrences and leaves unobserved and already terminal nodes alone. A snapshot
without the canonical terminal occurrence time carries an unknown time:
in-flight nodes become incomplete with the durable category, without invented
end timestamps. A later committed terminal supplies its actual time. Other
terminal categories retain incomplete node outcomes rather than fabricating
node success or failure. On a gap, reset provisional evidence and refold the
retained window; durable settlement survives the reset and delayed starts
cannot reopen it.

## Placing committed entries

A live activity is provisional. `Committed { base_revision, entries }`
supplies the typed transcript entries that commit added, including named
suppressions; it never repeats the earlier transcript and never carries the
session's read view. `entries` extend the session at `base_revision`: the
feed delivers a commit only to a consumer holding that revision, and answers
any other with a replay gap and the durable head. Replace the preview for
each entry's typed turn provenance, then render the entry yourself: lash
hands you roles, content blocks, tool calls and results, code cells and their
typed outcomes, and the sealed reply, never display text
([ADR 0129](adr/0129-committed-history-is-typed-facts-a-host-renders.md)). Read
`durable.transcript()` for a complete retained transcript after reconnecting
across a replay gap.

On the durable substrate the node that commits publishes the commit once its
owner's commit is acknowledged: a turn's `turn.commit` after the turn's own
activity, covering a context-pressure frame its preparation opened under
`pressure.frame`, and a session command's or compaction's `session.command`,
each with an `AgentFrameSwitched` ahead of it when the commit left the
session's frame for another; the commit that opens a session's initial frame
switches nothing. The switch names its commit's revision in `commit`, and the
feed delivers it only to a consumer that does not yet hold that revision: a
snapshot taken after the commit already stands on the frame.
An owner that lost a commit's acknowledgement, or a node lost before it
published, is covered by the next pass over the session: it announces the
durable head as a `Committed` whose `base_revision` is the head itself and
whose `entries` are empty. A consumer holding the head skips it as a
redelivery; one holding an earlier revision cannot extend it and rebuilds
from the durable head, which the feed answers as a replay gap.

A commit is durable before its `Committed` is published, and a run answers
from its durable terminal. A cursor read before the commit and resumed right
after the run's output is therefore behind the head with no bridge yet. The
node that made the commit holds that reader until the publication was
attempted, then judges the replay again: a late publication is not a gap. A
publication that failed, or a commit made on another node whose observation
has not arrived, still answers a replay gap with the durable head.

```rust,ignore
if let lash::observe::SessionObservationEventPayload::Committed { entries, .. } = &event.payload {
    for entry in entries.iter().filter(|entry| !entry.is_suppressed()) {
        place_entry(entry); // the host's own rendering of a typed entry
    }
}
```

`entry_id` supports equality and transport. An entry's position in a
transcript orders it; that order is not a cursor.

## Knowing a turn settled

A turn that commits publishes one `Committed` whose event `turn_id` names
the turn. It does so for every outcome: finished, stopped (a cancel or a
failure), or switched to another agent frame. The event comes after
everything the turn streamed and after the turn's outcome activity, and only
once the commit is durable. That event is the signal to settle the turn. Read how its run ended from the run's answer, which reads
the durable terminal and answers at once: `SendHandle::outcome` for the
send, or `session.run(run).outcome()` by the run's id.

Do not wait for a reply entry. A turn commits a reply, an entry whose
provenance has `is_turn_reply`, only when it finishes. A stopped turn
commits no reply. An `Immediate` stop commits the turn backtracked to its
last checkpoint, and an `AfterStep` stop commits through the step it waited
for. Neither has a reply.

A `Committed` is a wake, not the answer:

- **It names a physical turn.** A run that goes on past a commit, after an
  agent-frame switch, commits each of its turns. The first commits under the
  run's id and each later one under an id lash derives from it. The run's
  answer says whether the run has ended.
- **Some endings commit nothing.** A turn that ends with a refusal commits
  nothing, whether the refusal came as it prepared or as it finished. Neither
  does a stop that has no state to commit: one made before the turn's first
  phase recorded any, or one whose state this build cannot decode or whose
  head commit is refused. These endings publish no `Committed`, and the run's
  answer is the only place they show.
- **Publication is best effort.** A node lost after its commit publishes
  nothing, and the next pass announces the durable head as a `Committed` with
  no `turn_id` and no entries. On such an announcement or a replay gap, read
  the answer of every run the host still holds open.

## A stopped turn's tail: the live stream is the contract

Lash keeps no durable record of what a stopped turn streamed after its last
checkpoint, and never feeds it back
([ADR 0122](adr/0122-a-stopped-turns-uncommitted-tail-lives-only-on-the-live-stream.md)).

- **Lash keeps no uncommitted tail.** An `Immediate` stop backtracks to the
  last checkpoint, so the next turn's context is that checkpoint. An
  `AfterStep` stop waits for its step's checkpoint and leaves no tail.
- **The tail is the deltas after the last marker.** Take the prose and
  reasoning deltas after the turn's last `CheckpointRecorded`, or after
  `TurnStarted` if none was recorded, and remove those whose correlation IDs
  a later `ModelAttemptReset` retracts. Resubmitting that text is your
  choice, as ordinary input. Tool arguments are not streamed, and a tool's
  provisional output arrives with `ToolCallReturned`. Only
  `ToolCallCompleted` reports an outcome accepted by the owning round or cell
  commit. A return may never complete after an Immediate stop or worker loss;
  a restored Once member can complete with Interrupted instead of its lost
  success. `CodeBlockReturned` and `CodeBlockCompleted` make the same split
  for cells: completion follows the commit consuming the cell answer and
  carries its committed `CellOutcome`. Use returns for live progress and
  completions for done state.
- **Keep up, or the tail is gone for good.** Follow the stream within the live
  replay window: 2,048 events or 120 seconds per session by default, on the
  replay of the process the turn runs in. A replay gap, a follower that lags
  its subscription and jumps to the head, or a turn that runs on another
  process without a shared live replay store loses the tail, and nothing can
  recover it. A slow sink loses nothing: the deltas it has not taken yet pile
  into the frame of their block, including across a cancellation.
- **A delta is appended text, often several tokens.** The host chooses how
  deltas are coalesced with the required `LashCoreBuilder::delta_coalescing`:
  the interval, the frame size cap and the first-delta flush, or
  `DeltaCoalescing::off()`. Under `DeltaCoalescing::recommended()` the first
  prose or reasoning delta of a block arrives at once, and the block's later
  deltas arrive coalesced into frames of about 50 ms, cut early by any other
  event. Append each delta's text to its block, as
  for a single token. A frame's activity ID names the delta range it covers,
  so redelivery within a retained range adds no duplicate text. A partial
  overlap invalidates continuity and yields a gap; retention loss can discard
  text, and a re-sent model call resets its abandoned attempt's text
  ([ADR 0040](adr/0040-retried-model-attempts-retract-live-text-by-correlation.md)).
- **`Stopped` is published only after its commit.** A stopped turn's terminal
  (the `Done` a cancel records, an `Error`, `TurnOutcome { Stopped }` and the
  final `Done`) reaches you only once the turn's commit is accepted. A commit
  that fails publishes none of it.

## Internal phase instrumentation

Workspace measurement and fault-injection tools use the hidden turn-phase
probe. Its vocabulary is explicitly unstable. See the
[turn-phase instrumentation contract](architecture/turn-phase-probe.md) for
callback, registration and naming rules.

## Discovering a process fleet

`core.processes().list(&filter, limit, continuation)` reads a bounded page
of canonical process states. A page carries `processes`, `continuation`,
`change_cursor` and `verified_through`. Start with no continuation and follow
every continuation, including when a filtered page is empty. Pages examine
at most 256 candidates plus one lookahead, without loading the whole roster.
The continuation belongs to its issuing store and original filter.

To transport a continuation, select a `lash::process::ProcessRosterTokenBudget`
and call `cursor.to_token(budget)?`. Resume with
`ProcessRosterCursor::from_token(&token, budget)?`. The default is 64 KiB of
complete token text, including the version prefix and hex-encoded JSON.
`ProcessRosterTokenBudget::new(bytes)` accepts nonzero budgets up to the
16 MiB hard maximum (`MAX_BYTES`), bounding decoder allocation and parsing.
Both operations return `ProcessRosterTokenError::TooLarge` when the token
exceeds the selected budget; encoding refuses before issuing an unusable
continuation. Keep the selected budget with the host's scan state and reuse
it when reopening that scan; the codec reads no live configuration.

The first page captures `change_cursor` before scanning, and every later
page retains that fence. This is a convergent scan, rather than a snapshot
across pages: concurrent inserts and updates can appear during enumeration.
After scanning, drain `changed_since(change_cursor, limit)`. Each entry
carries an opaque `cursor`; a host can atomically commit an entry's projection
and that cursor in its own database, then resume strictly after it following
a crash. Lash provides no cross-database transaction. Apply the whole page
before retaining its `next` cursor. Even an empty change page
reports a verified high water. `verified_through` is the page's verified
change high water; it does not replace the pre-scan fence.

Changes are global latest-row upserts or deletion tombstones. Reapply the
roster's selection to each upsert: a row leaving the filter must be removed,
and a deletion removes its id. These trusted fleet reads require host
authorization; they do not grant session observer or provenance access.

`ChangePage::retained_after` reports the tombstone-compaction horizon. A
`ProcessChangeCursorPruned` refusal from changes or a scan continuation means
discarding the roster and starting a fresh scan. Its new fence lets scanning
restart even when deletion history before that fence is gone. Process facts
and provisional node observations continue through each process's recovering
observation feed; the fleet cursor is separate from its observation cursor.

## Tool progress and logs

Tool calls publish live progress and logs through a separate tool-live lane.
A host reads `core.tool_live().subscribe(ToolLiveFilter::Owner(owner)).await`
for a session frame or process, or uses `ToolLiveFilter::Call(address)` for one
card. `CallAddress` includes the admitted `ExecutionOwner`, Lash call ID and
attempt. Every emission carries that address and a stable producer epoch and
ordinal. `ToolLiveFact::Progress` forwards the gauge, optional total and
message; `Log` forwards severity, optional logger and JSON data.

Any tool, including a host-executed tool, gets the sealed publisher from
`AttemptContext::live()`. It bypasses the attempt recorder and never enters
history or process lifecycle facts. Its authoritative tool outcome is unchanged
by observation or store failure: `publish` returns nothing and never awaits.
The MCP plugin maps progress tokens and request-correlated messages onto this
same lane. Uncorrelated server logs go to tracing on the executing node; list
changes maintain the plugin catalogue.

### The session's owner before its first turn

A session call runs under `ExecutionOwner::SessionFrame` for the frame it was
admitted on. A new session already has that owner: its initial frame, which the
first commit opens. `ObservableSession::tool_live_owner()` reads it from the
durable head, so a host subscribes before it sends the first turn:

```rust
let session = core.session(id).created().await.open().await?;
let owner = session.observe().tool_live_owner().await?.expect("frame");
let mut feed = core.tool_live().subscribe(ToolLiveFilter::Owner(owner)).await?;
let turn = session.send(TurnInput::text("fetch the report")).output();
// Poll `feed` while `turn` runs: the first tool's progress arrives live.
```

A compaction or frame switch moves later calls to a new frame, and so to a new
owner. `ObservableSession::tool_live_owners()` yields the current owner and
then each owner a later commit moves the session to, whichever replica made it;
subscribe again for each one. Cursors belong to one owner.

### Items, filters and the wire

`ToolLiveItem` is one of `Emission` (one fact, or one `Omitted` count, for one
call), `OwnerOmitted` (a count no call's address can carry), `Fault` (a
retained record that could not be read; the feed continues after it) or `Gap`.
A `Call` filter receives its call's emissions and every owner-level item.
`ToolLiveItem` and `ToolLiveFilter` serialize as JSON objects tagged by
`kind` (`emission`, `owner_omitted`, `fault`, `gap`; `owner`, `call`), with
the fields named as in Rust, so a host route can carry them unchanged.

### Capacity

`LashCoreBuilder::tool_live_policy` sets validated per-owner publisher bounds;
`ToolLivePolicy::snapshot()` reads every configured and derived bound back.
The defaults are 64 records and 16 logs per 100 ms interval, 512 KiB, 64 KiB
per fact, 1 KiB per progress record and 128 calls. Store writes have a
five-second timeout, and completed slots have a two-second idle linger.
`ToolLivePolicy::new` takes `write_timeout` and `completed_linger` as typed
`Duration` values after `interval`. Write timeout accepts 10 ms through one
hour; completed linger accepts zero through one hour. These maxima are hard
safety ceilings. Zero linger retires drained slots on the next flush. The
policy is captured at publisher creation and stays fixed for its lifetime.
The owner's bytes are split three ways that never borrow from each other:

- the final-gauge reserve, `calls × progress_bytes` (128 KiB). Admitting a
  call admits its reservation, so its latest progress, and so its final gauge,
  is never refused for space. Progress stays in its reservation after a write
  and is not charged anywhere else;
- the log budget, half the rest (192 KiB, at most `logs` waiting logs);
- the write budget, the other half (192 KiB, at most `events` records), for
  the batch a store write has in flight.

Each record is charged its fact and call address serialized plus a 256-byte
envelope (`TOOL_LIVE_ENVELOPE_BYTES`: scope, emission identity and payload
tags). A write carries every waiting final first, round robin across calls,
then logs, then omission counts; what does not fit waits for the next
interval, so more finishing calls than one interval carries spread across
intervals. The completion fence seals the publisher and writes its final
progress again, even when an earlier interval carried it.

A call is admitted only with a free slot. A sealed call whose writes are done
keeps its slot for the selected `completed_linger` after its completion
fence or latest finished write, whichever is later. A flush retires a drained
slot when elapsed time is **greater than or equal to** that duration; checks
run on the publisher interval, after any write in flight completes. A new
call can reuse a drained slot earlier. A call that finds every
slot held by a live or unwritten call is refused at admission: it publishes
nothing, and the owner receives an `OwnerOmitted` `RoutesFull` count. That
cause is owner-scoped; a call-scoped `RoutesFull` comes only from the MCP
plugin's own route bound. Publication after the fence counts as
`AfterCompletion` on the call's address for as long as the handle lives: once
its slot retired, a late fact reopens an omission-only slot (or, with none
free, counts as an owner `RoutesFull`). There is no time-based cutoff for
this omission attribution: it stops being call-scoped only when a slot cannot
be admitted. A late fact itself is never accepted after the completion fence,
including during linger. Writes exceeding the selected `write_timeout` use
the ordinary failed-write accounting: logs become `StoreFailed` omissions,
progress is retried, and the tool outcome is unchanged.

`ToolLive::diagnostics(owner)` and `ToolLivePublisher::diagnostics()` report
the owner's live and sealed calls, reserved bytes, waiting progress and logs,
the write in flight, pending omission counts, refusals and failed writes.
`ToolLivePolicy::new` refuses a setting with a `ToolLivePolicyError` naming the
field, the supplied value, and the range or relationship it broke
(`LogsBelowEvents`, `ReserveWithinHalfBytes`, `FactWithinLogBudget`,
`ProgressWithinFact`). MCP correlation has its own `McpRoutePolicy`, defaults
of 128 routes and a two-second grace, and never evicts a live route for a later
registration.

### Loss, and how each kind shows

- **Omitted publication** reaches the subscriber as counts. `Coalesced`: a
  newer gauge replaced one no write had carried. `OwnerBudget`: the log budget
  was full. `Oversized`: a fact over `fact_bytes`, or progress over
  `progress_bytes`. `NonFinite`: a NaN or infinite progress or total, never
  stored. `AfterCompletion` and `RoutesFull` as above. `StoreFailed` counts logs
  a failed or timed-out write (five seconds) lost; progress and earlier counts
  are written again next interval. These counts travel in the next successful
  write, so a store that stays broken cannot deliver the count explaining it;
  diagnostics show `failed_writes` at once.
- **Lost windows** are `Gap` items: `Trimmed` when retention dropped what the
  cursor needed, `Unavailable` when the class has a new incarnation or the
  cursor is past it. The gap's cursor is where the class continues.
- **Broken transport** ends a subscription with a `ToolLiveError`.
  `StoreUnavailable` (with the store's error as source) and `NotificationLost`
  are reconnectable: call `subscribe_after` with the last delivered cursor.
  `CorruptPayload` is an encoding fault, `WrongOwner` a cursor for another
  owner, `Unavailable` a core with no transport for that owner kind. A stored
  record that does not decode is not an error: it is one `Fault` item.

The session and process replay stores retain tool facts in their own event
class: 2048 events, 8 MiB and 120 seconds per owner by default, separately from
turn text and node history. PostgreSQL transports this class through unlogged
tables and each replay store's replica listener; SQLite hosts retain the
single-process window. `subscribe_after(filter, Some(cursor))` resumes within
that class's window. It has no ordering relationship to
session or process observation streams, and is never merged into them.
