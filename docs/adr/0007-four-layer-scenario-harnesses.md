# Four layer scenario harnesses

## Status

accepted

## Decision

Lash has four scenario layers with explicit ownership. `lash-core` owns protocol-agnostic Runtime Scenarios; `lash-protocol-standard` owns Standard Protocol Scenarios; `lash-protocol-rlm` owns RLM Protocol Scenarios; `lash-facade-tests` owns facade Agent Scenarios. Runtime laws stay independent of protocol semantics while protocols and facade composition have their own regression cases.

## Why

Runtime invariants need narrow reproductions at their owning boundary. Protocol response classification, streaming, repair and history rendering also need direct scenarios. Agent Scenarios exercise builders, plugins, tools, child sessions, process graphs and app-facing final values together.

## Harness homes

| Harness | Root and coverage index |
| --- | --- |
| Runtime | `crates/lash-core/tests/runtime/tests/runtime_scenarios.rs`; runtime scenario tests |
| Standard Protocol | `crates/lash-protocol-standard/tests/protocol_scenarios.rs`; standard protocol scenario tests |
| RLM Protocol | `crates/lash-protocol-rlm/tests/protocol_drivers.rs`; RLM driver scenarios |
| Agent | `crates/lash-facade-tests/tests/facade_turns/tests/agent_scenarios.rs`; served-node agent scenarios |

The concrete test registrations determine which scenarios execute. Focused
prompt/history and driver tests retain their own narrower subjects; a prose
layer inventory is not an executable coverage registry.

## Ownership boundaries

Runtime Scenarios own admission ordering, checkpoint behavior, commands, cancellation, observation and commits. Persistence conformance owns backend permutations. The storage matrix is SQLite file, SQLite memory and PostgreSQL. Laws run the production runtime over a fault-injecting store with labelled commits, a virtual clock and `SimNodes` ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §14). Upgrade proofs use the synthetic-next tier. These are evidence dimensions, not extra scenario layers.

Focused scheduler, provider, stream, projection and helper tests remain focused where a full scenario obscures their invariant. Facade cases needing plugins, tools, process graphs or final values belong to Agent Scenarios.

## Behavior transcripts and consequences

Inline expect tests use `lash_core::testing::behavior_transcript` under ADR 0050. Standard and RLM share the sans-io transcript projection; Agent Scenarios project activity and process results. Transcripts supplement behavioral assertions. `lash-perf` groups measurements by the same four scenario concepts, while correctness ownership remains with these harnesses.
