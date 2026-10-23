# Lash VM execution bounds span durable process lifetimes

## Status

Accepted.

## Context

A code mode cell and a durable kernel process need explicit bounds on guest
work and logical memory. A park must preserve those meters, rather than grant
a new allowance when a worker or owner changes.

## Decision

RLM configuration requires independent `InstructionBound` and `MemoryBound`
values and an explicit channel. Both bounds are nonzero or explicitly
`unbounded()`. `ExecutionBounds::new` requires both axes; its charge selection
and memory meter are separate choices. The session records these facts in
its RLM namespace and reuses them on reopen. They are creation configuration,
not a replacement policy supplied at each open.

The worker pool has its own run ceilings. An unbounded protocol choice uses
the pool's ceiling; a finite choice above it is refused before guest work.
The host chooses pool settings explicitly (ADR 0123). There is no additional
VM-host default of 512 MiB.

A process start states `KernelRecordedSettings`: dialect, charge selection,
charge limit, logical memory and its meter, call depth, live tasks, requests
per park, join members and observation mode. Every run reads these settings
from `ProcessRecord::engine_config`; a restart or successor cannot replace
them with current factory defaults. `KernelProcessEngine` implements this
contract. Missing or malformed recorded settings retain typed refusals.

### Heap accounting

Memory uses `AccountingMetric::Capacity`: resident arena values, long payload
storage and control state. It is not allocator RSS. Before refusing, the
machine sweeps unreachable state and recounts what it holds. An installed
limiter may grow the bound; otherwise a refusal is `Bound::Memory`. Cold
resume can meet the bound later as pages become resident, under `K-BND-003`;
values, effects and charge agree before refusal. Page faults, trace limits and
staging failures remain infrastructure failures. Ordinary parks do not
collect. ADR 0145 §5 owns the meter and carry pacing.

### Lifetime and enforcement

The guest charge meter applies to one cell execution or one durable process
run. Checkpoint roots persist the exact meters with the tasks and heap;
resuming does not reset them. Charge bounds real work, including proportional
library and intrinsic work. Call depth, task count, requests per park and join
size have separate bounds. Guest charge does not read a wall-clock deadline;
worker CPU, held wall time and transport deadlines are separate host bounds
(ADR 0123).

A guest bound is a typed terminal result. RLM reports `CELL_BOUND_EXCEEDED`;
a kernel process reports its typed kernel failure. A host-capacity or storage
fault records no guest bound and resumes from committed state.

Formats change in place before 1.0. The registered stored formats still guard
recovery under ADRs 0106 and 0115.

## Consequences

Hosts choose the guest policy and the worker ceilings. A snapshot preserves
cumulative accounting. The memory metric is explicit and follows resident capacity, not
whole-process physical memory. Retrying an unchanged guest-bound
failure cannot make that execution fit.

## Code evidence

- [Required protocol bounds](../../crates/lash-protocol-rlm/src/plugin/config_types.rs).
- [Recorded process settings](../../crates/lash-vm-runtime/src/process.rs).
- [Kernel bounds and meters](../../crates/lash-kernel-vm/src/interface.rs).
- [Persistent census](../../crates/lash-kernel-state/src/paged/census.rs).
- [Worker ceilings](../../crates/lash-vm-protocol/src/bounds.rs).
- [Checkpoint meters](../../crates/lash-kernel-state/src/paged/root.rs).
