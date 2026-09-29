# 0123: Model code runs in resettable worker processes

## Status

Accepted 2026-09-29 (FIG-4158, the first lane of FIG-3821). This lane lands
the VM side of the decision: one owned, resettable instance, the owned
step/resume interface, the authority split, opaque parent-side VM state and
the parent-worker protocol types. The broker, the worker entry and pool, the
move of both adapters into workers and the deletion of every in-parent
parse, compile and execute path follow in their own lanes under FIG-3821,
which stays open until the last of them lands.

Sam's rulings of 2026-09-29 bind this decision:

1. **The boundary is the language, plus crash containment.** The VM has no
   ambient authority, and every effect it asks for goes through the parent.
   The worker process contains crashes and keeps host credentials out of the
   memory model code runs in: it starts with an empty environment and no
   inherited descriptors but its pipe. There are no namespaces, Landlock,
   seccomp, cgroups or fork server, so it is portable across Linux and
   macOS. Lash never claims an OS-level security boundary: a native escape
   from the VM reaches whatever the worker's OS user can reach.
2. **Workers are reused after a reset.** A pool reuses a worker across
   sessions once it is reset, so the VM is built for a reset that is clean
   by construction and provable by law, not cleaned up field by field.
3. **Whole-hog.** RLM code cells and durable Lashlang process bodies both
   run in workers in 1.0, and so does every parse, link, compile and module
   creation of model source. There is no in-process mode, and the in-parent
   paths are deleted, not kept as a fallback.

## Context

Model code ran in the host process. Its guest state was spread across the
RLM execution state, the process engine and one process-wide static (the
record symbol interner). Its parked continuations and snapshots crossed
between components as values the parent decoded semantically: restoring a
heap compiles its regular expressions, so the parent compiled
model-authored patterns just to read state it only needed to store. The RLM
snapshot envelope carried the guest heap next to the parent's grants,
including each grant's host-owned `execution_binding`, so anything that
could read or write that envelope could read or replace authority.

## Decision

### 1. One instance owns every guest-derived byte

`lashlang::VmInstance` owns the session's globals, heap and roots, the
execution scratch, the linked and compiled cell cache, and any run in
flight with its operand stacks, pending handles and suspension. Nothing
guest-derived lives in a `static`, `thread_local!`, `OnceLock` or
`LazyLock`:

- Record keys are content-addressed `Symbol`s (`&'static str` constants or
  shared `Arc<str>` text), compared by pointer then by text. The global
  interner is gone.
- `scripts/check-vm-static-state.py` refuses a static in lashlang,
  lash-typescript, lash-lashlang-runtime or the worker crate unless
  `scripts/vm-static-state-allowlist.txt` names it with a reason. The
  allowlist holds build tables projected from constant lists, test-only
  counters, parent-side telemetry, and the heap's write-stamp counter,
  which is never guest-readable, persisted or sent. A stale or unreasoned
  entry is refused too.
- Third-party interning below the parser (swc's atoms) is outside the VM
  crates and carries no guest-readable state; it is recorded here, not
  allowlisted.

### 2. A reset is a drop and replace

`VmInstance::reset` drops the instance and installs `VmInstance::pristine()`,
the one constructor every fresh and every reset instance comes from. It
constructs the instance fresh: FIG-4157 measured fresh construction faster
than cloning a prebuilt template in both of its populations, so there is no
template. It clears nothing field by field, so state added to the instance
later is covered by the same drop. The laws:

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
  `ReissueOperation { operation }` when a signal wait was handed to a
  successor and the continuation must issue it again. Resuming checks that
  the instruction pointer stands on the operation the discriminant names.

The law `step_resume_matches_straight_through_for_every_corpus_program`
drives every corpus program step by step, and in process mode parks it at
every boundary and reopens each continuation from its bytes on a pristine
instance, and compares both with the program run straight through.

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

To the parent, VM state is `lash_vm_protocol::OpaqueVmState`: a kind, an
owner, the exact VM contract (`lashlang::vm_contract_identity()`), a format
version, a length, a BLAKE3 digest and the bytes. The parent checks those
structurally and nothing else. The semantic decoders, which restore guest
values and compile regular expressions, are reachable only through
`VmInstance` (`open_continuation`, `open_snapshot`,
`restore_durable_parts`): `VmContinuation` no longer implements
`Deserialize`, and the protocol crate depends on nothing that could decode
the bytes. `parent_state_decode_never_compiles_regexp` pins both halves: the
parent's decode of a segment whose continuation holds an invalid pattern
succeeds, and only the worker's open refuses it.

### 6. The protocol

`lash-vm-protocol` defines what a parent and its worker exchange, and no
transport or pool:

- **Messages.** Parent to worker: `Start` (the program source, explicit
  context descriptions, and a fresh session or opaque state), `EffectResult`,
  `Cancel`, `Reset`, `Shutdown`. Worker to parent: `Ready`, `EffectRequest`,
  `Suspended`, `Complete`, `GuestError`, `Cancelled`, `ResetDone`.
- **Headers.** Every message carries its execution lease, owner and frame
  epochs and a transport sequence. `MessageFence` admits only the next
  sequence of the current lease and epochs.
- **Framing.** A frame is the magic `LVMP`, the 32-byte digest of the exact
  `BuildIdentity`, a big-endian length and the payload. A frame from another
  build is refused; there is no negotiation and no historical reader, since
  shapes change in place under the version freeze.
- **Bounded decoding.** Decoding charges frame size, nesting depth, node
  count and cumulative allocation against `DecodeLimits` before it
  allocates, and a malformed, oversized, wrong-build, truncated or trailing
  frame is a typed `CodecRefusal`. Every value is a charged node, map keys
  included.
- **Bounds.** `ProtocolBounds` states every bound a host holds its workers
  to, with no implicit default. Its `standard()` preset takes FIG-4157's
  measurements: 4 MiB frames; 100,000 decode nodes, about twice the densest
  measured continuation, whose decode alone reached about 446 ms; 64 MiB of
  cumulative decode allocation, about 3.6 times the largest measured; 2 MiB
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

## Consequences

- The parent can store, route and fence VM state it never decodes, and a
  worker can be replaced mid-session by replaying the journal into a
  pristine instance.
- Until the adapters move into workers, the parent calls the worker-side
  functions in process. They are the only code that decodes guest state,
  so moving them behind the pipe changes no parent code path.
- Pre-warming the process-wide interner is gone with the interner.
- The boundary statement is honest: a worker contains crashes and keeps
  credentials out of reach of model code, and the language keeps authority
  in the parent. Neither is an OS sandbox.
