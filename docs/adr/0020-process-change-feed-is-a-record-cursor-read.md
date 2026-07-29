# Process change feed is a record-level cursor read

Hosts that project process state into their own stores need a completeness lane: a way to find
every Runtime Process that changed since a watermark, including terminal transitions, which the
`ProcessEventSink` deliberately never carries (ADR 0017). We add a **Process Change Feed** to
`ProcessRegistry`: every process-row mutation (registration, event append, wait, status,
terminal write) bumps a per-store monotonic change sequence, and a cursor-paged
`processes_changed_since(cursor, limit)` read returns the changed `ProcessRecord`s in that
order with the next cursor. Consumers needing event detail re-read `events_after` per changed
process.

We chose a record-level cursor over a store-wide global event sequence: it keeps truth a state
read (ADR 0017's split stays intact — sink for freshness, state reads for completeness), costs
one column plus an index per backend instead of a total-order obligation on `process_events`
forever, and terminal transitions need no special casing because they are ordinary record
changes. A durable push sink was rejected outright — ADR 0017 already records why that
re-creates the durable log badly.

## Consequences

- Every store backend gains a `change_seq` (monotonic per store, not per process) on process
  rows; the cursor is opaque to consumers and not comparable across stores.
- The feed is a host-level, unscoped read for trusted projectors. App-facing visibility uses
  session-scoped observer edges; the feed does not filter by observer.
- The change cursor is the natural watermark for retention: a host can gate
  `prune_terminal_processes` on its projector's acknowledged cursor so unprojected history is
  never pruned.
