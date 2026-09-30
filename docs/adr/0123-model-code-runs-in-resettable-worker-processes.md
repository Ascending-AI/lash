# 0123: Model code runs in resettable worker processes

## Status

Accepted. The VM instance, parent-worker protocol, worker entry, bounded
process pool and broker are implemented. The RLM and process adapters execute
VM functions in the host process; their integration with worker execution is
open work under FIG-3821. This ADR describes the implemented components and
that current adapter boundary.

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

`VmInstance::reset` drops the instance and installs `VmInstance::pristine()`,
the one constructor every fresh and every reset instance comes from. It
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
  cancel checkpoint, or in process mode a segment boundary),
  `VmStep::Parked` (a durable continuation), `VmStep::Complete` or
  `VmStep::GuestError`.
- A resume consumes the answer to the pending request; one that answers a
  different request is refused and leaves the run suspended.
- `VmContinuation` carries `resume: VmResumePoint`: `NextInstruction`, or
  `ReissueOperation { operation, loop_phase }` when the continuation must
  issue an operation again. A signal wait handed to a successor reissues
  its wait. A run parked while awaiting an effect (section 8) reissues the
  resource operation or sleep it was waiting on: the VM rewinds to the
  operation's instruction, pushes its operands back, uncharges the
  instruction and rewinds the call site's occurrence, so the reissued
  request is the one it replaces. A foreground park also records
  `loop_phase` (the instruction budget left until the next yield and the
  last announced cancel checkpoint), so a resumed run meets its cancel
  checkpoints where a run that never parked meets them, and it carries the
  run's expired functions. Resuming checks that the instruction pointer
  stands on the operation the discriminant names.
- A resume may answer a parkable effect request (a resource operation, a
  batch, a sleep or a signal wait) with `VmResume::Park`. The run suspends
  into `VmStep::Parked` with `VmParkReason::AwaitingEffect`, and the parent
  resumes it later from the continuation with the effect's outcome. A run
  that cannot suspend there asks `VmRequest::ParkDeclined` and stays
  suspended on its request.

The law `step_resume_matches_straight_through_for_every_corpus_program`
drives every corpus program step by step, and in process mode parks it at
every boundary and reopens each continuation from its bytes on a pristine
instance, and compares both with the program run straight through.
`effect_park_matches_straight_through_for_every_corpus_program` parks every
corpus program at every parkable effect, reopens it on a pristine instance
and compares it with the program run straight through, and
`effect_park_keeps_cancel_checkpoints_where_an_unparked_run_meets_them`
pins the loop phase.

### 4. Authority stays with the parent

The worker holds the guest heap, roots and scratch, and nothing else. The
parent holds the grants and their `execution_binding`s, trigger routes,
ordinals, incorporation, pending-summary and group ledgers, and `ToolCallId`
derivation (ADR 0117). It authorises every effect request against the
admitted execution context: a worker's identity, receiver bytes or claimed
call site confer nothing. This is lash's internal authority model, not host
policy.

- The RLM durable root is split. The parent hands the worker an
  `RlmWorkerEnvelope` (the durable header and one fragment per binding) and
  reads back an `RlmWorkerCapture` (the header and the changed and
  unchanged fragments). Both deny unknown fields, so a capture that names a
  grant is refused. The root the parent persists takes
  `deferred_resolutions` and `deferred_trigger_resolutions` from the
  parent's own state only.
- A process segment's worker returns the VM bytes of its continuation only;
  the segment's ledgers and route stay in the parent's envelope.
- Laws: `rlm_worker_envelope_carries_no_grant_or_binding` (a sentinel in an
  `execution_binding` never appears in worker-bound or worker-returned
  bytes) and `worker_returned_state_cannot_replace_parent_authority` (a
  forged returned grant is refused, and a guest binding named after an
  authority slot stays a guest binding).

### 5. The parent never decodes VM state

