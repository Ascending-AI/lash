# 0123: Model code runs in resettable worker processes

## Status

Accepted. The parent client owns the bounded pool and transport in
`lash-vm-client`. `lash-vm-worker` owns source frontends and VM execution.
RLM cells, durable process bodies and process creation use the worker service.

## Context

Reusable workers need one owner for guest state, a reset that covers all of
it, and parent-owned identity and effect admission. VM snapshots need to be
stored and routed without decoding guest values. Model code has no ambient
language authority; its requested effects use host-owned bindings. Lash
implements this execution boundary, not host auth or security policy.

## Decision

### 1. One instance owns every guest-derived byte

`lashlang::VmInstance` owns the session's globals, heap and roots, the
execution scratch, the linked and compiled cell cache, and any run in
flight with its operand stacks, pending handles and suspension. Nothing
guest-derived lives in a `static`, `thread_local!`, `OnceLock` or
`LazyLock`:

- Record keys are content-addressed `Symbol`s (`&'static str` constants or
  shared `Arc<str>` text), compared by pointer then by text. Record symbols
  require no global interner.
- `scripts/check-vm-static-state.py` refuses a static in lashlang,
  lash-typescript, lash-lashlang-runtime or the worker crate unless
  `scripts/vm-static-state-allowlist.txt` names it with a reason. The
  allowlist holds build tables projected from constant lists, test-only
  counters, parent-side telemetry, and the heap's write-stamp counter,
  which is never guest-readable, persisted or sent. A stale or unreasoned
  entry is also refused.
