# VM workers

`lash_vm_client::WorkerPool::new(PoolConfig::standard(WorkerEntry::helper(path)))` prewarms a
credential-free helper. There is no in-process fallback. A host may instead
register `worker_entry()` as its first action and
configure `WorkerEntry::reexec()`. Register the
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
happen in the worker. Effect requests carry typed MessagePack `AbilityOp`; a value
answer carries heapless MessagePack `AbilityOutcome`, and a failure carries
`ExecutionHostError`. Descriptions contain no grants or backing host handles.
The parent broker must authorize requests against its admitted context.

The pool never retries guest execution. On infrastructure failure, it fences
the checkout, kills and reaps the process, then replenishes its minimum.
The broker must settle admitted operations and re-drive the owning substrate
invocation through its real journal. Carry the parent-owned `ExecutionBudget`
through replacement. The backend's recovery store preserves known CPU,
consumed attempts and unknown CPU attempts across substrate redrive, independently
of positional effect journals. CPU is charged
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

`Checkout::park` parks the run on its pending request: a process-mode
`ProcessBoundary`, or an effect the run can issue again (a resource operation,
a sleep or a signal wait, FIG-4159). The worker serializes its VM, and
`release` resets the process, so the parent can admit nested compilation on a
one-worker pool and then resume the continuation with `Start`; a run parked on
an effect issues that request again. A run that cannot be captured where it
stands declines with `ParkDeclined` and, once that is answered, issues its
request again on the same checkout. A nested checkout still has a bounded queue
and returns `CheckoutTimedOut` when no slot frees. It never waits silently
forever.

The standard presets retained after FIG-4162 are min 1/max 4, two queued items/eight
MiB, four MiB frames, two MiB VM state, one MiB effect values, 64 KiB source,
100,000 decoded nodes, 64 MiB charged decode allocation and five seconds IPC
silence. Checkout is five seconds, compute thirty seconds, serialization five
seconds, cancellation grace 100 ms and cumulative CPU ten seconds. An invocation
allows three attempts; the pool permits eight failed replacements per minute.
These are host bounds, not latency targets or guarantees for arbitrary guests.
The optimized matrix and its sampling contract live in
`crates/lash-perf/src/vm_worker_matrix/`; ADR 0123 records the measured result.

The shipped RLM/process service currently allows 64 MiB VM state, 128 MiB
frames, 256 MiB charged decode allocation and 128 MiB queued input. The existing
process and conformance fixtures exceed the pool's smaller baseline presets.
This larger compatibility profile stays explicit: the small synthetic matrix
does not justify rejecting existing valid process state. Hosts can supply a
smaller `PoolConfig` for their own admitted workload.

Effect values use explicit variants and IEEE number bits, preserving undefined,
non-finite numbers, negative zero, tuples and record order. Projection identities use parent-owned namespaces and keys; they carry no
backing host handles. Frames include their eight-byte envelope in
the configured cap, and encoding stops before crossing that allocation bound.
Linux native-process laws run in this lane. macOS has an actual-descriptor
adapter; execution evidence on a macOS runner remains pending. Persistence,
journal replay and live Restate kill points belong to the broker/adapter lanes.

`lash-vm-protocol` defines `WORKER_PROTOCOL_VERSION` and
`MIN_SUPPORTED_WORKER_PROTOCOL_VERSION` once for both sides. Pool admission
checks the worker's `Ready` message before guest work. An out-of-range version
returns `PoolError::ProtocolVersion` with both protocol versions, the supported
range and both crate versions. Crate versions are diagnostic only. The wire
version stays at 1 until 1.0. The `synthetic-next` acceptance feature moves both
ends of the supported range to 2, so N and N+1 cannot pair at admission; reviewed shape changes refresh the committed
`lash-vm-client/tests/snapshots/wire-v1*.snap` snapshots in place. After 1.0,
the repository gate requires snapshot changes to bump the protocol version and retain the old
versioned snapshot. The syntax-derived snapshot includes every service request
and response variant, field type and Serde attribute, under both feature selections.

Hosts select `WorkerEntry::helper(path)` or `Service::subprocess(path)` explicitly.
The SDK's `Service::default()` uses the documented `lash-vm-worker` executable
beside the host executable. It does not search PATH or a checkout. Tests receive
an explicit helper path through the runner's `LASH_VM_WORKER` environment.

SDK releases attach `lash-sdk-worker-VERSION-OS-ARCH.tar.gz` plus its SHA256.
The archive contains `bin/lash-vm-worker`, optional reference sources under
`sdk/`, and `manifest.json` with protocol and crate diagnostics, the explicit
worker binary path, and file checksums. SDK hosts can build from registry
packages; they do not need those reference sources, a matching checkout,
compiler or build profile. Pass the extracted binary path to the service.
`lash-vm-worker --version` prints JSON diagnostics, including the protocol range,
crate version, target and build flags. Packaging requires an optimized helper
without testing controls; runtime compatibility depends only on the protocol.

For a single executable, `crates/lash/examples/worker_host.rs` registers its
frontend and early re-exec entry before any runtime, credentials or stores.

The native bootstrap lives in `entry.rs`. The core boundary gate allows only
its argv read and empty-environment probe; every other ambient read in the
worker library remains refused.

RLM and process hosts share `lash_vm_client::service::Service`. The facade names
its configuration as `lash::rlm::WorkerService`, `WorkerPoolConfig`, `WorkerEntry`
and `WorkerDeadlines`. Pure artifact inspection and state restoration also run in
workers. Source and VM entry points remain in `lash-vm-worker`; the parent's pool,
framing, queue admission and opaque state client live in `lash-vm-client`.

Resident RLM cells request `capture_state_view`. Their completion carries the
final outcome and the snapshot's guest metadata together. The parent adopts
both without reopening and reserializing the completed snapshot in another
worker, so state inspection cannot exhaust the cell's CPU budget after its
final has arrived. Completion metadata is bounded and its definition IDs must
match the opaque snapshot. A malformed completion refuses the turn with
`ExecutionStateCaptureFailed` before any output enters history. Process runs
request the outcome alone.
