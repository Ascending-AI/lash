# Randomized simulation harness

## Status

accepted

## Decision

`lash-sim` is an unpublished workspace crate for randomized boundary and fault search. It composes runtime, protocol, agent, provider and persistence contracts and checks execution histories with independent oracles under ADR 0044. Virtual time can skip waits. Seeds select workloads and modeled boundaries; Tokio interleavings are not a deterministic schedule. Failed runs retain full execution histories for diagnosis.

Runtime execution uses `SimEngine`: lash-restate handlers on the in-process Restate server double, with concurrent handlers and SQLite memory as storage. Provider Wire Scripts exercise real provider serialization and parsing through `ScriptedLlmHttpTransport`. Vendor schemas remain the responsibility of provider crates.

## Fault boundaries

Storage faults enter the real backend transaction interfaces through their testing features and the neutral `BackendFaultKind`, `BackendFaultPoint`, `BackendFaultArm` and `BackendFaultObservation` vocabulary. SQLite file, SQLite memory and PostgreSQL are storage variants. Host evidence distinguishes the server double, live Restate and the simulator's in-process effect host. Synthetic-next owns upgrade proofs.

Commit-boundary failure is modeled. Half-transaction row persistence is rejected as a simulated state because the SQL backends commit atomically. A single virtual clock cannot prove disagreement between database and client clocks; database clock contracts require direct backend evidence. Simulation checks its recorded boundaries and actual runtime outcomes rather than inventing SQL execution leases.

## Alternatives and consequences

A custom deterministic executor is rejected because it adds a scheduling contract instead of exercising the runtime's concurrent engine behavior. Live provider calls are rejected as the main search mechanism because they are costly and cannot supply controlled wire failures. Simulation is an evidence dimension over the existing scenario contracts, not a fifth scenario family or part of the published SDK.

Passing runs can use search mode without per-seed artifact packages. Failures produce trace, replay, minimization and history evidence. Confidence selectors choose bounded search budgets under ADR 0008; a seed alone is insufficient to reconstruct a failed interleaving.

[Engine composition](../../crates/lash-sim/src/backend.rs), [virtual clock](../../crates/lash-sim/src/clock.rs), [provider transport](../../crates/lash-sim/src/provider/transport.rs) and [backend fault vocabulary](../../crates/lash-sim/src/backend_fault.rs) implement these boundaries.
