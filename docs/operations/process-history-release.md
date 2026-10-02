# Releasing a running process's event history

FIG-3482 investigates retained history across execution segments. The
post-1.0 sweep authorizes implementing a necessary stored horizon before
the 1.0 cut. This inventory was checked against main `f9dfed0c61` and the
release implementation. Restate is the only shipping engine. SQLite file,
SQLite memory and PostgreSQL are storage tiers.

Hosts call `Processes::release_events(process_id, through)` to release an
event prefix's payloads. The process keeps its identity, incarnation,
sequence counter and execution state. This reduces retained payload bytes;
it does not bound every byte a continuously running process retains.

## Growth inventory

These are source-verified behaviors. A segment is not a process terminal and
does not make process-owned rows prune-eligible.

| Record | Growth and readers | Existing cleanup owner | Effect of event release |
| --- | --- | --- | --- |
| Process events | One row per producer emit, signal or lifecycle transition. Replay matching, pages, event awaiters and observation summaries read them. | Terminal process pruning deletes the log. A running process previously had no release lever. | Strips selected payloads; retains ordering, invocation, semantics, admitted signal binding and replay fences. |
| Effect-summary events | At most eight individually recorded occurrences per effect node; later occurrences accumulate omission counts. Pending summaries travel in segment state until a boundary write. Producer emits and signals have no such cap. | Runtime incorporation writes events in the next boundary's transaction; terminal pruning owns their rows. | Releases committed payloads. Suffix snapshots report their summary incomplete. Pending summaries stay untouched. |
| Segment handovers and start markers | Each successor records a continuation, route, generation and execution-start fence. Normal retirement removes consumed handovers. Rows can coexist while retirement is owed or fails. | Engine-owned journaled resume and retirement; terminal pruning removes survivors. | Nothing. The successor continuation and start marker remain replay authority. |
| Restate invocation journals and results | Segment budgets bound completed effect count, not arbitrary result bytes or the number of completed segments. Tool children have their own invocations. | Restate completion retention; active execution retains its journal. | Nothing. SQL cleanup cannot replace engine retention. |
| Wake delivery rows | A wake-producing event adds a row with its own content. Claim, enqueue and discard update it. Settled rows stay process-owned; the outbox contract has no running-process deletion. | Process deletion cascades the rows. Owed wakes prevent terminal pruning; discarded wakes permit explicit redrive. | Nothing. Release preserves content and claims, including pending work. These rows can also accumulate during a long process. |
| Wake allocation and receiver floors | One monotonic floor per process and target session, independent of queue-row lifetime. | Session/process-state cleanup and receiver terminal transitions. | Nothing. Event sequences and floors never reset. |
| Process record, wait, cancellation and outcome | Current state replaces the record; its values can be large. | Lifecycle writes and terminal pruning. | Nothing. Outcomes stay awaitable. Cancellation retains its special replay-matching payload. |
| Child processes, parent-end plans and consumer holds | Each child and retained obligation is independently owned. A captured started-child set may grow. | Existing parent-end settlement, hold release and terminal pruning. | Nothing. Event age cannot prove child work or a consumer obligation dead. |
| Triggers and mutation receipts | Deliveries follow their process; receipts fence retries. Occurrence accounting has its own terminal frontier. | Trigger reconciliation and explicit eligible receipt/tombstone maintenance, under ADRs 0021 and 0067. | Nothing. The process id remains stable, preserving delivery linkage. |
| Session history, inputs and commit receipts | Child-session turns can retain graph nodes, revisions, receipts and input identity evidence. | Shared reachability, session deletion, vacuum and eligible evidence retention, under ADRs 0047 and 0023. | Nothing. Release neither compacts conversations nor releases history pins. |
| Attachments, artifacts and process environments | Bytes follow live referrers and may outlive the event mentioning them. | Referrer cleanup and attachment GC, under ADRs 0113 and 0124. | Nothing. Releasing JSON does not authorize deletion of referenced bytes. |
| Live program state and effect groups | VM stacks, retained values, child ids and outstanding group handles cross segments; they can grow by program choice. | Program consumption and group recovery/consumer contracts, under ADRs 0025 and 0099. | Nothing. Age cannot discard live state. |

