# Runtime commit budgets are explicit host policy

## Status

Accepted.

## Context

[ADR 0047](0047-history-is-shared-branches-are-sessions.md) established the
pre-transaction commit guard while also naming 1 MiB and 512 nodes as Lash's
authoritative limits. That combined two decisions which change for different
reasons: Lash owns a backend-independent admission mechanism, while an
embedding host owns the latency and capacity envelope that mechanism enforces.

The required-explicit-bounds idiom already exists for durable Lashlang process
lifetimes in [ADR 0055](0055-lashlang-execution-bounds-span-durable-process-lifetimes.md).
The ownership follows [ADR 0014](0014-operational-policy-stays-with-the-host.md)
and [ADR 0023](0023-retention-stays-a-parameterized-host-lever.md): Lash exposes
and enforces a lever without choosing deployment policy for the host.

## Decision

Every host supplies one required `CommitBudget`. Its byte and node fields each
explicitly choose either a non-zero bound or `Unbounded`; `CommitBudget` has no
`Default`. The host resolves that value once as part of runtime construction,
and every `RuntimeCommit` carries it separately from semantic commit identity.
The facade and the in-memory, SQLite, and PostgreSQL backends all enter the
shared realization path, which validates that same carried value before a
backend transaction starts.

The former 1 MiB byte limit and 512-node limit remain only a documented
recommended starting point. Hosts tune them for their own backend latency and
capacity envelope. Exceeding either dimension is a typed terminal rejection on
turn settlement, public append, and park; retrying the same commit unchanged
cannot succeed, so a host must increase its configured bound or submit a
smaller commit.

What the byte budget measures, and the benchmark that should justify any
recommended sizing, are separate work in FIG-1189. This decision establishes
the mechanism and policy owner, not the measurement composition or the
numbers' justification.

## Consequences

- Missing configuration fails runtime construction instead of silently
  inheriting Lash-owned constants.
- Bounded and intentionally unbounded deployments are both explicit and
  serializable.
- One policy value reaches every host commit surface and every backend; no
  backend may substitute a local limit.
- Changing the recommendation does not change the runtime contract or durable
  semantic commit identity.
