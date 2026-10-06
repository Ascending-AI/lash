# 0094: Child lifecycle is a registration fact settled by scope end

Status: Accepted

## Context

Process provenance describes where work comes from and where observations go.
It does not determine cleanup. A child's eventual lifetime must be recorded
independently of a transient tool attempt or worker teardown.

## Decision

A process record carries a required `LifetimeDecision` and admitted ancestry.
`Until { scope, grant }` requests cooperative cancellation when the named scope
closes; `Detached` names no ending scope. The model does not choose a lifetime.
The runtime materializes `StartCx` from admitted ancestry and evaluates the
host's policy once. Resume reads the recorded decision instead of re-running
policy. Root starts may be detached or use a host-looked-up session scope.

`ScopeId` is an effect opener or a session. A process opener names the minted
`ProcessId`, which identifies one lifetime. A logical turn uses its run scope;
a session scope closes at deletion. Registration validates the selected grant
against ancestry. These scope rules belong to
[ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md), and process
identity belongs to
[ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md).

Evidence: `crates/lash-core-execution/src/runtime/process/model/scope_lifetime.rs`, and
`crates/lash-core-execution/src/runtime/process/model/start_request.rs`.

### Storage and scope-close ledger

`ParentEndPlan` carries the closed `ScopeId`, end time, settlement time and a
durable cascade cursor. It contains no child action list. Applying it queries
live children whose lifetime is `Until` that scope and which have no
cancellation request. The ledger retains its typed scope independently of the
parent process row.

Registration and the close-ledger check share one transaction. A new `Until`
child encountering the end fact is refused with `ParentEnded`; a repeated
registration returns its existing record. A child committed before closure is
found by the plan's lifetime query. The owner's terminal transaction records
the scope's end in the same transaction as its outcome
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §4 and §11).

Application is batched: each transaction writes a cancel request row into a
bounded set of children's mailboxes, waking them, and advances the plan's
cursor; no single statement walks a large tree (ADR 0132 §11). A crash leaves
the cursor at the last committed batch, so the next owner continues from it.
The per-scope/per-child request identity and settle marker are idempotent.
Settlement does not await child termination.

Evidence: `crates/lash-core-execution/src/runtime/process/registry.rs`,
`crates/lash-core-execution/src/runtime/process/parent_end.rs`,
`crates/lash-core-execution/src/runtime/process/scope_close.rs`,
`crates/lash-sqlite-store/src/process_registry/registration.rs`, and
`crates/lash-postgres-store/src/postgres/process_registry.rs`.

### Stop and Cancel Origin

Cancellation is cooperative and remains distinct from turn cancellation.
Immediate stop requests cancellation of the awaited process with `TurnStopped`;
after-step stop follows
[ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md).
Other processes follow their own recorded lifetimes.

`CancelRequest` records `origin`, `requester`, and `requested_at_ms`. The origins
are `TurnStopped`, `ParentEnded`, `OperatorRequested`, `ModelRequested`, and
`StartFailed`. The first request wins. The same origin and requester repeat as
a no-op without comparing a fresh timestamp; a different request or a terminal
row follows the typed refusal path. The cancellation family is version 3 under
`lash.process-cancellation-request`; its preimage contains process id, the
permanent origin tag, and requester, with no timestamp.

Evidence: `crates/lash-sansio/src/tool_output.rs`,
`crates/lash-core-execution/src/runtime/process/validation.rs`, and
`crates/lash-core-execution/src/runtime/process/events.rs`.

### Start compensation

Lash's durable engine owns process execution under
[ADR 0110](0110-the-engine-owns-process-recovery.md). Registration inserts a
runnable process actor in the transaction that records the start (ADR 0132 §5
and §12), so there is no submission that can fail after registration. A start
that fails before its transaction commits leaves no row. A `StartFailed`
request against a never-started row makes it Cancelled. A registration
conflict cannot license cancellation of another start's record.

Evidence: `crates/lash-core-execution/src/runtime/process/validation.rs`.

### 12. An opener close cancels waits, not registered processes

Logical Run Closing stops new tool admission, settles protected obligations
and releases source subscriptions. A process realized by a declared start has its own
registered lifetime. Closing the Run or cancelling `processes.await(job)`
does not itself request cancellation of `job`. The process can nevertheless be
cancelled when its recorded `Until` scope closes. Work that must survive the
turn needs a lifetime that reaches beyond that turn.

A dead worker or node is neither scope closure nor opener close.
Finalization settles protected obligations, commits the outcome and records
scope closure, then completes retirement. Recovery resumes the first step
whose commit is missing. The Run ownership contract belongs to
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md).

Evidence: `crates/lash-core-execution/src/runtime/process/scope_close.rs` and
`crates/lash-core-execution/src/runtime/process/parent_end.rs`.

## Alternatives considered

Turn-local child action lists cannot settle a child registered around closure
or survive worker loss. A ledger query over recorded lifetimes covers both. A
single-statement cascade over a large tree holds locks too long; a durable
cursor bounds each transaction.
Provenance-based cleanup would make observation relations control lifetime.

## Consequences

- All start surfaces record an inspectable lifetime.
- Scope closure and registration have one race rule.
- Cancellation has a stable cause, requester, and first timestamp.
- Detached work remains host-managed; retries follow each execution's recorded policy.