- Third-party interning below the parser (swc's atoms) is outside the VM
  crates and carries no guest-readable state; it is recorded here, not
  allowlisted.

### 2. A reset is a drop and replace

`VmInstance::reset` drops the instance and constructs fresh guest state under
its configured cache capacities. `VmInstance::pristine()` selects the standard
64-entry capacities; `with_cache_capacities` selects others, including zero to
disable residency. Reset preserves this host policy and drops every cache entry. It
constructs a fresh instance rather than cloning a template. It clears nothing
field by field, so state added to the instance later is covered by the same
drop. The laws:

- `vm_reset_leaves_no_guest_observable_state`: session A plants a sentinel
  in globals, a closure, a regular expression with advanced `lastIndex`, an
  error object, a record, a cached compiled cell, a parked continuation and
  a run left in flight on its effect. After a reset, session B probes each
  one; none is observable, and B's outputs equal a pristine instance's byte
  for byte.
- `reset_equals_fresh_for_every_corpus_program`: every corpus program runs
  identically on a reset instance and a pristine one.

A worker is reset only after a clean completion or release. Any guest
crash, native panic, exhausted limit, protocol violation or timeout
discards the worker instead.

### 3. Model code runs through an owned step/resume interface

`VmInstance::start` runs a program until it asks its host for something,
and `VmInstance::resume` answers that request. Nothing crossing the
interface borrows:

- A run returns `VmStep::Suspended { request }` (an ability operation, a
  cancel checkpoint, or in process mode a snapshot point),
  `VmStep::Parked` (a durable continuation), `VmStep::Complete` or
  `VmStep::GuestError`.
- A resume consumes the answer to the pending request; one that answers a
  different request is refused and leaves the run suspended.
- `VmContinuation` carries `resume: VmResumePoint`: `NextInstruction`, or
  `ReissueOperation { operation, loop_phase }` when the continuation must
  issue an operation again. A run parked while awaiting an effect (section 8) reissues the
  resource operation, resource-operation batch, process await or sleep it
  was waiting on: the VM rewinds to the operation's instruction, pushes its
  operands back, restores any pending-request entries a batch consumed,
  uncharges the instruction and rewinds the occurrences of the call site and
  of every batch leaf, so the reissued request is the one it replaces. An
  await parked on one handle of a tuple, list or record of process handles
  also carries the leaf results it already received (see "Await and batch
  parking" below). A foreground park also records
  `loop_phase` (the instruction budget left until the next yield and the
  last announced cancel checkpoint), so a resumed run meets its cancel
  checkpoints where a run that never parked meets them, and it carries the
  run's expired functions. Resuming checks that the instruction pointer
  stands on the operation the discriminant names.
- A resume may answer a parkable effect request (a resource operation, a
  batch, a process await, a sleep) with `VmResume::Park`. The run suspends
  into `VmStep::Parked` with `VmParkReason::AwaitingEffect`, and the parent
  resumes it later from the continuation with the effect's outcome. A run
  that cannot suspend there asks `VmRequest::ParkDeclined` and stays
  suspended on its request.

The law `step_resume_matches_straight_through_for_every_corpus_program`
executes every corpus program step by step, and in process mode parks it at
every boundary and reopens each continuation from its bytes on a pristine
instance, and compares both with the program run straight through.
`effect_park_matches_straight_through_for_every_corpus_program` parks every
corpus program at every parkable effect, reopens it on a pristine instance
and compares it with the program run straight through, and
`effect_park_keeps_cancel_checkpoints_where_an_unparked_run_meets_them`
pins the loop phase.

### 4. Authority stays with the parent

The worker holds the guest heap, roots and scratch, and nothing else. The
parent holds the grants and their `execution_binding`s,
ordinals, incorporation, pending-summary and group ledgers, and `ToolCallId`
derivation (ADR 0117). It authorises every effect request against the
admitted execution context: a worker's identity, receiver bytes or claimed
call site confer nothing. This is lash's internal authority model, not host
policy.

- The RLM durable root is split. The parent hands the worker an
  `RlmWorkerEnvelope` (the durable header and one fragment per binding) and
  reads back an `RlmWorkerCapture` (the header and the changed and
  unchanged fragments). Both deny unknown fields, so a capture that names a
  grant is refused. Deferred tool outcomes have one durable home in the
  recorded resolution. The parent keeps an in-memory `DeferredLink`
  while executing the cell; the snapshot carries no copy. Product event records remain in host storage under ADR 0137.
- A process body's worker returns the VM bytes of its continuation only;
  the body's ledgers stay in the parent's envelope.
- Laws: `rlm_worker_envelope_carries_no_grant_or_binding` (a sentinel in an
  `execution_binding` never appears in worker-bound or worker-returned
  bytes) and `worker_returned_state_cannot_replace_parent_authority` (a
  forged returned grant is refused, and a guest binding named after an
  authority slot stays a guest binding).

### 5. The parent never decodes VM state

At the broker boundary, VM state is `lash_vm_protocol::OpaqueVmState`: a kind, an
owner, the VM component versions (`lashlang::vm_contract_versions()`), a BLAKE3
digest and the bytes. Each version is stored once: the bytes' format version is
the contract component their kind names, and their length is the bytes' own
(FIG-4645). The parent checks those
structurally, admitting each component against its declared read range
(`lashlang::vm_contract_reads()`, ADR 0115). The semantic decoders,
which restore guest values and compile regular expressions, are reachable
only through
`VmInstance` (`open_continuation`, `open_snapshot`,
`restore_durable_parts`): `VmContinuation` exposes no general
`Deserialize` implementation, and the protocol crate depends on nothing that
could decode the bytes. `parent_state_decode_never_compiles_regexp` pins both halves: the
parent's decode of a snapshot whose continuation holds an invalid pattern
succeeds, and only the worker's open refuses it.

### 6. The protocol

`lash-vm-protocol` defines what a parent and its worker exchange, and no
transport or pool:

- **Messages.** Parent to worker: `Start` (the program source, explicit
  context descriptions, and a fresh session or opaque state),
  `EffectResponse`, `Park` (answering a process boundary or a parkable
  effect request: serialize the run and end it `Suspended`), `Cancel`,
  `Reset`, `Shutdown`. Worker to parent: `Ready`, `EffectRequest`,
  `Suspended`, `Complete`, `GuestError`, `Cancelled`, `ResetDone`.
- **Headers.** Every message carries its execution lease, owner and frame
  epochs and a transport sequence. `MessageFence` admits only the next
  sequence of the current lease and epochs.
- **Framing.** A frame is the magic `LVMP`, a big-endian length and the
  payload. The handshake checks the worker wire version against
  `[MIN_SUPPORTED_WORKER_PROTOCOL_VERSION, WORKER_PROTOCOL_VERSION]`, both
  defined in `lash-vm-protocol`. An out-of-range worker receives a typed
  refusal naming both versions. Crate versions are diagnostic only. Shapes
  change in place at protocol version 1 under the pre-1.0 version freeze,
  with a committed snapshot for each feature selection.
- **Bounded decoding.** Decoding charges frame size, nesting depth, node
  count and cumulative allocation against `DecodeLimits` before it
  allocates, and a malformed, oversized, truncated or trailing
  frame is a typed `CodecRefusal`. Every value is a charged node, map keys
  included.
- **Bounds.** `ProtocolBounds` states every bound a host holds its workers
  to, with no implicit default. Its `standard()` preset allows 4 MiB frames;
  nesting depth 128;
  100,000 decode nodes; 64 MiB of cumulative charged decode allocation; 2 MiB
  of VM state; 1 MiB effect values; 64 KiB of source; and a 5-second
  no-response watchdog, which host waits pause and which is not a guest
  execution limit. The shared adapter service admits 64 MiB VM state,
  128 MiB frames, 256 MiB charged decode allocation and a 128 MiB queue.
  These bounds are explicit in its pool configuration.
- **Execution observations.** A run that reports its execution (a durable
  process body, or a traced cell) hands each step's observations to its
  parent as `Observations` frames before the step's response. The worker
  packs them into chunks of whole observations, each sized so that its frame
  is within the decode bounds and its payload passes the receiver's own
  check, so no transport bound limits how many observations a step makes:
  the run's own budgets do. Fuel bounds how many it can make, and its heap
  budget bounds the bytes a step holds and hands on, on both sides of the
  pipe. A stream that outgrows the heap budget, or a single observation no
  frame can carry, is the run's limit `WorkerLimit::Observations`: recorded
  like fuel, heap or depth, never retried (FIG-4458).
- **Encoded run limits.** An effect request or result over its byte bound is
  `WorkerLimit::EffectValue`; an opaque snapshot or continuation over its bound
  is `WorkerLimit::VmState`. Both carry the measured size and bound. A worker
  frame that cannot encode is `WorkerLimit::Frame`, carrying the message kind,
  complete encoded size and bound. Encoding counts without retaining bytes
  past the cap, and the outgoing fence advances only after encoding succeeds.
  These deterministic limits are recorded, never retried. Recorded effect
  results stay recorded even when they exceed the delivery bound. The RLM
  plugin result retains the typed limit in `CellFailure::worker_limit`; a
  process terminal retains it in its structured failure data (FIG-4475,
  FIG-4476).
- **Infrastructure outcomes.** `WorkerCrashed`, `WorkerUnresponsive`,
  `ProtocolViolation`, `RunRefused`, `WorkerLimitExceeded` and the parent-side
  `WorkerDeployment` are kept
  apart from guest errors. EOF or exit is supervisor evidence, never worker
  testimony; a fully received `Complete` wins over a later EOF; a partial
  frame is refused and the last committed checkpoint kept.
- **Typed causes.** Every cause crosses the pipe as a variant, never as
  text. `ProtocolViolation` carries a `ProtocolBreach`: a broken frame,
  fence, sequence or payload, which a fresh worker may keep, so it is
  retried. `RunRefused` carries a `RunRefusal`: an input of the run the
  worker reads when it starts (its state, context, source, artifact or
  limits, or a payload over its bound) is refused the same way on every
  attempt, so it is terminal: the RLM cell records a Host failure and the
  process ends `process_run_refused` with the refusal as its failure data.
  A worker's `Refused` frame carries one of the two and nothing else; it
  cannot name a crash, silence or limit. The diagnostic beside a cause is a
  `Detail` cut to a fixed bound, so a refusal's text never changes its
  class (FIG-4645).

### 7. Durability

Determinism is not a goal; durability is. The parent commits, in one
transaction, the next VM snapshot revision, the broker ledger that matches it,
and the admission of every operation the VM issued since its last snapshot;
bodies start only after that commit
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §8). When a
worker dies, the parent fences the transport lease, settles every operation it
already admitted, and returns a typed retryable infrastructure failure. The
run resumes from its last committed snapshot: on restore the saved outcome of
each admitted operation is fed back in, and no earlier host operation runs
again. Ordinals are admitted operation identities, never reset locally. A
replacement attaches to a still addressable operation rather than dispatching
another. A `Once` operation admitted without an outcome records `Interrupted`;
no recorded effect executes twice. Cancellation stays the recorded
instruction-checkpoint observation of ADR 0039: a physical kill after a grace
period does not decide precedence.

### 8. The broker

`lash-vm-broker` is the parent side of a worker run. `Broker::run` checks a
worker out of its `WorkerSlots`, starts the program from a fresh session or
the last committed checkpoint, answers every request the worker sends, and
commits the run's end. It holds no transport or pool of its own: a worker
is a `WorkerTransport` (send, cancel-safe receive, kill), and the watchdog
belongs to the transport, which reports a silent worker as
`WorkerRead::Unresponsive`.

- **Every effect is brokered.** A request's payload is decoded, resolved
  against the run's frozen bindings (`AdmittedContext`) and refused with
  the typed `AuthorityRefusal` payload (`lash_vm_request_refused`) before
  any tool runs: an unknown binding or operation, arguments the binding's
  contract refuses, an empty aggregate, a handle this run was never
  granted, a request kind the payload does not match, or an operation the
  broker does not serve yet. A refused request takes no ordinal. The broker
  serves resource operations, batches, awaits, sleeps and cancel
  checkpoints. The shipped adapters also serve prints, finishes, failures,
  admitted host tool calls for product events under ADR 0137.
- **The parent owns every counter.** `ParentLedger` gives each admitted
  request the next ordinal and derives its `ToolCallId`s through
  `CodeCallIdentities` (ADR 0117 §2), the one derivation both Lashlang
  hosts also mint from. Each admission carries a fingerprint (BLAKE3 over
  the canonical request) that the admission row retains, so a resumed run that
  asks something different at an admitted ordinal fails with
  `RetainedRequestDrift` instead of reusing the recorded answer.
- **Fencing.** A message whose lease, owner epoch, frame epoch or sequence
  is not the next one is a protocol violation, and so is a request id that
  does not advance. The worker is discarded; nothing it sent after the
  violation is acted on.
- **Recovery goes through the substrate.** When a worker is lost (a crash,
  EOF, an unresponsive transport or a violation), the broker kills it,
  settles the operation already admitted, within `BrokerBounds`, and returns
  `BrokerFailure::WorkerLost`, which is retryable. The run resumes from its
  last committed snapshot, and each admitted operation's saved outcome is fed
  back in with no second dispatch. The broker never restarts a run, rewinds a counter or
  retries an effect locally. A partial frame is refused and the last
  checkpoint kept, and a fully received `Complete` wins over a later EOF.
- **Park on effect releases the slot.** When a parkable request's effect
  needs a worker of its own (nested compilation, a nested run, the body of
  an awaited process), the broker answers `Park`. The worker serializes the run. The broker holds that
  continuation in memory, and the slot goes back to the pool before the effect
  is performed; a crash meanwhile resumes from the last committed snapshot. On the
  outcome the broker checks a worker out again, resumes the continuation,
  and hands the held outcome to the reissued request, matched by
  fingerprint. A pool of one slot therefore completes a nested
  compilation, or a cell that awaits a process it started, instead of
  deadlocking.
- **Cancellation.** A stop sends `Cancel` and waits the grace period
  before a physical kill. The run ends `Cancelled` only when the cancellation
  was observed at a recorded instruction checkpoint (ADR 0039);
  otherwise it ends `Interrupted`. The kill itself decides nothing.
- **Frames.** `VmSession::open_frame` advances the frame fence, opens the
  frame in the checkpoint store and waits, bounded, until every run of an
  earlier frame has retired. A run whose frame is retired is killed and
  ends `FrameRetired`, and the new frame starts from a store that holds no
  guest state of the old one, so its globals are undefined.
- **Checkpoints.** `CheckpointStore::commit` stores the VM bytes, the
  ledger and the frame epoch together; it refuses a commit from a retired
  frame, and an identical re-commit is a no-op. `BrokerBounds::standard()`
  uses a 30-second settle deadline and a one-second cancellation grace.

The conformance laws `vm_broker_tests!` use an in-process worker double. The
store matrix is SQLite memory, SQLite file and PostgreSQL; laws run the
production runtime over a fault-injecting store with labelled commits, a
virtual clock and `SimNodes` (ADR 0132 §14). Upgrade proofs use the
synthetic-next tier. The broker law macro registers:

- `worker_kill_before_start_runs_no_effect`
- `worker_kill_mid_compute_redrives_through_the_substrate`
- `worker_kill_after_request_before_record_settles_the_admitted_operation_once`
- `worker_kill_after_record_before_delivery_replays_with_zero_dispatch`
- `worker_kill_mid_serialization_keeps_the_last_checkpoint`
- `worker_kill_after_complete_before_commit_commits_once`
- `unauthorized_worker_effect_request_is_refused_without_invoking_a_tool`
- `stale_epoch_and_duplicate_worker_messages_are_refused`
- `a_refused_run_is_terminal_and_a_broken_exchange_is_redriven`
- `cancellation_winner_is_the_journaled_checkpoint_across_worker_kill`
- `frame_open_retires_worker_state_and_old_globals_are_undefined`
- `one_slot_nested_effect_does_not_deadlock`

### 9. The worker entry and pool

`lash-vm-client` launches an explicitly configured worker entry with an empty
environment. The entry closes inherited descriptors except its IPC socket
before running the worker server. The pool bounds checkout, queue size,
process count, deadlines, cumulative CPU and replacement attempts. Clean
release resets the worker; a crash, protocol failure or exhausted limit
discards it. Pool accounting stays parent-owned across resume.

Async compiler and guest-state callers use `ServiceRuntimeOps::request_accounted`.
It checkpoints worker accounting around a blocking-pool task; the synchronous
checkout and framed exchange are private to that seam. Cold pool creation also
runs on the blocking pool. Pool saturation remains a typed host failure across
plugin and tool-attempt boundaries, so the attempt is recomputed or parks
without recording a tool refusal. Guest compile refusals remain tool results.

This provides crash containment, rather than an OS sandbox. Lash installs no
namespaces, seccomp, Landlock or cgroups. A native VM escape has the worker
user's OS access. Both adapters run model code through this process boundary.

Sources: `crates/lash-vm-client/src/ipc.rs:26`,
`crates/lash-vm-worker/src/entry.rs:26`, and
`crates/lash-vm-client/src/pool.rs`.

### Presets and host packaging

FIG-4162 retains the standard pool bounds: one prewarmed process, at most four
processes, two queued inputs and eight MiB queued bytes. Protocol bounds remain
four MiB frames, two MiB state, one MiB effect payloads, 64 KiB source, 100,000
decoded nodes and 64 MiB charged decode allocation. VM admission allows fifty
million instructions, 64 MiB guest memory and 1,024 frames. Checkout and IPC
silence are five seconds, compute thirty seconds, serialization five seconds,
cancellation grace 100 ms, cumulative CPU ten seconds, and attempts three.
The restart window admits eight failed replacements per minute.

The facade's `WorkerTuning::standard()` retains the working preset: 16 KiB
inbound buffering, 64 entries per linked-cell and compiled-process cache,
8 MiB parser stack base plus 40,000 bytes per source byte, GC every 1,024
allocations, cooperative yields every 1,024 instructions and an initial
adaptive cancellation gap of 2^20 instructions. The immutable 2^28 gap
backstop and decoder structural refusal limits remain. Parser stack slope is
measured; the other cadence/capacity values have no workload measurements.
The parent sends this resolved policy in the worker bootstrap. Worker response
and projection writes use the parent's serialization deadline, and worker
reads use the configured parent wait (standard 86,400 seconds), including
projection responses. The factory and hand-built process engine accept
`VmSegmentPolicy::standard()` overrides: the provisional preset is 120 seconds,
three attempts and immediate retries, with no workload measurement.

These bounds limit resource admission; the synthetic service-time matrix does
not prove an arbitrary-guest deadline or optimal host concurrency. Existing RLM
and process fixtures need the service's explicit larger profile: 64 MiB state,
128 MiB frames and queued input, and 256 MiB charged decode allocation. Small
echo workloads do not justify reducing that valid-state envelope. Hosts select
their own configuration when their workload needs different bounds.

Releases bundle an explicitly selected optimized helper, JSON diagnostics and
reference SDK sources with checksums. Registry consumers build without the
reference tree. The host passes `WorkerEntry::helper(path)` or
`Service::subprocess(path)`; the SDK default names the helper beside the host
executable, with no PATH or monorepo discovery. `--version` reports the protocol
range, crate version, target and build flags. Admission compares only protocol
versions before guest work. A single executable registers the facade's early
re-exec entry and frontend before constructing its runtime, credentials or
stores, as `crates/lash/examples/worker_host.rs` demonstrates.

The optimized same-machine matrix records 10,000 paired fresh-state observations
per workload and 200 cold helper starts. Warm zero-resource-effect overhead is
0.155 ms p50 and 0.475 ms p99, within the 1/5 ms diagnostic budget. Scalar
one-effect exchange is 105/1,281 microseconds, over the 100/500 microsecond
budget; nine of ten exchange populations exceed at least one threshold.
The whole-batch and large-value measurements include guest work and
serialization. The matrix uses the canonical optimized workspace feature
graph, including testing features, without invoking test controls. Shared-host
load, sample counts, complete tails and memory counters are recorded in
`crates/lash-perf/src/vm_worker_matrix/measurements-20260930.json` and its
Markdown companion. These measurements support bounded admission and keep
latency failures visible; they do not set a portable CI latency gate.

Sources: `crates/lash-vm-client/src/config.rs:52`,
`crates/lash-vm-client/src/service.rs:442`, `scripts/package_vm_worker.py:18`,
and `.github/workflows/release.yml:689`.

## Consequences

Opaque records let the broker store and fence state without restoring guest
values. A reset drops all guest-derived state by construction. Field-by-field
cleanup could miss a newly added state owner; keeping grants in returned guest
state would let that state replace parent-owned bindings.

RLM cells, durable process bodies, process creation, artifact inspection and
semantic guest-state restoration run through the shared worker service.
Parent adapters retain opaque VM bytes and worker-verified structural metadata.

## Implementation

- `crates/lashlang/src/runtime/instance.rs` owns the instance;
  `:65` resets by replacement and `:106` owns semantic state decoding.
- `crates/lash-protocol-rlm/src/executor/state/worker_envelope.rs` separates
  guest bytes from parent authority.
- `crates/lash-vm-protocol/src/state.rs` defines opaque state;
  `crates/lash-vm-protocol/src/codec.rs` defines framing and decode charges.
- `crates/lash-vm-broker/src/broker.rs` defines worker-loss recovery;
  `:391` releases a slot for a nested effect without committing the park.
- `crates/lash-protocol-rlm/src/executor/mod.rs` and
  `crates/lash-lashlang-runtime/src/process.rs` broker worker execution.
- `crates/lash-vm-worker/src/service.rs` owns source compilation and artifact
  inspection; `scripts/check-vm-parent-paths.py` checks the production inventory.
- `crates/lash-typescript/tests/corpus_laws/vm_instance.rs` pins reset and
  step/resume equivalence; the former conformance broker registration is
  retired. `crates/lash-vm-worker/tests/pool_laws.rs`
  exercises the physical pool.

### Durable worker accounting

Cells and process bodies reserve their parent-admitted code scope on the
backend before model work starts. The reservation records consumed attempts,
known cumulative CPU, unknown CPU attempts, and whether a worker is active.
Worker release settles measured CPU before parent callbacks. Park and resume
keep the same attempt. A failed or interrupted active attempt requires a
replacement attempt when the run resumes; the reservation prevents a resumed
run from receiving fresh counters. The epoch fence prevents a stale owner from
overwriting the current owner's totals.

Parent loss during an active attempt records its CPU as unknown. It consumes
an attempt without inventing measured usage or preventing resume. Repeated
losses exhaust the typed attempt bound. Each attempt has a CPU cap, so the
worst-case work bound is `(attempts × per-attempt CPU cap) + known CPU`. Known
CPU and consumed attempts remain monotone across resumes. The VM snapshot
carries the totals consumed so far, so a resumed run's first reservation
seeds its scope with them before any worker launches. SQLite and PostgreSQL
store this accounting beside the snapshot.

The reservation is live accounting, read outside every deciding transaction,
so its answer decides nothing that is committed (ADR 0105 §1). A process body
whose reservation is refused fails its attempt retryably and issues no
operation. A body whose budget stays exhausted parks with `ActivationLoop`
once its activation budget is spent (ADR 0132 §3).

The same holds for every verdict of the host's worker budget, pool capacity or
recovery store, wherever it is met: a refused reservation, a deadline or the
cumulative CPU or attempt bound met mid-run, a full queue, a checkout timeout
or a restart storm. Another node with capacity would not reach it, so it is
never an execution's recorded outcome. A process body fails its attempt
retryably. An RLM cell fails its attempt retryably too: it commits nothing,
the model never sees the verdict, and the turn commits nothing after it, not
even its cancellation reading, since the retry runs the cell again from the
last committed snapshot. Only a limit the run itself exhausted — fuel, heap,
frame depth, its observation stream, or its encoded effect values, VM state
and frames, measured against their bounds, is recorded: the process terminal
`process_execution_bound_exhausted`, or the cell's program failure (FIG-4451,
FIG-4458, FIG-4475, FIG-4476).

