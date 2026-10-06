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
the rows an adoption update stamps. A same-turn-ID retry can adopt still-open
manifest rows from an earlier attempt that this attempt did not count, causing
an undercount. Cancelled or failed puts and committed explicit IDs can cause an
overcount. The node bound describes the recorded count, not an exact guarantee
about every adoption row a retry can stamp.

Exceeding either configured dimension produces a typed commit rejection.
Retrying identical payload and policy cannot admit it. The host must raise the
bound or reduce the payload. The budget applies to turn settlement, append and
park commits.

### Live heads

Every commit carries its head's session configuration and checkpoint
manifest, so a head whose bare commit exceeds the budget refuses every write to
its session, a session command's settlement included. Creation is the one head
write outside a runtime commit, and it measures that commit:
`admit_created_session` sizes the bare commit over the created head, a session
command's settlement with the session's initial frame, and refuses a config
that does not fit with the typed budget rejection, writing nothing. The sizing
probe uses a fixed initial-frame timestamp with all nine fractional digits,
so the admission threshold does not vary with wall-clock precision. This
synthetic timestamp is never persisted. Every later head write is a budgeted
commit.

A command whose commit exceeds the budget settles failed with the budget
rejection, over the durable head. That settlement is the head's bare commit
plus its refusal receipt, and the budget does not charge a failed settlement's
receipt whose message stays within 512 bytes (every budget rejection does), so
a failed settlement fits whenever the head's bare commit does. Every other
command outcome is charged.

The budget is host policy, not durable state, so the host must not set it
below its live heads. A host that does strands each such session's leading
session command: the shift that meets it refuses the command run with the
typed budget rejection and stops, without admitting the command again, and the
command stays open and unsettled. Raising the budget recovers the session: its
next shift applies the command and settles it.

Attachment admission is separately configurable through
`max_attachment_bytes: Option<u64>`. `None` leaves puts unbounded;
`Some(max_bytes)` rejects an oversized in-memory attachment before manifest or
backend work. This is a separate payload limit, not a third commit dimension.

### Attachment read and materialization budgets

`AttachmentReadPolicy` is independent of put admission and history retention.
The default is 32 MiB per blob and 128 MiB per request; hosts can configure it
through `LashCoreBuilder::attachment_read_policy`. Session rebuilds, backend
replacement and process runtimes keep the configured policy.

`AttachmentStore::get` requires an actual-byte limit. File reads use bounded
scratch buffers; S3 consumes chunks and checks before copying them into retained
storage. Buffer growth is geometric and capped by the read limit. Neither relies on reported object size. SQLite returns the actual
length and conditionally projects the BLOB only when it fits, within one query.

Before provider dispatch, resolution charges each unique retained blob buffer capacity once
and every attachment occurrence for encoding. Inline and pre-resolved bytes
are subject to the same budget. Each occurrence reserves four base64-sized
copies plus JSON envelope and escaped MIME/label bytes. URL and provider-file
strings also reserve escaped copies. Repeated IDs share retained bytes while
each provider occurrence still has its encoding charge. The remaining budget,
including expansion, determines the limit passed into each backend read.

These are attachment payload-work bounds, not a claim about whole-process RSS,
allocator bookkeeping, a backend's network chunk, or non-attachment prompt
content. A refusal settles as `AttachmentResolutionFailed` before provider
dispatch. Read refusal does not change retained history or attachment ownership.

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
- Retry adoption residuals remain visible; the recorded node count does not
  silently claim an exact transactional census.

## Code evidence

- [Required policy and row measurement](../../crates/lash-core-store/src/store/commit_budget.rs#L24).
- [Logical-byte measurement](../../crates/lash-core-store/src/store/commit_budget.rs#L290).
- [Created-head measurement](../../crates/lash-core-store/src/store/commit_budget.rs#L246).
- [Measured creation](../../crates/lash-core-store/src/store/catalog.rs#L91).
- [A stranded command stops its shift](../../crates/lash-core-execution/src/engine/shift.rs#L224).
- [Verified realization](../../crates/lash-core-store/src/store/realization.rs#L8).
- [Host attachment configuration](../../crates/lash/src/core.rs#L841).
- [Physical-operation timing](../../crates/lash-perf/src/runtime_perf/store.rs#L1).
