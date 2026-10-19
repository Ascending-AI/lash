# 0123: Model code runs in resettable worker processes

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

## Status

Accepted. The parent client owns the bounded pool and transport in
`lash-vm-client`. `lash-vm-worker` hosts the kernel machine and lowers and
prints dialect source. Code mode cells, durable process bodies and process creation
use the worker service.

## Context

Reusable workers need one owner for guest state, a reset that covers all of
it, and parent-owned identity and effect admission. Parked runs need to be
stored and routed without decoding guest values. Model code has no ambient
language authority; its requested effects use host-owned bindings. Lash
implements this execution boundary, not host auth or security policy.

The kernel ([ADR 0139](0139-the-lash-vm-is-a-dialect-free-kernel.md)) is a
plain library: it does no I/O, keeps no global state and returns to its
embedder at every park and fuel slice. Where it runs is the embedder's choice
(kernel design §9 rule 5). This ADR is lash's choice.

## Decision

### 1. One machine owns each run's mutable bytes

A worker hosts one `lash_kernel_vm::paged::Machine` for one execution between
resets: its tasks, its heap's resident and dirty pages, and its scratch. No mutable
run state lives in a `static`, `thread_local!`, `OnceLock` or
`LazyLock`:

- `scripts/check-vm-static-state.py` refuses a static in a `lash-kernel-*`,
  `lash-dialect-*` or `lash-ext-*` crate, `lash-regress`, `lash-vm-runtime`
  or the worker crate unless `scripts/vm-static-state-allowlist.txt` names it
  with a reason. The inventory follows production modules from crate roots;
  cfg(test), testing-only items and the conformance harness crate are excluded.
  The allowlist holds guest-free build tables and opt-in diagnostic counters.
  A stale or unreasoned entry is also refused. Function identities hash the
  caller's definition without retaining it globally; binary decode depth
  belongs to each TokenReader. `//:vm_static_state` and `//:vm_parent_paths`
  execute in `//:main_tests`; `//:vm_guard_laws` pins production scoping.
- The function registry a worker assembles at startup (the kernel library,
  the machine's functions, each extension's and each dialect's helpers) is
  fixed before the first document arrives and holds no guest data.
- Third-party interning below a dialect's parser (swc's atoms) carries no
  guest-readable state; it is recorded here, not allowlisted.

### 2. A reset is a drop

`Reset` drops the hosted machine, its host wire, its owner and its CPU
ceiling; the next `Start` builds a new machine from the document it carries.
A worker also retains a bounded cache of immutable document preparations
(FIG-6148): sixteen documents whose encoded content totals at most eight MiB.
Exact document bytes identify each entry, including its kernel format and
content-addressed helper function pins; the worker's immutable embedding owns
the prepared library, so admission never crosses a registry or release boundary.
A hit retains the same document allocation, reusing its admission and compiled
code. Eviction drops that allocation; the library reclaims dead weak entries at
the next admission. Oversized documents run uncached. This cache holds no run,
guest heap, host wire, owner, bindings, outcomes or authority, and changes no
charge or resume behaviour. A reset drops all execution state while immutable
preparation can serve another activation. The hosting plugin factory and
worker service each retain the same sixteen-document/eight-MiB bound for
decoded content and transport encodings. The factory shares pure preparation
across its per-activation engine bindings. The RLM hosting factory retains
that kernel factory across bindings and resets it when worker or registry
configuration changes. Every binding keeps its own
current store port, trace context and recorded settings. A store read still
fetches and checks current bytes, including absence, before reusing decoding.
The hosting factory also retains at most sixteen derived entry-effect sets
totalling eight MiB encoded,
keyed by verified document identity, entry and the immutable registry it holds.
The checker graphs and helper-body scratch are dropped after derivation; every
lookup still reads and checks the document and entry before reusing the result.

Execution state is never cleared field by field, so state added to the machine
later is covered by the same drop. A worker is reset only after a clean end or
release. Any guest crash, native panic, exhausted limit, protocol violation
or timeout discards the worker instead.

### 3. Model code runs through slices and deliveries

