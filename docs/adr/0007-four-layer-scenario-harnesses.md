# Four layer scenario harnesses

## Status

accepted

## Decision

Lash has four scenario layers with explicit ownership. `lash-core` owns protocol-agnostic Runtime Scenarios; `lash-protocol-standard` owns Standard Protocol Scenarios; `lash-protocol-rlm` owns RLM Protocol Scenarios; `lash` owns facade Agent Scenarios. Runtime laws stay independent of protocol semantics while protocols and facade composition have their own regression cases.

## Why

Runtime invariants need narrow reproductions at their owning boundary. Protocol response classification, streaming, repair and history rendering also need direct scenarios. Agent Scenarios exercise builders, plugins, tools, subagents, process graphs and app-facing final values together.

## Harness homes

| Harness | Root and coverage index |
| --- | --- |
| Runtime | `crates/lash-core/tests/runtime/tests/runtime_scenarios.rs`; `RUNTIME_SCENARIO_COVERAGE` in `runtime_scenarios/cases.rs` |
| Standard Protocol | `crates/lash-protocol-standard/tests/protocol_scenarios.rs`; `STANDARD_PROTOCOL_SCENARIO_COVERAGE` |
| RLM Protocol | `crates/lash-protocol-rlm/tests/protocol_drivers.rs`; `RLM_PROTOCOL_SCENARIO_COVERAGE` in `protocol_drivers/scenarios.rs` |
| Agent | `crates/lash/src/tests/agent_scenarios/`; `AGENT_SCENARIO_COVERAGE` in `cases.rs` |

Coverage metadata records the test name, display name and owned boundary. Macros derive names and retain test function pointers; metadata checks require unique names and non-empty ownership text. Focused prompt/history and white-box driver tests stay outside the protocol scenario index.

## Review

`scripts/scenario-review.sh` intent-to-adds the four harness roots, so `git diff` shows untracked scenario sources without staging them.

## Ownership boundaries

Runtime Scenarios own admission ordering, checkpoint behavior, commands, cancellation, observation replay and commits. Persistence conformance owns backend permutations. The storage matrix is SQLite file, SQLite memory and PostgreSQL. Host laws run on the in-process Restate server double, live Restate and lash-sim's in-process effect host; upgrade proofs use the synthetic-next tier. These are evidence dimensions, not extra scenario layers.

Focused scheduler, provider, stream, projection and helper tests remain focused where a full scenario obscures their invariant. Facade cases needing plugins, tools, process graphs or final values belong to Agent Scenarios.

## Behavior transcripts and consequences

Inline expect tests use `lash_core::testing::behavior_transcript` under ADR 0050. Standard and RLM share the sans-io transcript projection; Agent Scenarios project activity and process results. Transcripts supplement behavioral assertions. `lash-perf` groups measurements by the same four scenario concepts, while correctness ownership remains with these harnesses.