At the broker boundary, VM state is `lash_vm_protocol::OpaqueVmState`: a kind, an
owner, the VM component versions (`lashlang::vm_contract_versions()`), a format
version, a length, a BLAKE3 digest and the bytes. The parent checks those
structurally, admitting each component against its declared read range
(`lashlang::vm_contract_reads()`, ADR 0115). The semantic decoders,
which restore guest values and compile regular expressions, are reachable
only through
`VmInstance` (`open_continuation`, `open_snapshot`,
`restore_durable_parts`): `VmContinuation` exposes no general
`Deserialize` implementation, and the protocol crate depends on nothing that
could decode the bytes. `parent_state_decode_never_compiles_regexp` pins both halves: the
parent's decode of a segment whose continuation holds an invalid pattern
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
- **Framing.** A frame is the magic `LVMP`, the 32-byte digest of the exact
  `BuildIdentity`, a big-endian length and the payload. A frame from another
  build is refused; the protocol has no negotiation, since
  shapes change in place under the version freeze.
- **Bounded decoding.** Decoding charges frame size, nesting depth, node
  count and cumulative allocation against `DecodeLimits` before it
  allocates, and a malformed, oversized, wrong-build, truncated or trailing
  frame is a typed `CodecRefusal`. Every value is a charged node, map keys
  included.
- **Bounds.** `ProtocolBounds` states every bound a host holds its workers
  to, with no implicit default. Its `standard()` preset allows 4 MiB frames;
  nesting depth 128;
  100,000 decode nodes; 64 MiB of cumulative charged decode allocation; 2 MiB
  of VM state; 1 MiB effect values; 64 KiB of source; and a 5-second
  no-response watchdog, which host waits pause and which is not a guest
  execution limit.
- **Infrastructure outcomes.** `WorkerCrashed`, `WorkerUnresponsive`,
  `ProtocolViolation`, `PayloadTooLarge` and `WorkerLimitExceeded` are kept
  apart from guest errors. EOF or exit is supervisor evidence, never worker
  testimony; a fully received `Complete` wins over a later EOF; a partial
  frame is refused and the last committed checkpoint kept.

### 7. Durability

Determinism is not a goal; durability is. When a worker dies, the parent
fences the transport lease, settles every operation it already admitted
within its invocation, and returns a typed retryable infrastructure failure
so the owning substrate invocation is re-driven. The VM and its counters
are rebuilt only by real journal replay, never by resetting ordinals
locally inside a live invocation. A replacement attaches to a still
addressable operation rather than dispatching another, and no recorded
effect executes twice. A checkpoint commits VM bytes and the parent's
counters and ledgers together. Cancellation stays the journaled
instruction-checkpoint observation of ADR 0039: a physical kill after a
grace period does not decide precedence.

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
  checkpoints; prints, finishes, failures, process events and signal waits
  are unsupported request kinds.
- **The parent owns every counter.** `ParentLedger` gives each admitted
  request the next ordinal and derives its `ToolCallId`s through
  `CodeCallIdentities` (ADR 0117 §2), the one derivation both Lashlang
  hosts also mint from. Each admission carries a fingerprint (BLAKE3 over
  the canonical request) that the journal retains, so a re-driven run that
  asks something different at a recorded ordinal fails with
  `RetainedRequestDrift` instead of reusing the recorded answer.
- **Fencing.** A message whose lease, owner epoch, frame epoch or sequence
  is not the next one is a protocol violation, and so is a request id that
  does not advance. The worker is discarded; nothing it sent after the
  violation is acted on.
- **Recovery goes through the substrate.** When a worker is lost (a crash,
  EOF, an unresponsive transport or a violation), the broker kills it,
  settles the operation already admitted, within `BrokerBounds` and in the
  same invocation, and returns `BrokerFailure::WorkerLost`, which is
  retryable. The substrate re-drives the invocation: the new attempt starts
  from the last committed checkpoint, re-admits from the checkpoint's
  ordinals, and gets each recorded answer back from the journal with no
  second dispatch. The broker never restarts a run, rewinds a counter or
  retries an effect locally. A partial frame is refused and the last
  checkpoint kept, and a fully received `Complete` wins over a later EOF.