The parent drives the machine. `Run { slice, cancel }` runs ready tasks until
none is ready, the run ends, or `slice` charge units are spent; the worker
answers `Parked` (the effects and sleeps requested since the last park, the
waits withdrawn since, and, when the park requested something, the
candidate checkpoint it sealed), `Slice` (still ready) or `Ended`. `Deliver`
hands the machine one committed outcome for the wait it named, in any order;
`Delivered` says it was taken or dropped. `Seal` seals the run as it stands
at a safe point, which every slice return and park is (`K-MACH-007`), into a candidate
(`Sealed`). Host reads (clock, random, projection reads) cross to the parent
in the middle of a slice as `HostRead` and are answered by `HostAnswer`;
the heap's page reads and seals cross the same way as `Pages`
([ADR 0145](0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md)
§3); prints arrive in order as `Printed`, with no authority. A run cancel is the
flag sent with a slice and is observed at the machine's next safe point.

These cross in as few exchanges as fit one frame (FIG-5921): a `Run` carries
the run's `Start` when it begins the run and the outcomes delivered since the
last slice, and a `Parked` whose requests the parent saves carries the
capability of the candidate root it sealed under the execution the start
named. Parts that outgrow one frame together cross alone, as `Start`,
`Deliver` and `Seal`. A cell's
lowering `Prepare` and its run's start share one lease with no reset between:
a reset there would drop no machine, and the start drops what it would.

A parked run names sites and identities, never code positions, so it
resumes in another worker, after a rebuilt executable, under the same
document. A worker started from a checkpoint reads the root's header, then
only the pages its next steps touch. Laws in `crates/lash-vm-worker/tests/pool_laws.rs`:
`a_parked_run_carries_on_in_another_worker_without_asking_again`,
`a_typescript_cell_lowered_in_the_worker_parks_and_resumes`,
`a_parked_run_is_refused_under_another_document`,
`host_reads_are_answered_by_the_parent_and_prints_arrive_in_order` and
`a_run_cancel_is_observed_where_a_slice_returns`.

### 4. Authority stays with the parent

The worker holds the machine and its immutable preparations. The parent holds the grants
and their `execution_binding`s, the effect ledger, admissions and
`ToolCallId` derivation (ADR 0117). It admits every effect a run requests
against the admitted execution context: a worker's identity, the bytes it
returns or a claimed site confer nothing. This is lash's internal authority
model, not host policy.

- A code mode cell's envelope (`crates/lash-protocol-rlm/src/executor/envelope.rs`)
  is the host's half of its parked run: the document the cell was lowered to,
  the effects it was lowered against with the tool each is, the grants its
  deferred resolutions recorded, its prints and its admitted calls. It is
  committed beside the machine's state, never sent to the worker.
- A process body's worker returns its parked state only; the body's ledger
  stays with the broker.

### 5. The parent never decodes machine state

At the broker boundary a parked run is a `CheckpointRoot` page in the
activation's staging, named by a stage capability
([ADR 0145](0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md)
§2, §6). The parent re-hashes every page the worker seals, checks each
child's authority, and reads only the root's header: its owner binding,
document, kernel version and pinned release. It never decodes a heap page.
A run resumes from a committed root (`StartFrom::Checkpoint`): the root's
capability under the activation's root lease and the admission of every
wait it holds. Only the worker opens the root into a machine, which refuses
a root written under another document. The protocol crate depends on
nothing that could decode the machine's state.

### 6. The protocol

`lash-vm-protocol` defines what a parent and its worker exchange, and no
transport or pool:

- **Messages.** Parent to worker: `Start` (the owner, the admitted document's
  encoding, a fresh start or a committed checkpoint root, the run's bounds
  and its owner binding), `Prepare` (pure work with no parent authority:
  lowering source to a document, printing a document as source), `Run`,
  `Deliver`, `HostAnswer`, `Pages`, `Seal`, `Cancel`, `Reset`, `Shutdown`.
  Worker to parent: `Ready`, `Started`, `HostRead`, `Pages`, `Printed`,
  `Parked`, `Slice`, `Ended`, `Delivered`, `Sealed`, `Prepared`,
  `Progress`, `LimitExceeded`, `Refused`, `Cancelled`, `ResetDone`. A
  `Pages` exchange carries at most 64 pages and 4 MiB. The parent compiles no model code: the
  worker validates and compiles the document it is started with.
- **Headers.** Every message carries its execution lease, owner and frame
  epochs and a transport sequence. `MessageFence` admits only the next
  sequence of the current lease and epochs.
