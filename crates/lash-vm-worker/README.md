# VM workers

Helper/native installation is shared with `lash-kernel-engine` through
`embedding::install`. The worker retains process isolation, wire pages,
cancellation and resets; standalone callers can use the engine in process.

A worker hosts one kernel machine (`lash_kernel_vm::KernelMachine`) for one
execution between resets, and lowers and prints dialect source. The parent's
pool, framing, queue admission and opaque state client live in
`lash-vm-client`; the broker that commits each park lives in `lash-vm-broker`
(ADR 0123).

`lash_vm_client::WorkerPool::new(PoolConfig::standard(WorkerEntry::helper(path)))`
prewarms a credential-free helper. There is no in-process fallback. A host may
instead register `worker_entry(&embed)` as its first action and configure
`WorkerEntry::reexec()`. Register the entry before creating a runtime, loading
credentials, opening stores, or constructing providers.

A host names its dialects (ADR 0140). `dialect-typescript` and
`dialect-python` are build features, neither a default, and `embed` builds the
embedding from the ones it names:
`Embedder::new().dialect(typescript(tuning)?)?.dialect(python())`.
`worker_entry` binds that configuration to the parent's packages; a host
binding directly calls `finish(&held)` with the same holdings.
An embedding with no dialect is refused (`EmbedError::NoDialect`). The helper
executable installs every dialect it was built with, so build it with the
host's dialect features. A run names the dialect its session or process
recorded, and a worker that lacks it refuses the run with
`RunRefusal::DialectNotInstalled` before it reads the document.

The parent installs [host packages](../../docs/host-packages.md) once and
supplies the worker's complete definition registry, including retained
artifacts. A single-binary embedding supplies natives by `FunctionId`; it
does not assemble a second package list. A shipped worker receives the
registry in one release-image frame at pool start, before serving work.
The registry stays fixed for the worker's lifetime. A native-only function
with no implementation refuses pool start; a function with a body can use
that body without its optional native.

The launcher clears the environment. Immediately after exec, the entry closes
all inherited descriptors except its socket. Linux uses `close_range` and
requires kernel 5.9 or newer. Workers support Linux only; elsewhere the entry
refuses with `PoolError::UnsupportedPlatform`. The kernel limits guest
authority and the process contains native crashes. Once its embedding is
assembled, and before it reads a frame, the entry confines the process
(`WorkerConfinement`, carried in the bootstrap): an address-space ceiling
(`RLIMIT_AS`, soft and hard) and a seccomp filter on every thread. The filter
admits the system calls a serving worker was measured to make: its socket,
memory that is never writable and executable at once, threads of its own
process, clocks, entropy, its own CPU ceiling and an abort's signal. `openat`,
`clone3` and `getcwd` are refused with an error, for the runtime's fallbacks
(`getcwd` for a panic that prints a backtrace); any other call kills the
process, which the parent reports as
`WorkerCrashed { evidence: ForbiddenSyscall }`. A `dhat-heap` build also
admits `openat`, `statx` and `lseek`, which its profile flush needs to
symbolize and write the profile. x86-64 and aarch64 only; a
worker that cannot confine itself refuses with `BootstrapFault::Confinement`.
Before guest work an owned child installs a CPU ceiling (`RLIMIT_CPU`) from
the configured execution budget.

`checkout` reserves the complete encoded input size and bounds its wait. A
checkout owns one execution lease. `start` takes the admitted document's
encoding, a fresh start or a committed checkpoint root, the run's bounds and
the execution its parks are saved under; it crosses with the first `run`,
where the worker validates and compiles the document. `run` runs ready tasks
for one slice of charge units and answers `Parked`, `Slice` or `Ended`. Host
reads (clock, random, projection reads) arrive mid-slice and are answered
with `host_answer`; the heap's page reads and seals arrive mid-slice too, as
`Pages` exchanges of at most 64 pages and 4 MiB, and the parent answers them
from the activation's spool or, under its root lease, from the store; prints
are collected with `take_printed`. `deliver` hands the machine one committed
outcome, which crosses with the next slice, and `seal` takes the candidate
root a park that requests something sealed, or asks the worker to seal one.
Parts that do not fit one frame together cross in exchanges of their own.
Heap pages are opaque in the parent: it re-hashes each page a worker seals
and reads a checkpoint root's header, and only a worker opens a root, under
the document it names
([ADR 0145](../../docs/adr/0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md)). Descriptions carry no grants or backing host handles; the
broker admits every effect against its admitted context.

A cell's lowering and its run share one lease: the worker that lowered the
document starts the run with no reset between, so a cell that calls a tool and
finishes crosses the boundary in 22 frames (FIG-5921). A worker compiles its
package library before it reports ready, so its first run costs what a warm one
does (FIG-5922).

