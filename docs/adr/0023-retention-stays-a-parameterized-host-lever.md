# Retention stays a parameterized host lever

Hosts need differentiated retention — ephemeral debris (subagent turns, fan-out helpers) pruned
aggressively, long-lived processes kept until the host's own projection has durably consumed
them. We considered a producer-declared retention class on `ProcessRegistration` (shaped like
Recovery Disposition) and rejected it: retention is operational policy, not a correctness
contract, and ADR 0014/0017 already place operational policy with the host. Instead
`prune_terminal_processes` takes an optional process filter (the enriched
`ProcessListFilter` — originator scope, identity kind/label, caused-by, created-at range) and a
required `ProjectionWatermark::{UpTo(cursor),NoProjector}` choice tied to the Process Change
Cursor (ADR 0020), so a host can express
"prune terminal subagent processes after 24h" and "prune terminal host-scope processes after
90 days, but never past my projector's acknowledged cursor" as two scheduled calls.

The public `Processes::prune` lever forwards that filter, so differentiated policy is
expressible without dropping to the raw registry: `agent-workbench` prunes the terminal rows
an `originator_id` scope owns when it deletes that session, because deleting a session detaches
its state from globally-owned process rows without deleting them. Because retention only ever
deletes terminal rows, the facade refuses a filter selecting a non-terminal status — including
the `Running` default a `..Default::default()` filter carries — instead of accepting a filter
that can only ever reclaim nothing.

No schema change beyond ADR 0020's change sequence; no declared class field on any backend; a
producer (including lash-owned spawn paths) never guesses a policy the host owns. The
watermark bound is what makes host projection safe: without it, a host that projects process
history can silently destroy unprojected evidence, and the failure only surfaces as
"unknown process" much later.

## Durable-core evidence retention (FIG-2502 / FIG-653)

`SessionStoreFactory::reclaim_retained_evidence(RetentionBound)` is the explicit,
factory-wide host lever. The bound is an exclusive commit timestamp horizon;
the operation consults neither a clock nor live configuration. No internal
schedule runs it. A runtime receipt is eligible only if its owner is durably in
`deleted_sessions` and its stored commit timestamp is before that horizon.
Session deletion now retains these receipts for this lever instead of erasing
them implicitly. `deleted_sessions` is permanently exempt: FIG-754 / FIG-748
require identity evidence after every other row disappears. Scope fences
(`effect_scope_retirements`) are the other permanent-row class; ADR 0049 owns
their release rule.

Usage deltas use the same terminal-session gate. They remain while their
matching operation receipt remains. Live deltas reconstruct the token ledger
on resume, so a host bound alone must not silently change live accounting.
The gate is intentionally stricter than an age-only audit-row policy.

The sweep has two phases in one fenced transaction: first delete eligible
receipt roots; then reconcile terminal usage by a correlated `NOT EXISTS`
against remaining receipts, and reconcile deleted-owner attachment manifests.
SQLite holds `BEGIN IMMEDIATE`; PostgreSQL holds one cross-worker advisory
transaction lock; the memory factory holds its shared write transaction.
Counts describe committed work. An error rolls the complete operation back;
repetition returns zero after the eligible set has been exhausted. There are
no partial batches or uncommitted watermarks.

Attachment GC still uses later-turn receipts for live-session supersession,
but those receipts are never pruned. All four SQL oracle sites also recognize
the permanent deleted-session marker, so a terminal intent cannot become live
again when its positive receipt witness disappears. Committed attachment rows
are instead protected by the graph-retention precondition from FIG-2501:
a surviving fork or pin keeps bytes independently of receipt retention.
Receipt deletion therefore cannot cause even a conservative orphan leak.

The retry outcome is `StoreError::SessionDeleted`, both before and after a
terminal receipt is pruned. A live receipt always survives, so a legitimate
live retry still replays its original result and can never become the FIG-853
"someone else committed" conflict because of this lever.

`vacuum()` remains a bound-free cleanup of already-tombstoned graph and terminal
input rows. Attachment byte GC retains its existing explicit reclamation policy.
Neither uses the new receipt horizon. Exact node-to-attachment edges remain a
possible granularity improvement, not a correctness prerequisite or an unbuilt
part of this shipped lever. See ADR 0028 and ADR 0047.

Effect-journal retention is implemented in the lifecycle form recorded by ADR
0025. It does not take a second horizon: deleting a session retires that exact
single-use session id, while host-scheduled terminal-process retention retires
the exact canonical process scope before pruning its row. Restate retains its
native invocation journal under its native policy and creates no SQL replay
rows. Age remains a bound only after the owning scope is terminal and after
the relevant revision, epoch, or change-sequence watermark.

The producer still does not declare a retention class. Lash defines eligibility
and the Host Application chooses how much eligible evidence to retain.

Trigger mutation receipts follow the same ownership rule, but age alone does not establish that an
operation id can never be retried. Lash therefore deliberately exposes no public receipt-pruning
facade and has no production caller or maintenance schedule for the low-level
`TriggerStore::prune_mutation_receipts` primitive. The low-rate receipt table remains unbounded in
the safe interim: retaining idempotency evidence is preferable to re-evaluating a live retry with a
changed disposition. This trigger-receipt primitive is outside the FIG-2502 runtime-receipt/usage
lever; its host/platform terminal-scope design remains unshipped. The new
retention census records that deliberate permanent retention explicitly.