Evidence: [event SQL](../../crates/lash-store-sql/src/process/events.rs),
[append and replay matching](../../crates/lash-core-execution/src/runtime/process/validation.rs),
[effect accounting](../../crates/lash-core-execution/src/runtime/process/effect_summary.rs),
[workflow handover](../../crates/lash-restate/src/process/workflow.rs),
[handover SQL](../../crates/lash-store-sql/src/process/segment_handovers.rs),
[wake SQL](../../crates/lash-store-sql/src/process/wake_deliveries.rs),
[outbox and retention contracts](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs),
[SQLite pruning](../../crates/lash-sqlite-store/src/process_registry_change.rs),
[trigger maintenance](../../crates/lash-core-execution/src/triggers.rs) and
[row ownership](../adr/0067-durable-rows-name-one-owner-and-one-reclaim-trigger.md).

## Exact release contract

Let `H` be the previous horizon, `L` the record's last event sequence and
`T` the host's requested sequence. The committed horizon is
`max(H, min(T, L))`. A lower or repeated request releases zero new events.
An unknown process refuses `ProcessUnknown`; a pruned process refuses
`ProcessNoLongerRetained`. Zero releases nothing when no prefix was released.

The store rewrites newly selected rows and raises the horizon in one
transaction. Pages of 256 bound each page allocation, not total transaction
duration or payload bytes. Hosts can select smaller incremental horizons.
`released_events` counts rows crossing the horizon, including the cancellation
event whose special payload stays. It does not count reclaimed bytes.

Each row keeps its sequence, type, key, invocation, signal binding, semantics
and timestamp. SHA-256 over the existing canonical JSON identity leaf replaces
the payload. An equal-key retry supplies and verifies that payload and gets
the recorded metadata with it. Changed type or payload conflicts. Cancellation
compares cancellation identity rather than payload bytes, so its payload stays.
There is no second history ledger or process lifecycle.

Full and Lite pages starting strictly below the horizon return
`NoLongerRetained(Released { released_through })`, never a silently shortened
page. Lite still selects only sequence and event type in SQL. Reads after the
horizon keep their ordering; recent tails exclude released rows. Facade cursors
resume at the reported horizon. Required event awaits refuse typed
`ProcessEventsReleased`. Snapshots fold the retained suffix and report
`HistoryReleased`. The optional wait-start timestamp lookup first uses the
stored current wait, then searches the retained suffix, with its existing
diagnostic timestamp fallback. Signal identity and ordinal stay unchanged.

The schema adds `process_events.released_payload_digest` and
`process_event_horizons(process_id, released_through)`, with PostgreSQL's
`lash_` prefixes. The positive horizon is process-owned. Both backends cascade
its deletion at process pruning. Versions stay frozen and fixtures change in
place.

Evidence: [release types and digest](../../crates/lash-core-execution/src/runtime/process/events.rs),
[SQLite release](../../crates/lash-sqlite-store/src/process_registry/event_release.rs),
[PostgreSQL release](../../crates/lash-postgres-store/src/postgres/process_registry/event_release.rs),
[host API](../../crates/lash/src/process_admin.rs),
[snapshot fold](../../crates/lash/src/process_observation.rs),
[event awaits](../../crates/lash-core-execution/src/runtime/work/awaiter.rs) and
[signal-wait timestamp](../../crates/lash-lashlang-runtime/src/process.rs).

## Safety and ownership

SQLite serializes the write transaction. PostgreSQL locks the same process
row as append before reading the high-water mark and horizon. An append either
precedes release and belongs to its prefix, or follows it with a higher
sequence. Transaction failure publishes neither partially stripped history
nor a horizon. Retrying an uncertain commit observes the committed horizon or
performs the release once. Concurrent releases cannot lower it.

The host selects a horizon after acknowledging every projection it needs.
Lash supplies no maximum lifetime for a host event reader. Release deliberately
expires readers needing that prefix; the cursor reports the loss explicitly.
Terminal pruning's projection watermark keeps its separate contract.