- **Framing.** A frame is the magic `LVMP`, a big-endian length and the
  payload. The handshake checks the worker wire version against
  `[MIN_SUPPORTED_WORKER_PROTOCOL_VERSION, WORKER_PROTOCOL_VERSION]`, both
  defined in `lash-vm-protocol`. An out-of-range worker receives a typed
  refusal naming both versions. Crate versions are diagnostic only.
- **Bounded decoding.** Decoding charges frame size, nesting depth, node
  count and cumulative allocation against `DecodeLimits` before it
  allocates, and a malformed, oversized, truncated or trailing frame is a
  typed `CodecRefusal`. Every value is a charged node, map keys included.
- **Bounds.** `ProtocolBounds` states every bound a host holds its workers
  to, with no implicit default. Its `standard()` preset allows 2 MiB of
  parked state, 1 MiB effect values, 64 KiB of source and a 5-second
  no-response watchdog, which host waits pause and which is not a guest
  execution limit, over `DecodeLimits::standard()`.
- **Encoded run limits.** An effect request or result over its byte bound is
  `WorkerLimit::EffectValue`; parked state over its bound is
  `WorkerLimit::VmState`; what one slice prints over the run's heap bound is
  `WorkerLimit::Observations` (FIG-4458). A worker frame that cannot encode
  is `WorkerLimit::Frame`. Each carries the measured size and bound. These
  deterministic limits are recorded, never retried (FIG-4475, FIG-4476).
- **Infrastructure outcomes.** `WorkerCrashed`, `WorkerUnresponsive`,
  `ProtocolViolation`, `RunRefused`, `WorkerLimitExceeded` and the
  parent-side `WorkerDeployment` are kept apart from guest errors. EOF or
  exit is supervisor evidence, never worker testimony; a fully received
  `Ended` wins over a later EOF; a partial frame is refused and the last
  committed park kept.
- **Typed causes.** Every cause crosses the pipe as a variant, never as
  text. `ProtocolViolation` carries a `ProtocolBreach`: a broken frame,
  fence, sequence or payload, which a fresh worker may keep, so it is
  retried. `RunRefused` carries a `RunRefusal`: an input of the run the
  worker reads when it starts (its state, document or bounds, or a payload
  over its bound) is refused the same way on every attempt, so it is
  terminal: the code mode cell records a Host failure and the process ends
  `process_run_refused` with the refusal as its failure data. A worker's
  `Refused` frame carries one of the two and nothing else; it cannot name a
  crash, silence or limit. The diagnostic beside a cause is a `Detail` cut to
  a fixed bound, so a refusal's text never changes its class (FIG-4645).

### 7. Durability

Determinism is not a goal; durability is. The machine parks; the broker
commits. Each time a run parks with new requests, the worker seals the
changed pages into a candidate root, and the broker publishes it, in one
transaction, with the ledger of the waits it stands on and the admission of
every effect requested since the last park; effects start only after that
commit ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md)
§8, [ADR 0145](0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md)
§6). A park that requests nothing saves nothing. When a worker dies, the
parent fences the transport lease and returns a typed retryable
infrastructure failure. The run resumes from its last committed park: a new
machine opens the committed root under a fresh root lease and the first
delivery answers, from their records, every effect whose outcome
committed. Whatever ran after that park runs again; no
committed effect does. Cancellation stays the recorded checkpoint
observation of ADR 0039: a physical kill after a grace period does not decide
precedence.

### 8. The broker

