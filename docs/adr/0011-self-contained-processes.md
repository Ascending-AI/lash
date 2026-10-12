# Self-contained processes run under the environment their starter states

## Decision

A Runtime Process is a standalone durable record with a minted id, input, identity, provenance, lifetime, ancestry, events and status. An engine process records the complete immutable environment and engine settings its starter states in `EngineLaunch`. It builds only the ordered process components that environment names, from the separate process component registry. It holds no live reference to its creator's session.

A session-turn process has no `ProcessEnv` (D5). It starts and runs its child session under the child's own configuration. The child's id is the create request's stated id, or the stable id derived from the process id.

Engine settings are stated with the start (D4). Restart, redrive and adoption load the recorded environment ref and settings; they never run the start policy, select components anew or consult deployment defaults. A build that cannot serve the named components, config formats or unsettled executable revisions does not claim the process to run it. Engine-free cancellation remains available.

Definition starts refer to immutable definitions under ADR 0095. Realization resolves that definition and registers its engine input; the retained record's identity names the definition id. Registry names do not re-resolve an admitted definition. Captured cell locals must be immutable and durably representable.

## Rules and guarantees

Originator and `caused_by` are provenance. Observer edges determine observation. The host chooses notice destinations under ADR 0137. These relationships do not imply one another or cleanup. Descendants inherit the root session capability; host-originated roots need no session observer. A process-created session is an ordinary child session with its own usage.

Every registration records a Lifetime over its admitted Ancestry under ADR 0108. Scope-end cleanup follows that explicit lifetime under ADR 0094. Session deletion removes session-owned relationships and deliveries; provenance alone does not cancel a process. Artifact liveness follows referrer edges under ADR 0113. The process actor's committed state owns execution recovery under ADR 0110 and [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md).

## Alternatives and consequences

Live session binding is rejected because session changes would change recovery inputs and prevent session-independent work. An owner enum bundling execution, cleanup and host routing is rejected because those relationships have independent meanings. An ambient sessionless execution configuration is unnecessary because the process carries its environment.

Processes can exist without sessions and outlive their creators when their recorded lifetime permits it. Arguments and recorded specifications provide state handover; mutable creator state does not. The implementation is in [process records and lineage](../../crates/lash-core-execution/src/runtime/process/model.rs), [process environments](../../crates/lash-core-store/src/process_env.rs) and [process execution](../../crates/lash-vm-runtime/src/process.rs).

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
