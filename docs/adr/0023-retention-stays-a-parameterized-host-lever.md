# Retention stays a parameterized host lever

Hosts need differentiated retention — ephemeral debris (subagent turns, fan-out helpers) pruned
aggressively, long-lived processes kept until the host's own projection has durably consumed
them. We considered a producer-declared retention class on `ProcessRegistration` (shaped like
Recovery Disposition) and rejected it: retention is operational policy, not a correctness
contract, and ADR 0014/0017 already place operational policy with the host. Instead
`prune_terminal_processes` gains two optional parameters: a process filter (the enriched
`ProcessListFilter` — originator scope, identity kind/label, caused-by, created-at range) and
an `up_to_change_seq` bound tied to the Process Change Cursor (ADR 0020), so a host can express
"prune terminal subagent processes after 24h" and "prune terminal host-scope processes after
90 days, but never past my projector's acknowledged cursor" as two scheduled calls.

No schema change beyond ADR 0020's change sequence; no declared class field on any backend; a
producer (including lash-owned spawn paths) never guesses a policy the host owns. The
watermark bound is what makes host projection safe: without it, a host that projects process
history can silently destroy unprojected evidence, and the failure only surfaces as
"unknown process" much later.

## Durable-core amendment

ADR 0047 extends the same rule from terminal processes to every reclaim primitive.
`vacuum`, receipt pruning, effect-journal pruning, and attachment/blob reclamation
all take an explicit host-supplied `RetentionBound`; none infers a horizon or runs
as an internal background policy. Where evidence participates in retry,
idempotency, or host projection, age is only a bound after the owning scope is
terminal and after the relevant revision, epoch, or change-sequence watermark.

The producer still does not declare a retention class. Lash defines eligibility
and the Host Application chooses how much eligible evidence to retain.
