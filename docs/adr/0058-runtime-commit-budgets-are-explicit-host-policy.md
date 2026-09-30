# Runtime commit budgets are explicit host policy

## Status

Accepted.

## Context

Lash owns backend-independent commit admission. The host owns the latency and
capacity envelope that admission enforces. ADR 0014 and ADR 0023 give the host
operational policy; ADR 0055 applies the same explicit-bounds rule to VM work.

## Decision

Every host supplies a required `CommitBudget`. Both byte and node dimensions
choose a nonzero bound or `Unbounded`. `CommitBudget` has no `Default`.

Bytes cover the logical persisted payload carried by a `RuntimeCommit`:
session configuration, graph delta, checkpoint manifest and changed component
bodies, attachment reference IDs, follow-on work, current frame reference,
usage deltas and durable turn result. The node dimension counts graph-node
rows plus the attempt's recorded attachment-intent adoption rows. Attachment
body bytes are outside the commit byte budget.

Runtime construction resolves the budget once. The commit carries it separately
from semantic commit identity. The facade realization boundary validates the
session binding and budget before delegating to the store; the SQL stores use
the shared core planner. Budget measurement consumes the commit payload rather
than querying the store for adoption counts. No backend substitutes a local
budget.

### Adoption accounting

The `adopted_intent_rows` count is attempt-recorded evidence. It can differ from
the rows an adoption update stamps. A same-turn-ID replay can adopt still-open
manifest rows from an earlier attempt that this attempt did not count, causing
an undercount. Cancelled or failed puts and committed explicit IDs can cause an
overcount. The node bound describes the recorded count, not an exact guarantee
about every adoption row a replay can stamp.

Exceeding either configured dimension produces a typed commit rejection.
Retrying identical payload and policy cannot admit it. The host must raise the
bound or reduce the payload. The budget applies to turn settlement, append and
park commits.

Attachment admission is separately configurable through
`max_attachment_bytes: Option<u64>`. `None` leaves puts unbounded;
`Some(max_bytes)` rejects an oversized in-memory attachment before manifest or
backend work. This is a separate payload limit, not a third commit dimension.

### Reference sizing curve

A 1 MiB logical-byte bound and a 512-row bound are documented starting points,
not runtime defaults or physical-latency guarantees. Hosts measure their own
byte and row curves, including the joint configured point, before selecting a
latency target. Logical size and physical cost are separate quantities.

The runtime performance decorator times the delegated store operation,
including its connection acquisition, dispatch and backend I/O. It records
logical commit sizing outside that timer. A byte curve does not prove a row
curve, and separate axis measurements do not prove a joint margin.

## Why

A backend-specific constant would turn operational policy into a library
contract and could disagree across stores. Carrying one explicit policy with
the commit keeps admission portable while letting each deployment choose its
capacity envelope. Keeping policy outside semantic identity allows policy
changes without changing what the commit means.

## Consequences

- Missing policy fails construction instead of inheriting library constants.
- Bounded and explicitly unbounded deployments are both serializable.
- One carried policy reaches every commit entry and backend.
- Hosts tune logical bytes and recorded rows against measured physical cost.
- Replay adoption residuals remain visible; the recorded node count does not
  silently claim an exact transactional census.

## Code evidence

- [Required policy and row measurement](../../crates/lash-core-store/src/store/commit_budget.rs#L24).
- [Logical-byte measurement](../../crates/lash-core-store/src/store/commit_budget.rs#L252).
- [Verified realization](../../crates/lash-core-store/src/store/realization.rs#L8).
- [Host attachment configuration](../../crates/lash/src/core.rs#L841).
- [Physical-operation timing](../../crates/lash-perf/src/runtime_perf/store.rs#L1).