`lash-vm-broker` is the parent side of a kernel run. Its `kernel` module
holds the pending-effect ledger (`EffectLedger`), the park transaction and
delivery in any order (`store`), the loop that drives a machine through its
parks (`KernelBroker`), and number-exact effect values (`value`). The broker
does not know where the machine lives: it drives a `DrivenMachine` that a
`Machines` starts or resumes. Lash's is a machine in a pooled worker
(`lash-vm-client`'s `machine` module); `lash-vm-worker::in_process::InProcess` is the testing-only adapter for
broker laws. Machine construction and the page source a machine reads and
seals through live in the worker crate, alongside source import; the page
service, its capabilities and the activation's spool are the broker's
(`kernel::pages`).

- **Every effect is brokered.** `KernelEffects` admits each request against
  the run's bindings before any tool runs. A refused request takes no
  admission.
- **The parent owns every identity.** `CodeCallIdentities` derives each
  admitted effect's `ToolCallId` (ADR 0117 §2), the one derivation every
  host mints from.
- **Bounds.** `KernelCeilings` holds a run's bounds to the broker's own
  ceilings on what one park admits and one list `join` holds (`K-BND-001`);
  a park that would pass one admits nothing and ends the run there.
- **Fencing.** A message whose lease, owner epoch, frame epoch or sequence is
  not the next one is a protocol violation. The worker is discarded; nothing
  it sent after the violation is acted on.
- **A park releases the worker.** Between a park and its deliveries the run
  is committed state, not a worker, so a pool of one slot completes a cell
  that awaits a process it started.

Laws: `crates/lash-vm-worker/tests/park_matrix.rs`
(`sqlite_memory::a_kernel_run_of_three_tasks_resumes_from_its_parks_at_every_cut`,
`committed_outcomes_delivered_in_either_order_resume_and_keep_their_numbers`),
the ledger and ceiling laws in `crates/lash-vm-broker/src/kernel`, and in
`pool_laws.rs` `worker_crash_mid_run_leaves_the_parent_running` and
`a_passed_bound_is_the_runs_typed_end_and_the_worker_is_reused`.

### 9. The worker entry and pool

`lash-vm-client` launches an explicitly configured worker entry with an empty
environment. The entry closes inherited descriptors except its IPC socket
before running the worker server. The pool bounds checkout, queue size,
process count, deadlines, cumulative CPU and replacement attempts. Clean
release resets the worker; a crash, protocol failure or exhausted limit
discards it. Pool accounting is parent-owned and counts per activation: a
run's attempts and cumulative CPU are not stored with its park. Its held
wall time is: an execution holds workers for at most
`Deadlines::execution_wall`, summed from each checkout's admission to its
release, and a code mode cell saves what it held with each park, so the cap
holds across page answers, activations and owners. The pool's queue and a
durable park that released the worker do not count (FIG-6786). The restart
window counts failed workers, never a run's own typed limit or refusal.

Async callers use `ServiceRuntimeOps::request_accounted`, which checkpoints
worker accounting around a blocking-pool task. Pool saturation is a typed
host failure across plugin and tool-attempt boundaries, so the attempt is
recomputed or parks without recording a tool refusal. Guest lowering
refusals remain tool results.

This provides crash containment and system-call confinement, not a full OS
sandbox. Before it serves, a worker installs an address-space ceiling and a
seccomp allowlist of the calls it was measured to make (FIG-5858); a call
outside it kills the worker. Lash installs no namespaces, Landlock or
cgroups. A native escape from the machine keeps the worker user's identity
but only the allowlisted calls. An owned child installs a kernel CPU
ceiling (`RLIMIT_CPU`) before guest work, from its current process CPU and
the configured execution CPU budget; only a reset for a new checkout
installs another.

The parent never starts a machine or lowers source:
`scripts/check-vm-parent-paths.py` refuses `KernelMachine` entry points and
dialect front ends outside the worker crate and the kernel set.
`workers_refuse_another_protocol_before_guest_admission` pins the handshake.

Sources: `crates/lash-vm-client/src/ipc.rs`,
`crates/lash-vm-client/src/pool.rs`, `crates/lash-vm-worker/src/entry.rs` and
`crates/lash-vm-worker/src/worker.rs`.

### Presets and host packaging

`PoolConfig::standard()` holds one prewarmed process, at most four
processes, two queued inputs and eight MiB queued bytes, over
`ProtocolBounds::standard()`. Run bounds allow fifty million charge units,
64 MiB of machine memory, call depth 1,024, 1,024 live tasks, 256 requests
per park and 1,024 `join` members. Checkout and IPC silence are five
seconds, compute thirty seconds, serialization five seconds, cancellation
grace 100 ms, cumulative CPU ten seconds, attempts three and held wall
time 600 seconds. The restart
window admits eight failed replacements per minute. `PoolConfig::codemode()`
raises parked state to 64 MiB, frames and queued bytes to 128 MiB and decode
allocation to 256 MiB; these allowances have no workload measurement behind
them.

`WorkerTuning::standard()` holds a parent wait of 86,400 seconds, 16 KiB
inbound buffering, an 8 MiB parser stack plus 40,000 bytes per source byte
(measured at 1.8x the worst observed frames), and a slice of one million
charge units. The other working values have no workload measurements. The
parent sends this policy in the worker bootstrap.

These bounds limit resource admission; they do not prove an arbitrary-guest
deadline or optimal host concurrency, and no portable CI latency gate rests
on them. Hosts select their own configuration when their workload needs
different bounds.

Releases bundle an explicitly selected optimized helper, JSON diagnostics and
reference SDK sources with checksums. Registry consumers build without the
reference tree. The host passes `WorkerEntry::helper(path)` or
`Service::subprocess(path)`; the SDK default names the helper beside the host
executable, with no PATH or monorepo discovery. `--version` reports the
protocol range, crate version, target and build flags. Admission compares
only protocol versions before guest work. A single executable registers the
facade's early re-exec entry before constructing its runtime, credentials or
stores, as `crates/lash/examples/worker_host.rs` demonstrates. Sources:
`crates/lash-vm-client/src/config.rs`, `crates/lash-vm-client/src/service.rs`
and `scripts/package_vm_worker.py`.

### Host verdicts

A verdict of the host's worker budget, pool capacity or recovery store (a
deadline, the cumulative CPU or attempt bound, a full queue, a checkout
timeout or a restart storm) is never an execution's recorded outcome, since
another node with capacity would not reach it. A process body fails its
attempt retryably. A code mode cell fails its attempt retryably too: it commits
nothing, the model never sees the verdict, and the retry runs the cell again
from the last committed park. Only a limit the run itself exhausted (charge,
memory, call depth, what it prints, or its encoded effect values, parked
state and frames, measured against their bounds) is recorded: the process's
terminal failure, or the cell's program failure (FIG-4451, FIG-4458,
FIG-4475, FIG-4476).