A retryable worker infrastructure failure follows the same rule during cell
setup as during execution. A crashed or unresponsive worker, or a broken
protocol, fails the attempt retryably, including during source
analysis and compilation. `PoolError::is_host_verdict` uses the infrastructure
outcome's retryability classification. The cell records no Host failure for
it, and its retry runs the same cell on a replacement worker (FIG-4459). A
`RunRefused` is not retryable: the cell records its Host failure, since a
retry would meet the same refusal (FIG-4645).

A missing or unexecutable configured worker is a typed `WorkerDeployment`
fault carrying the executable path. The shared spawn seam classifies ENOENT,
EACCES and ENOEXEC; synchronous and async pool callers retain that cause.
It is non-transient and records no guest result. The turn writes a durable
`WorkerDeployment` park on its first refusal, keeping its committed state. No
Lash transient retry budget is spent. Supplying an executable at that path
lets an operator redrive the same run to completion (FIG-4776).

An owned child also installs a kernel CPU ceiling before guest work, from its
current process CPU and the configured execution CPU budget. It remains
in force across every effect response and if the parent dies. Only reset for a
new checkout installs another ceiling. Waiting for a parent response settles
known CPU and clears the active marker before any host callback; resuming
computation marks it active again. The kernel rounds CPU limits to seconds, so the
per-attempt cap used in the bound above is the configured cumulative CPU budget
plus one second. The ceiling bounds computation. The worker has the filesystem
and network access of its OS user.

