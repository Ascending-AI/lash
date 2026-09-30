# VM workers

`WorkerPool::new(PoolConfig::standard(WorkerEntry::helper(path)))` prewarms a
credential-free helper. There is no in-process fallback. A host may instead
register `worker_entry(immutable_host_build_identity)` as its first action and
configure `WorkerEntry::reexec` with that same compiled identity. Register the
entry before creating a runtime, loading credentials, opening stores, or
constructing providers.

The launcher clears the environment. Immediately after exec, the entry closes
all inherited descriptors except its socket. Linux uses `close_range` and
requires kernel 5.9 or newer. Unsupported descriptor adapters fail admission.
The language limits guest authority and the process contains native crashes.
This does not provide OS confinement against a native escape.

`checkout` reserves the complete encoded input size and bounds its wait. A
checkout owns one execution lease. `start` accepts source or stored artifact
bytes and explicit `vm_run` descriptions encoded from `RunContext`. VM state is
opaque in the parent; semantic decoding, linking, compilation, and execution
happen in the worker. Effect requests carry JSON-encoded `AbilityOp`; a value
answer carries JSON-encoded `AbilityOutcome`, and a failure carries
`ExecutionHostError`. Descriptions contain no grants or backing host handles.
The parent broker must authorize requests against its admitted context.

The pool never retries guest execution. On infrastructure failure, it fences
the checkout, kills and reaps the process, then replenishes its minimum.
The broker must settle admitted operations and re-drive the owning substrate
invocation through its real journal. Carry the parent-owned `ExecutionBudget`
through replacement and persist its totals with VM checkpoints. CPU is charged
from process-clock progress and final `wait4` evidence, including a crash before
the final frame. Physical cancellation does not decide the journaled winner.

Release exchanges Reset and ResetDone before reuse. The reset drops the entire
VmInstance. Dropping an unreleased checkout discards it. After an error, a limit,
protocol violation, timeout, or reset failure, the process cannot return to the
idle pool. Repeated failures trip a typed restart storm and wake queued work.

Compute, serialization, and response phases have separate absolute deadlines.
Repeating a phase cannot extend it. The no-response watchdog bounds IPC waits,
bootstrap, and reset; it is not the compute deadline. Parent-owned effect waits
pause execution deadlines. CPU and retry totals remain in the parent budget.

Process-mode `ProcessBoundary` can park through `Checkout::park`, after which
`release` resets the process. The parent can then admit nested compilation on a
one-worker pool and resume the continuation with Start. Parking while an effect
is awaiting its result returns `PendingEffectParkingRequired`. FIG-4159 owns
that pending-effect checkpoint extension. A nested checkout still has a bounded
queue and returns `CheckoutTimedOut` when this seam is unavailable. It never
waits silently forever.

The standard measured FIG-4157 values are min 1/max 4, two queued items/eight
MiB, four MiB frames, two MiB VM state, one MiB effect values, 64 KiB source,
100,000 decoded nodes, 64 MiB charged decode allocation and five seconds IPC
silence. CPU, compute, serialization, retry and restart presets are provisional
until FIG-4162 measures the integrated path.

Effect values use explicit variants and IEEE number bits, preserving undefined,
non-finite numbers, negative zero, tuples and record order. They cannot carry
heap references or live projections. Frames include their 40-byte envelope in
the configured cap, and encoding stops before crossing that allocation bound.
Linux native-process laws run in this lane. macOS has an actual-descriptor
adapter; execution evidence on a macOS runner remains pending. Persistence,
journal replay and live Restate kill points belong to the broker/adapter lanes.

The helper's compiled identity fingerprints its transitive local source, the
locked dependency graph and compiler pin, plus target, debug assertions and its
testing feature. Re-exec hosts supply their own immutable compiled identity.

The native bootstrap lives in `entry.rs`. The core boundary gate allows only
its argv read and empty-environment probe; every other ambient read in the
worker library remains refused.
