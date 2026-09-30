# VM workers

`lash_vm_client::WorkerPool::new(PoolConfig::standard(WorkerEntry::helper(path)))` prewarms a
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
backing host handles. Frames include their 40-byte envelope in
the configured cap, and encoding stops before crossing that allocation bound.
Linux native-process laws run in this lane. macOS has an actual-descriptor
adapter; execution evidence on a macOS runner remains pending. Persistence,
journal replay and live Restate kill points belong to the broker/adapter lanes.

The helper's compiled identity fingerprints its transitive local source, the
locked dependency graph and compiler pin, plus target, debug assertions and its
testing feature. Cargo and Buck2 run the same `build.rs`; the fingerprint is a
compiler environment input and is never generated into the source tree. Cargo
watches the closure files and source directories, including source additions
and removals. Buck2 declares the closure as build-script inputs. The inventory
generator declares inputs without hashing their contents. Re-exec hosts supply
their own immutable compiled identity.

SDK releases attach `lash-sdk-worker-VERSION-OS-ARCH.tar.gz` plus its SHA256.
It contains `bin/lash-vm-worker`, the complete exact `sdk/` source tree and a
manifest with the compiled identity and every source file's checksum. Build the
host against those sources using the pinned compiler, matching target, release
profile and disabled testing feature. Place the helper beside the host or select
its path explicitly. `lash-vm-worker --build-identity` exposes the compiled
identity for inspection; the handshake refuses a different build. Registry-only
consumption of this source closure is unresolved in FIG-4408.

For a single executable, the facade's `crates/lash/examples/worker_host.rs`
registers its frontend and early re-exec entry before any host runtime,
credentials or stores. The worker and parent use the same compiled identity.
The source tree and helper are a matching pair; substituting a worker from a
different checkout, compiler, profile, feature selection or target is refused.

The native bootstrap lives in `entry.rs`. The core boundary gate allows only
its argv read and empty-environment probe; every other ambient read in the
worker library remains refused.

RLM and process hosts share `lash_vm_client::service::Service`. The facade names
its configuration as `lash::rlm::WorkerService`, `WorkerPoolConfig`, `WorkerEntry`
and `WorkerDeadlines`. Pure artifact inspection and state restoration also run in
workers. Source and VM entry points remain in `lash-vm-worker`; the parent's pool,
framing, queue admission and opaque state client live in `lash-vm-client`.