- **Park on effect releases the slot.** When a parkable request's effect
  needs a worker of its own (nested compilation, a nested run), the broker
  answers `Park`. The worker serializes the run. The broker holds that
  continuation
  within the invocation without committing a checkpoint, and the slot goes
  back to the pool before the effect is performed. On the
  outcome the broker checks a worker out again, resumes the continuation,
  and hands the held outcome to the reissued request, matched by
  fingerprint. A pool of one slot therefore completes a nested
  compilation instead of deadlocking.
- **Cancellation.** A stop sends `Cancel` and waits the grace period
  before a physical kill. The run ends `Cancelled` only when the journal
  observed the cancellation at an instruction checkpoint (ADR 0039);
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

The conformance laws `vm_broker_tests!` use an in-process worker double and
journal effects through the tier's controller. The store matrix is SQLite
memory, SQLite file and PostgreSQL; hosts are the in-process Restate server
double, live Restate and lash-sim's in-process effect host. Upgrade proofs use
the synthetic-next tier. The broker law macro registers:

- `worker_kill_before_start_runs_no_effect`
- `worker_kill_mid_compute_redrives_through_the_substrate`
- `worker_kill_after_request_before_record_settles_the_admitted_operation_once`
- `worker_kill_after_record_before_delivery_replays_with_zero_dispatch`
- `worker_kill_mid_serialization_keeps_the_last_checkpoint`
- `worker_kill_after_complete_before_commit_commits_once`
- `unauthorized_worker_effect_request_is_refused_without_invoking_a_tool`
- `stale_epoch_and_duplicate_worker_messages_are_refused`
- `cancellation_winner_is_the_journaled_checkpoint_across_worker_kill`
- `frame_open_retires_worker_state_and_old_globals_are_undefined`
- `one_slot_nested_effect_does_not_deadlock`

### 9. The worker entry and pool

`lash-vm-worker` launches an explicitly configured entry with an empty
environment. The entry closes inherited descriptors except its IPC socket
before running the worker server. The pool bounds checkout, queue size,
process count, deadlines, cumulative CPU and replacement attempts. Clean
release resets the worker; a crash, protocol failure or exhausted limit
discards it. Pool accounting stays parent-owned across redrive.

This provides crash containment, rather than an OS sandbox. Lash installs no
namespaces, seccomp, Landlock or cgroups. A native VM escape has the worker
user's OS access. Current adapter execution in the host process does not
acquire the worker's process containment merely by using `VmInstance`.

Sources: `crates/lash-vm-worker/src/process.rs:26`,
`crates/lash-vm-worker/src/entry.rs:26`, and
`crates/lash-vm-worker/src/pool.rs:9`.

## Consequences

Opaque records let the broker store and fence state without restoring guest
values. A reset drops all guest-derived state by construction. Field-by-field
cleanup could miss a newly added state owner; keeping grants in returned guest
state would let that state replace parent-owned bindings.

The adapter boundary matters. `RlmExecutionState` owns a `VmInstance` in the
host, and the process adapter compiles and resumes the VM locally. Their
worker-side helper functions delimit semantic decoding, but do not create a
process boundary. The worker and broker are available components; the full
adapter integration depends on FIG-3821.

## Implementation

- `crates/lashlang/src/runtime/instance.rs:36` owns the instance;
  `:65` resets by replacement and `:106` owns semantic state decoding.
- `crates/lash-protocol-rlm/src/executor/state/worker_envelope.rs` separates
  guest bytes from parent authority.
- `crates/lash-vm-protocol/src/state.rs:86` defines opaque state;
  `crates/lash-vm-protocol/src/codec.rs:3` defines framing and decode charges.
- `crates/lash-vm-broker/src/broker.rs:22` defines worker-loss recovery;
  `:391` releases a slot for a nested effect without committing the park.
- `crates/lash-protocol-rlm/src/executor/state.rs:575` and
  `crates/lash-lashlang-runtime/src/process.rs:507` show local adapter execution.
- `crates/lash-typescript/tests/corpus_laws/vm_instance.rs` pins reset and
  step/resume equivalence; `crates/lash-conformance/src/macros/vm_broker.rs:12`
  registers the broker laws; `crates/lash-vm-worker/tests/pool_laws.rs`
  exercises the physical pool.
