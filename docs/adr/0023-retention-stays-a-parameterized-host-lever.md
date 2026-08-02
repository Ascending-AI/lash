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

No schema change beyond ADR 0020's change sequence; no declared class field on any backend; a
producer (including lash-owned spawn paths) never guesses a policy the host owns. The
watermark bound is what makes host projection safe: without it, a host that projects process
history can silently destroy unprojected evidence, and the failure only surfaces as
"unknown process" much later.

## Durable-core ruling (partially implemented)

ADR 0047 extends the same rule from terminal processes to every reclaim
primitive. The delayed failure above generalizes: reclaiming retry, idempotency,
or projection evidence without the host's watermark can silently destroy
unconsumed proof and surface much later as a different error.

The remaining ruling is that `vacuum`, receipt pruning, and attachment/blob
reclamation will all take an explicit host-supplied `RetentionBound`; none will
infer a horizon or run as an internal background policy. This is not fully
shipped behavior: `vacuum()` currently takes no bound, runtime commit receipts
have no pruning surface, and attachment liveness still depends on manifest
rows and receipt predicates rather than explicit stored edges. The FIG-653 L7
retention work owns that implementation.

Effect-journal retention is implemented in the lifecycle form recorded by ADR
0025. It does not take a second horizon: deleting a session retires that exact
single-use session id, while host-scheduled terminal-process retention retires
the exact canonical process scope before pruning its row. Restate retains its
native invocation journal under its native policy and creates no SQL replay
rows. Age remains a bound only after the owning scope is terminal and after
the relevant revision, epoch, or change-sequence watermark.

The producer still does not declare a retention class. Lash defines eligibility
and the Host Application chooses how much eligible evidence to retain.

Trigger mutation receipts follow the same ownership rule. The public
`LashCore::triggers().prune_mutation_receipts(cutoff_epoch_ms)` lever makes receipt
reclamation reachable without coupling it to process retention; the host supplies the bound and
schedules it. FIG-653 still owns evolving this raw cutoff into the shared `RetentionBound`
contract.
