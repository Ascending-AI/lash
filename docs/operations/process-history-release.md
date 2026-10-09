# Releasing a running process's event history

FIG-3482 investigates retained history across execution segments. The
post-1.0 sweep authorizes implementing a necessary stored horizon before
the 1.0 cut. This inventory describes lifecycle history under
[ADR 0137](../adr/0137-the-host-owns-events-routing-and-scheduling.md). SQLite file, SQLite memory and PostgreSQL are
storage tiers.

Hosts call `Processes::release_events(process_id, through)` to release an
event prefix's payloads. The process keeps its identity, incarnation,
sequence counter and execution state. This reduces retained payload bytes;
it does not bound every byte a continuously running process retains.

## Growth inventory

These are source-verified behaviors. A segment is not a process terminal and
does not make process-owned rows prune-eligible.

| Record | Growth and readers | Existing cleanup owner | Effect of event release |
| --- | --- | --- | --- |
| Process events | One row per typed lifecycle transition. Replay matching, pages, event awaiters and observation summaries read them. | Terminal process pruning deletes the log. Hosts explicitly release running-process history. | Strips selected payloads; retains ordering, invocation and lifecycle identity fences. |
| Effect-summary events | At most eight individually recorded occurrences per effect node; later occurrences accumulate omission counts. Pending summaries travel in committed execution state until a boundary write. | Runtime incorporation writes events in the next boundary's transaction; terminal pruning owns their rows. | Releases committed payloads. Suffix snapshots report their summary incomplete. Pending summaries stay untouched. |
| Process record, wait, cancellation and outcome | Current state replaces the record; its values can be large. | Lifecycle writes and terminal pruning. | Nothing. Outcomes stay awaitable. Cancellation retains its special replay-matching payload. |
| Child processes, parent-end plans and consumer holds | Each child and retained obligation is independently owned. A captured started-child set may grow. | Existing parent-end settlement, hold release and terminal pruning. | Nothing. Event age cannot prove child work or a consumer obligation dead. |
| Session history, inputs and commit receipts | Child-session turns can retain graph nodes, revisions, receipts and input identity evidence. | Shared reachability, session deletion, vacuum and eligible evidence retention, under ADRs 0047 and 0023. | Nothing. Release neither compacts conversations nor releases history pins. |
| Attachments, artifacts and process environments | Bytes follow live referrers and may outlive the event mentioning them. | Referrer cleanup and attachment GC, under ADRs 0113 and 0124. | Nothing. Releasing JSON does not authorize deletion of referenced bytes. |
| Live program and Run state | VM stacks, values, aggregate prefixes, unseated finals, source seals, material, state frontier, capacity and owed starts/cancels survive takeover. | Run consumption, protected drain, dependency release and identity-fenced retirement under ADRs 0065 and 0099. | Nothing. Age cannot discard live state; Closing is not garbage collection. |

Evidence: [event SQL](../../crates/lash-store-sql/src/process/events.rs),
[append and replay matching](../../crates/lash-core-execution/src/runtime/process/validation.rs),
[effect accounting](../../crates/lash-core-execution/src/runtime/process/effect_summary.rs),
[snapshot SQL](../../crates/lash-store-sql/src/durable/snapshots.rs),
[outbox and retention contracts](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs),
[SQLite pruning](../../crates/lash-sqlite-store/src/process_registry_change.rs),
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

Each row keeps its sequence, typed lifecycle kind, key, invocation
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
diagnostic timestamp fallback. Call identity stays unchanged.

The schema adds `process_events.released_payload_digest` and
`process_event_horizons(process_id, released_through)`, with PostgreSQL's
`lash_` prefixes. The positive horizon is process-owned. Both backends cascade
its deletion at process pruning. Versions stay frozen and fixtures change in
place.

Evidence: [release types and digest](../../crates/lash-core-execution/src/runtime/process/events.rs),
[SQLite release](../../crates/lash-sqlite-store/src/process_registry/event_release.rs),
[PostgreSQL release](../../crates/lash-postgres-store/src/postgres/process_registry/event_release.rs),
[host API](../../crates/lash/src/process_admin.rs),
[snapshot fold](../../crates/lash/src/process_feed.rs),
[event awaits](../../crates/lash-core-execution/src/runtime/work/awaiter.rs) and
[process wait projection](../../crates/lash-core-execution/src/runtime/process/validation.rs).

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
admission, pending summaries, Run aggregates and source seals, cancellation
state, parked calls, process work and holds are outside release. Recorded
engine steps read their committed answers. A resumed phase retrying an
uncommitted append still has its digest fence. Product records and notice
delivery belong to the host under ADR 0137 and are outside this event log. Hosts
submit work through `send()` and the engine; release executes no turn.

Deleting rows would require proof that every writer stopped replaying its
keys. Storage has neither proof.
No TTL, registration retention field, automatic cleaner, successor process
or `ContinuedAsNew` terminal is added.

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

Released lifecycle metadata survives while the process is retained. Live
program state and child session history follow their own contracts. A total
storage bound needs new proof about dead lifecycle identities and live-state retention. Today's store contract supplies none.

## Validation cases

The [store law](../../crates/lash-conformance/src/conformance/process_registry/event_release.rs)
checks typed expiry, tails, equal-key and conflicting retry,
monotonic allocation, clamping and repetition on all three storage tiers.
The [facade law](../../crates/lash/src/process_history/tests.rs) checks the
read gap, continuation and incomplete fold.

Transaction serialization and rollback above are source reasoning. Crash-reopen
release transactions, concurrent append/release stress and a many-segment live-retention benchmark remain useful cases.
They were not added to this unit's minimal gate. The task report distinguishes
executed tests from these cases.
