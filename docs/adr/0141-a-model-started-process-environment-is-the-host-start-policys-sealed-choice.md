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
`cx.restrict()` inside a process (FIG-6074: the enclosing environment
restricted to the effects the target performs), `policy::restrict_or`,
`policy::published`, or `policy::refuse`. None is a default. `cx.engine_defaults()` is also an explicit
choice, sealed as settings at declaration, rather than consulted at realization.
A host start states `EngineLaunch` itself.

A process environment links each effect its process performs to one tool,
exactly: a start whose environment links an effect the process does not
perform, or leaves one unlinked, is refused (FIG-6074, P1). Opaque
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
and [model starts](../../crates/lash-plugin-process-controls/src/declarations.rs).
