# 0141: A model-started process's environment is the host start policy's sealed choice

## Status

Accepted.

## Context

A process is separate from its starting session. Its environment must survive
session changes and deletion, and a resumed declaration must retain the choice
its starter committed. Choosing from live session defaults during realization
would change that choice after a crash.

## Decision

### 1. The host supplies the start policy

A model start uses a required `ProcessEnvPolicy`. It receives `ProcessEnvCx`
and returns the complete environment choice and engine settings. The host
registers it with the session's process controls, and with process controls
components when nested model starts are offered. Lash fills nothing from a
session or deployment default.

`ResolvedStartTarget` is authoritative: lash reads the immutable definition,
verifies any signature claim, resolves the engine payload and exposes the
engine's signature before calling the policy. The policy sees the definition
id, engine kind, payload and signature that will actually run, rather than a
model's description of them (D8).

### 2. Choosing may await reads

`ProcessEnvPolicy::choose` is async because a host may read a definition's
metadata, an enclosing process's recorded environment, or durable integration
bindings to make its choice. Those reads are derivation; the policy performs
no effects. Executable process components bind live services lazily after the
choice has been recorded.

### 3. The committed decision is final

Declaration holds the environment through `hold_process_env`, validates the
stated settings and seals a `DeclaredLaunch`. Its ref and settings commit with
the tool outcome. Realization, restart and adoption read that declaration and
never run its policy again (D9).

Before that commit, an abandoned attempt may run the policy again. No child
registers from the abandoned attempt. `ProcessEnvPolicyError::Refused` is the
tool's refusal and records no start. `Fault`, including a store fault while
holding the choice, is an attempt fault: it retries without committing a
refusal. A changed policy cannot alter a declaration that already committed.

### 4. Inheritance and presets are explicit choices

The host may choose `policy::from_run` with its own ordered component mapping,
`policy::restrict_or` inside a process (the enclosing environment restricted
to the selected target's reached operation uses),
`policy::published`, or `policy::refuse`. None is a default. `cx.engine_defaults()` is also an explicit
choice, sealed as settings at declaration, rather than consulted at realization.
A host start states `EngineLaunch` itself.

### 5. One route per operation in each process

Each process routes each operation to exactly one provider and tool. The
immutable execution environment records routes per `OperationUse` (defining
Link and Import slot), preserving contract and deferred-resolution identity;
all uses of one operation in a process must select the same provider and tool.
Two incompatible selections refuse before environment publication with
`EmbedError::ConflictingOperationRoutes`, naming the operation and both uses.
The model sees no single name for two downstreams. A host needing two accounts
in one process publishes two operation names.

A workflow started by a cell is its own self-contained process, with its own
recorded routes (ADR 0011). The cell may route `inbox.send` to one provider
while the workflow routes its `inbox.send` to another. The child's host start
policy states that choice; no live parent route overrides it.

`HostArtifacts::inspect_target` accepts `LinkTarget::Linked(LinkedStart)` or
`LinkTarget::Definition(ProcessDefinitionId)` and returns the ordered
`LinkInspection` for the complete retained execution, including captured
code's defining Links. `link_process_env` consumes the same target and
inspection, applies explicit selections by `OperationUse`, and validates
complete link coverage. Hosts publish the resulting environment and supply
its reference through `EngineLaunch`. Inspection publishes nothing and takes
no lasting hold. The implementation is `crates/lash/src/artifacts.rs` and
`crates/lash-vm-runtime/src/process/documents.rs`.

Fulfilment is checked over the reached Link DAG, including deferred grants;
extra visible package members do not widen the selected execution. Opaque
capability refs are compared for equality, never presumed narrower.

## Consequences

The process builds only its ordered named components. Session-turn processes
have no process environment and run their child session's configuration.
Narrowing a session leaves its running processes' environments unchanged.
Hosts own authorization and revocation: lash exposes recorded authority and
comparison helpers, and imposes no process authority gate.

[ADR 0004](0004-process-environments-carry-plugin-options-not-product-metadata.md)
owns component configuration; [ADR 0011](0011-self-contained-processes.md)
owns the self-contained record. Implementation:
[policy and declaration](../../crates/lash-core-execution/src/runtime/process/env_policy.rs),
[environment publication](../../crates/lash-core-execution/src/runtime/process/env_hold.rs),
and [model starts](../../crates/lash-plugin-process-controls/src/declarations.rs) and
[policy choices](../../crates/lash-plugin-process-controls/src/lib.rs).