### Await and batch parking

A cell or body that awaits a process handle, a tuple, list or record of them,
or a resource-operation batch parks on that await and releases its worker, so
the awaited body or nested work takes the slot. `EffectKind::parkable` and
`VmRequest::parkable` admit `Await` and `ResourceOperationBatch`; the runtime
adapter parks every parkable request. A hand-over answer on these paths is the
park, never a guest error.

An aggregate await stays one host await per handle, in traversal order. When
the host parks the run on one handle, the continuation's resume point is
`VmSuspendedOperation::Await { settled }`: the awaited value stands on the
operand stack and, when `settled` is not zero, a list of the `settled` leaf
results the run already received stands above it. The resumed run walks the
same value again, takes those results for its first leaves without asking the
host, and issues exactly the await it parked on, which the broker answers with
its held outcome. A continuation carries at most
`VM_PARKED_AWAIT_SETTLED_LIMIT` (1024) settled results; past the bound the
capture declines and the host answers the await in place. Decoding refuses a
larger count, and a reissued await whose carried results do not match its walk
is refused.

This representation keeps the host contract and the admitted operations
unchanged: each handle keeps its own admitted operation and ordinal, and a
straight-through run and a parked one issue the same awaits in the same
order, so a run restored from its snapshot after a parent crash feeds each
admitted await its saved outcome. Two alternatives are
rejected. Reissuing the whole aggregate on resume would await settled handles
again, so the reissued request would not match the held operation and
each resume would admit fresh ordinals. One host await over the whole
aggregate would change the `Await` ability for every host and merge
separately admitted waits into one operation.

A resource-operation batch is one host operation, so it parks as
`VmSuspendedOperation::ResourceOperationBatch` and reissues the same batch.

The native laws in `crates/lash-vm-worker/tests/pool_laws.rs`
(`one_slot_process_await_releases_worker_for_the_awaited_body`,
`one_slot_aggregate_process_await_parks_on_every_pending_handle`,
`a_parked_aggregate_await_resumes_after_a_parent_crash_with_the_same_result`,
`one_slot_resource_operation_batch_parks_and_resumes`), the VM laws in
`crates/lashlang/src/runtime/tests/await_park_cases.rs`, and the RLM law
`one_slot_cell_that_starts_and_awaits_a_process_completes` in
`crates/lash-protocol-rlm/src/executor/tests/one_slot_process_await.rs`
pin this behaviour.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