A crashed or unresponsive worker, or a broken protocol, fails the attempt
retryably, including during lowering. `PoolError::is_host_verdict` uses the
infrastructure outcome's retryability classification; the cell records no
Host failure for it, and its retry runs on a replacement worker (FIG-4459).
A worker that died after it began computing the run (`RunCrashedWorker`),
such as an allocation abort at the address-space ceiling, is no capacity
verdict. A process body's attempt fails retryably and spends one of its
attempts, so a body that keeps killing its worker ends once they are spent.
A code mode cell reads the supervisor's evidence (FIG-6806). A limit lash
sets on the worker, an allocation abort at its address-space ceiling
(`SIGABRT`) or its confinement's kill for a forbidden system call, ends the
cell at once: it records `CELL_WORKER_DIED` as its Policy failure, whose
advice names the cell as the cause, and the model reads it (FIG-6786).
Anything else, such as a `SIGKILL` with no cgroup out-of-memory kill
behind it from an operator, a node drain or a neighbour's out-of-memory
kill, is a loss from outside the cell. It spends one of the cell's
`Deadlines::max_attempts` runs, counted per execution on the node that
serves it, and fails the attempt as a host verdict while one is left. The
next pass runs the cell again on a fresh worker from its last park, whose
journal replays the calls that ran, so no completed call runs again and the
model never reads the loss. A cell whose worker is lost on every run
records `CELL_WORKER_DIED` once they are spent, as a Host failure. Its
advice does not blame the cell; it says whether running the cell again is
safe, naming the tool calls of it that completed and took effect. The
count is in memory: an owner that takes the session over counts the cell's
runs from zero, and the activation-loop budget still parks a session whose
passes keep failing. A worker lost before it computed the run stays a host
verdict that spends nothing (FIG-6786).
A `RunRefused` is not retryable: the cell records its Host failure, since a
retry would meet the same refusal (FIG-4645).

A missing or unexecutable configured worker is a typed `WorkerDeployment`
fault carrying the executable path. The spawn seam classifies ENOENT, EACCES
and ENOEXEC. It is non-transient and records no guest result. The turn writes
a durable `WorkerDeployment` park on its first refusal, keeping its committed
state; supplying an executable at that path lets an operator redrive the same
run to completion (FIG-4776).

## Consequences

Sealed parked graph state lets the broker store and fence a run without
restoring guest values. A reset drops the run's mutable execution state by construction; immutable
document preparation has separate bounded ownership.
Field-by-field cleanup could miss a newly added state owner; keeping grants
in returned guest state would let that state replace parent-owned bindings.

Code mode cells, durable process bodies, process creation and dialect lowering and
printing run through the shared worker service. Parent adapters retain
sealed parked graph state and the document identities it names.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host
events, routing and scheduling.