Lash protects execution dependencies by preserving them. Continuations,
admission, pending summaries, effect groups, cancellation state, signal
promises, child work, trigger linkage, holds and wake content are outside
release. Counts include released signal rows, so a successor cannot reuse an
ordinal. Recorded engine steps replay their recorded answers. An old segment
or tool child retrying an unrecorded append still has its digest fence. Hosts
submit work through `send()` and the engine; release executes no turn.

Deleting rows would require proof that every writer stopped replaying its
keys and replacement signal-ordinal evidence. Storage has neither proof.
No TTL, registration retention field, automatic cleaner, successor process
or `ContinuedAsNew` terminal is added.

## Restate retention configuration

[The live-server recipe](../../scripts/ci/restate_suite.py) pins Restate
1.7.12. Its [pinned configuration defaults](https://github.com/restatedev/restate/blob/v1.7.12/crates/types/src/config/invocation.rs#L127)
set journal, idempotency and workflow completion retention to 24 hours.
These are source-verified defaults, not deployed-server measurements.

[Lash service binding](../../crates/lash-restate/src/services.rs) sets authority,
lazy-state and retry options, but no retention override. The pinned Rust SDK
0.11.1 leaves retention options absent until configured. Hosts must inspect
their namespace-qualified services and handlers because server defaults and
existing overrides can differ. The [pinned service contract](https://github.com/restatedev/restate/blob/v1.7.12/crates/types/src/schema/service.rs#L121)
caps journal retention by workflow completion retention for workflows and
idempotency retention for keyed requests.

Workflow retention starts after the run completes and controls later state
and promise access, as [Restate's service documentation](https://docs.restate.dev/services/configuration#retention-of-completed-invocations)
describes. It cannot reclaim a running or paused segment's journal. Hosts
account for late attachments and requests before reducing it. Lash's recorded
start contract refuses started work whose required journal is gone instead
of repeating effects with an empty journal. Event release changes none of this.

Live expiry timing, physical disk reclamation, deployed overrides and late
requests after expiry were not exercised. The server double supplies no
evidence about live Restate retention.

## Bounded measurement

The previous worker measured 2,000 replay-keyed producer events on SQLite
memory for each payload size. Its retained receipt is
`.buck2/test-invocations/20261002T194035-n_nq3lwj/test-report.json`.
Each release reported 2,000 events; its repeat reported zero. The temporary
measurement test was removed after collecting the receipt.

| Payload string bytes per event | Before: JSON plus key bytes | After: JSON plus digest plus key bytes | Rows before and after |
| --- | ---: | ---: | ---: |
| 16 | 1,128,456 | 1,185,566 | 2,000 |
| 256 | 1,608,456 | 1,185,566 | 2,000 |
| 4,096 | 9,288,456 | 1,185,566 | 2,000 |

These sum stored string lengths and exclude pages, indexes, WAL, allocator
overhead and referenced bytes. Small payloads can cost more after release
because of the digest. Large payloads fall to about 593 bytes per event in
this fixture. This is an event-log reproduction, not a many-segment engine
storage benchmark. It proves payload reclamation and retained fence growth,
not bounded total storage.

Released metadata and wake rows survive while the process is retained. Live
program state and child session history follow their own contracts. A total
storage bound needs new proof about dead replay keys, signal-count preservation
and settled-wake redrive eligibility. Today's store contract supplies none.

## Validation cases

The [store law](../../crates/lash-conformance/src/conformance/process_registry/event_release.rs)
checks typed expiry, tails, equal-key and conflicting replay, signal counts,
monotonic allocation, clamping and repetition on all three storage tiers.
The [facade law](../../crates/lash/src/process_observation/tests.rs) checks the
read gap, cursor and incomplete fold. The [handover law](../../crates/lash-restate/src/tests/segment_generation_handoff/crash_cuts.rs)
releases at a stored-but-unjournaled handover, crashes that step, forces engine
replay and checks the continuation, outcome and one successor.

Transaction serialization and rollback above are source reasoning. Crash-reopen
release transactions, concurrent append/release stress, pending-wake redrive
after release and a many-segment live-retention benchmark remain useful cases.
They were not added to this unit's minimal gate. The task report distinguishes
executed tests from these cases.