`prepare` runs pure work in a worker: `Request::Lower` lowers source in a
dialect to a kernel document against Current and the session bindings in
scope. Namespace composition reads the snapshot's visible Package Members. `Request::Print` renders a document in a selected
exact source format with host limits and an optional disposable map;
`Request::Read` separately decodes that source against the base Document it
was printed from, without admission. Neither printing nor reading resolves
dependencies or stores source/maps.
The host-facing `WorkerService::print` and `::read_exported` expose these
operations; hosts compare the returned Documents' canonical bytes themselves.
Both read guest-controlled input, so both run here. A dialect's parser runs on
a thread whose reserved stack is 8 MiB plus 40,000 bytes per source byte at
the 64 KiB source cap; pages commit only when touched.

The pool never retries guest execution. On infrastructure failure, it fences
the checkout, kills and reaps the process, then replenishes its minimum. The
run resumes from its last committed park through the durable engine. CPU is
charged from process-clock progress and final `wait4` evidence, including a
crash before the final frame. Physical cancellation does not decide the
recorded winner.

Release exchanges `Reset` and `ResetDone` before reuse. The reset drops the
machine, its host wire, its owner and its CPU ceiling. Dropping an unreleased
checkout discards it. After an error, a limit, protocol violation, timeout, or
reset failure, the process cannot return to the idle pool. Repeated failures
trip a typed restart storm and wake queued work.

Compute, serialization, and response phases have separate absolute deadlines.
Repeating a phase cannot extend it. The no-response watchdog bounds IPC waits,
bootstrap, and reset; it is not the compute deadline. Parent-owned host waits
pause execution deadlines. CPU and attempt totals remain in the parent budget.

A run that parks releases its worker: between a park and its deliveries the
run is committed state, so a one-worker pool completes a cell that awaits a
process it started. A nested checkout still has a bounded queue and returns
`CheckoutTimedOut` when no slot frees. It never waits silently forever.

The standard presets are min 1/max 4, two queued items/eight MiB, four MiB
frames, two MiB parked state, one MiB effect values, 64 KiB source, 100,000
decoded nodes, 64 MiB charged decode allocation and five seconds IPC silence.
Runs allow fifty million charge units, 64 MiB of machine memory, call depth
1,024, 1,024 live tasks, 256 requests per park and 1,024 `join` members.
Checkout is five seconds, compute thirty seconds, serialization five seconds,
cancellation grace 100 ms and cumulative CPU ten seconds. An invocation allows
three attempts; the pool permits eight failed replacements per minute. A
worker's address space is capped at 4 GiB, which must exceed the parser stack
of the largest admitted source. These
are host bounds, not latency targets or guarantees for arbitrary guests.
`PoolConfig::codemode` raises parked state to 64 MiB, frames and queued input to
128 MiB and charged decode allocation to 256 MiB. Hosts can supply a smaller
`PoolConfig` for their own admitted workload.

Effect values cross as JSON text with every number carried as written.
Frames include their eight-byte envelope in the configured cap, and encoding
stops before crossing that allocation bound.

`lash-vm-protocol` defines `WORKER_PROTOCOL_VERSION` and
`MIN_SUPPORTED_WORKER_PROTOCOL_VERSION` once for both sides. Pool admission
checks the worker's `Ready` message before guest work. An out-of-range version
returns `PoolError::ProtocolVersion` with both protocol versions, the supported
range and both crate versions. Crate versions are diagnostic only. The wire
version stays at 1 until 1.0. The `synthetic-next` acceptance feature moves
both ends of the supported range to 2, so N and N+1 cannot pair at admission;
reviewed shape changes refresh the committed
`lash-vm-client/tests/snapshots/wire-v1*.snap` snapshots in place. After 1.0,
the repository gate requires snapshot changes to bump the protocol version and
retain the old versioned snapshot.

Hosts select `WorkerEntry::helper(path)` or `Service::subprocess(path)`
explicitly. The SDK's `Service::default()` uses the documented `lash-vm-worker`
executable beside the host executable. It does not search PATH or a checkout.
Tests receive an explicit helper path through the runner's `LASH_VM_WORKER`
environment.

SDK releases attach `lash-sdk-worker-VERSION-linux-ARCH.tar.gz` plus its
SHA256. The archive contains `bin/lash-vm-worker`, optional reference sources
under `sdk/`, and `manifest.json` with protocol and crate diagnostics, the
explicit worker binary path, and file checksums. SDK hosts can build from
registry packages; they do not need those reference sources, a matching
checkout, compiler or build profile. Pass the extracted binary path to the
service. `lash-vm-worker --version` prints JSON diagnostics, including the
protocol range, crate version, target and build flags. Packaging requires an
optimized helper without testing controls; runtime compatibility depends only
on the protocol.

For a single executable, `crates/lash/examples/worker_host.rs` registers its
early re-exec entry before any runtime, credentials or stores.

The native bootstrap lives in `entry.rs`. The core boundary gate allows only
its argv read and empty-environment probe; every other ambient read in the
worker library remains refused.

Code mode and process hosts share `lash_vm_client::service::Service`. The facade
names its configuration as `lash::vm::WorkerService`, `WorkerPoolConfig`,
`WorkerEntry`, `WorkerDeadlines` and `WorkerConfinement`.
