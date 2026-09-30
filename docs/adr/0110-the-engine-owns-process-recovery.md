# 0110: The engine owns process recovery; lash never re-runs started work

## Status

Accepted.

## Context

Re-executing a started process without its journal can repeat effects whose
results the process has already lost. The engine owns retry and replay.
The registry records execution facts and terminal evidence; it cannot
reconstruct an engine journal.

## Decision

### 1. Execution ownership comes from the input

`ProcessInput::External` is externally owned. Lash does not execute or
submit it, and its external owner records its completion. An engine input
is work the engine executes. Registration carries no separate recovery
disposition, so ownership cannot contradict the input's class.

Evidence: `crates/lash-core-execution/src/runtime/process/validation.rs:156`
and `crates/lash-restate/src/process/admission.rs:586`.

### 2. Recovery is the engine's replay

A started process resumes through its engine's recorded journal. A fresh
admission that finds execution already started without that journal ends
`Abandoned` with `ResumeRefused { SubstrateLost }`, before executing the
process body. Generation refusal uses the same terminal vocabulary with
`RetiredGeneration`.

Restate journals the admission verdict and nonce. The root's execution-start
write binds attempt 1. Successor segments use their retained handover and
set-if-absent segment-start marker. Retrying the same execution is
idempotent; a successor execution takes the next attempt. The registry
refuses other attempts and refuses starts of external inputs.

Recovery reads live processes in bounded pages and asks Restate about their
current segments. A run that finishes with failure without a process
terminal ends that live process `SubstrateLost`; a process whose own park
refuses remains at that park. If Restate holds no run for the current
segment, recovery resubmits it, with at most one page of resubmissions per
pass. Admission then starts work that has never started or refuses a
started segment whose journal is unavailable. Submission coalesces by
workflow key. Recovery does not re-arm `ProcessStart`.

The terminal transaction arms `ProcessTerminal`, so waiters receive the
terminal even when the segment invocation cannot publish it. A boundary
records its successor's external reference in its journaled handover; a
store failure makes the step retry.

Evidence: `crates/lash-restate/src/process/admission.rs:452`,
`crates/lash-core-execution/src/runtime/process/validation.rs:146`, and
`crates/lash-restate/src/process/park_reconcile.rs:201`.

### 3. Effect implementors own the unjournaled window

An effect can finish before the engine records its result. Its implementor
makes a retry safe using Lash's `ToolCallId`. Lash derives and records that
id for the admitted logical call; crash replay and reported-failure retry
use the same id. Provider call ids are correlation data. [ADR 0117](0117-lash-names-every-tool-call.md)
owns the derivation and recording rules.

Effects are at least once, as specified in [ADR 0042](0042-tool-attempts-are-atomic.md).
`ToolRetryPolicy::Never` disables retry after a reported failure; it cannot
prevent engine replay of an effect whose result was never recorded.
There is no at-most-once marker. A process-engine implementor must journal
its effects through the effect host and respect the same resume refusal.

Evidence: `crates/lash-sansio/src/tool_call_id.rs:270`,
`crates/lash-core-execution/src/tool_provider.rs:1011`, `:1387`, and
`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs:48`.

### 4. The engine bounds retries

Lash carries no process retry budget in registrations or start requests.
Restate's invocation retry policy bounds attempts and pauses an invocation
at exhaustion. Reconciliation parks a live process with
`EngineRetryExhausted`, retaining its journal and writing no terminal.
Resuming that park retries the paused invocation over its journal.

Delivery obligations have their own attempt policy, owned by ADR 0109.
That bounds store-to-engine delivery, not re-execution of the process.

Evidence: `crates/lash-restate/src/process/park_reconcile.rs:94` and
`crates/lash-core-execution/src/runtime/process/model/start_request.rs`.

### 5. Who writes `Abandoned`

`AbandonWriter` is `Producer` or `ResumeRefused { reason }`. The producer
records its own lost-work outcome. Lash writes a resume refusal when it
cannot safely continue the execution.

`ProcessCompletionAuthority` is `ExternalOwner`, `WorkflowKey`, or
`WorkflowKeyRecovery`. Backends validate the authority against the input's
ownership. Recovery also names the segment ordinal, so it cannot end a
process a later segment carries.

Evidence: `crates/lash-core-execution/src/runtime/process/events.rs:71`
and `crates/lash-core-execution/src/runtime/process/events.rs:130`.

### 6. Operators use cancellation or external completion

An operator stops engine-owned work through cancellation. A lost execution
ends through the engine's resume refusal. The external owner, including an
operator acting for that owner, completes an external process with its
observed outcome, including `Abandoned { Producer }`.

These operations record the outcome directly. There is no separate abandon
request waiting for a Lash lease to expire, and closing a host does not
abandon started work.

Evidence: `crates/lash-core-execution/src/runtime/process/registry_concerns.rs:511`
and `crates/lash-core-execution/src/runtime/process/events.rs:130`.

### 7. Restate owns process execution

Restate's process workflow owns production execution and recovery. SQL
stores retain process facts, continuation data and delivery obligations.
A SQL store is not a second execution engine. Tests exercise the contract
through the in-process Restate server double and live Restate; simulation
uses Lash-sim's in-process effect host.

## Consequences

A registry row cannot authorize re-execution from scratch. Losing an engine
journal yields an explicit terminal instead of a best-effort reconstruction.
External ownership is explicit in the input and completion authority.

A per-process recovery disposition would offer different answers to the
same missing-journal problem without providing the journal. An at-most-once
marker would trade an unrecorded result for permanently missing work. The
engine's replay and implementor's call-id idempotency cover those boundaries.

Executable evidence includes
`crates/lash-restate/src/tests/substrate_lost.rs` and
`crates/lash-restate-test/tests/substrate_lost_zombie.rs`.
