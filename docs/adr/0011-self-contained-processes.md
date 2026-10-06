# Self-contained processes capture their environment at creation

## Decision

A Runtime Process is a standalone durable record with a minted id, input, captured environment reference, identity, provenance, lifetime, ancestry, events and status. It holds no live reference to its creator's session. Execution reconstructs plugins and policy from the captured immutable environment, rather than reopening the originating session.

Definition starts refer to immutable definitions under ADR 0095. Realization resolves that definition and registers its engine input; the retained record's identity names the definition id. Registry names do not re-resolve an admitted definition. Captured cell locals must be immutable and durably representable.

## Rules and guarantees

Originator and `caused_by` are provenance. Observer edges determine observation, and a wake target routes at most one session wake. These relationships do not imply one another or cleanup. Descendants inherit the root session capability; host-originated roots need no session observer. A process-created session is an ordinary child session with its own usage.

Every registration records a Lifetime over its admitted Ancestry under ADR 0108. Scope-end cleanup follows that explicit lifetime under ADR 0094. Session deletion removes session-owned relationships and deliveries; provenance alone does not cancel a process. Artifact liveness follows referrer edges under ADR 0113. The process actor's committed state owns execution recovery under ADR 0110 and [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md).

## Alternatives and consequences

Live session binding is rejected because session changes would change recovery inputs and prevent session-independent work. An owner enum bundling execution, cleanup and wake routing is rejected because those relationships have independent meanings. An ambient sessionless execution configuration is unnecessary because the process carries its environment.

Processes can exist without sessions and outlive their creators when their recorded lifetime permits it. Arguments and captured specifications provide state handover; mutable creator state does not. The implementation is in [process records and lineage](../../crates/lash-core-execution/src/runtime/process/model.rs), [environment specifications](../../crates/lash-core-store/src/process_identity.rs) and [process execution](../../crates/lash-lashlang-runtime/src/process.rs).
