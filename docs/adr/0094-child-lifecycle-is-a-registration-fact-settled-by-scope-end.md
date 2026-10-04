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
host's policy once. Replay reads the recorded decision instead of re-running
policy. Root starts may be detached or use a host-looked-up session scope.

`ScopeId` is an effect opener or a session. A process opener names the minted
`ProcessId`, which identifies one lifetime. A logical turn uses its run scope;
a session scope closes at deletion. Registration validates the selected grant
against ancestry. These scope rules belong to
[ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md), and process
identity belongs to
[ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md).

Evidence: `crates/lash-core-execution/src/runtime/process/model/scope_lifetime.rs:1`,
`:34`, `:98`, `:132`, `:278`, `:402`, and
`crates/lash-core-execution/src/runtime/process/model/start_request.rs:14`.

### Storage and scope-close ledger

`ParentEndPlan` carries the closed `ScopeId`, end time, settlement time, and its
store-to-engine obligation. It contains no child action list. Applying it
queries live children whose lifetime is `Until` that scope and which have no
cancellation request. The ledger retains its typed scope independently of the
parent process row.

Registration and the close-ledger check share the registry transaction. A new
`Until` child encountering the end fact is refused with `ParentEnded`; replay
of an existing registration returns its existing record. A child committed
before closure is found by the plan's lifetime query. Session-store outcome
and scope-close writes are separate durable steps. The `ScopeClose` and
`ParentEnd` obligations make missing delivery retryable under
[ADR 0109](0109-store-to-engine-delivery-is-an-outbox-of-obligations.md).

Application delivers cancellation to the engine before recording the request
in the registry. Otherwise a crash after the request write could remove the
child from the query before its execution learns about cancellation. The
per-scope/per-child delivery key, request identity, and settle marker are
idempotent. Retryable delivery failure retries the application; a permanent
refusal still records the request. Settlement does not await child termination.

Evidence: `crates/lash-core-execution/src/runtime/process/registry.rs:83`,
`crates/lash-core-execution/src/runtime/process/parent_end.rs:1`, `:75`,
`crates/lash-core-execution/src/runtime/process/scope_close.rs:1`,
`crates/lash-sqlite-store/src/process_registry/registration.rs:51`, and
`crates/lash-postgres-store/src/postgres/process_registry.rs:288`.

### Stop and Cancel Origin

Cancellation is cooperative and remains distinct from turn cancellation.
Immediate stop requests cancellation of the awaited process with `TurnStopped`;
after-step stop follows
[ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md).
Other processes follow their own recorded lifetimes.

`CancelRequest` records `origin`, `requester`, and `requested_at_ms`. The origins
are `TurnStopped`, `ParentEnded`, `OperatorRequested`, `ModelRequested`, and
`StartFailed`. The first request wins. The same origin and requester replay as
a no-op without comparing a fresh timestamp; a different request or a terminal
row follows the typed refusal path. The cancellation family is version 3 under
`lash.process-cancellation-request`; its preimage contains process id, the
permanent origin tag, and requester, with no timestamp.

Evidence: `crates/lash-sansio/src/tool_output.rs:909`, `:920`,
`crates/lash-core-execution/src/runtime/process/validation.rs:236`, and
`crates/lash-core-execution/src/runtime/process/events.rs:960`.

### Start compensation

Restate owns process execution and retries under
[ADR 0110](0110-the-engine-owns-process-recovery.md).
A registered start carries a durable `ProcessStart` delivery obligation.
Scheduling compensates with `StartFailed` only when the current start created
the row and the submission failure proves that no execution is running.
Ambiguous failure or an existing row preserves the record for recovery.
A `StartFailed` request against a never-started row with no external reference
makes it Cancelled. A registration conflict cannot license cancellation of
another start's record.

Evidence: `crates/lash-restate/src/controller/process_scheduling.rs:145`,
`:429`, and `crates/lash-core-execution/src/runtime/process/validation.rs:827`.

### 12. An opener close cancels waits, not registered processes

Logical Run close stops new tool admission and drains protected obligations.
Generic effect-group close releases its Durable Wait children. A process
realized by a declared tool start has its own registered lifetime. Closing the Run or cancelling `processes.await(job)`
does not itself request cancellation of `job`. The process can nevertheless be
cancelled when its recorded `Until` scope closes. Work that must survive the
turn needs a lifetime that reaches beyond that turn.

A dead worker and segment handover are neither scope closure nor opener close.
Finalization settles protected obligations and accounting, commits the outcome,
then records scope closure and completes retirement. Recovery resumes missing
steps. The logical tool lifetime belongs to the
[Tool-run contract](../architecture/tool-run-contract.md).

Evidence: `crates/lash-core-execution/src/runtime/process/scope_close.rs:1`,
`crates/lash-restate/src/process/mod.rs:241`, and
`crates/lash-core-execution/src/runtime/process/parent_end.rs:75`.

## Alternatives considered

Turn-local child action lists cannot settle a child registered around closure
or survive worker loss. A ledger query over recorded lifetimes covers both.
Recording cancellation before engine delivery can hide an undelivered request
from recovery; delivery therefore precedes the registry write.
Provenance-based cleanup would make observation relations control lifetime.

## Consequences

- All start surfaces record an inspectable lifetime.
- Scope closure and registration have one race rule.
- Cancellation has a stable cause, requester, and first timestamp.
- Detached work remains host-managed; retries belong to the engine.
