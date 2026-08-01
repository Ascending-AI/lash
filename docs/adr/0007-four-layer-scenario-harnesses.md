# Four Layer Scenario Harnesses

## Status

accepted

## Decision

Lash testing uses four scenario harness layers with explicit ownership: `lash-core` owns protocol-agnostic Runtime Scenarios, `lash-protocol-standard` owns Standard Protocol Scenarios, `lash-protocol-rlm` owns RLM Protocol Scenarios, and `lash` owns facade-level Agent Scenarios. This keeps core runtime invariants independent of protocol semantics while still giving Standard, RLM, and full agent behavior their own deterministic regression surfaces.

## Why

The runtime must protect durable session, effect, queue, lease, checkpoint, and observation behavior without learning RLM, Lashlang, or Standard tool-loop details. Conversely, protocol behavior should not rely only on full facade e2e tests, because failures in response classification, streaming, repair loops, or history rendering need small deterministic reproductions. Facade Agent Scenarios remain necessary for composition: builders, plugins, tools, subagents, process graphs, and app-facing final values.

## Consequences

- Existing e2e tests in `lash` stay and become the seed of the Agent Scenario Harness rather than being deleted.
- Existing `lash-core` runtime tests migrate gradually into Runtime Scenarios where that reduces duplication or strengthens invariants; focused unit tests remain focused.
- Standard and RLM get separate protocol scenario harnesses instead of leaking protocol-specific cases into `lash-core`.
- `lash-perf` reports group measured cases by the same four named scenario concepts (`Runtime Scenario`, `Standard Protocol Scenario`, `RLM Protocol Scenario`, and `Agent Scenario`) while remaining separate from correctness harness ownership.

## Harness Homes

- Runtime Scenarios use `crates/lash-core/src/runtime/tests/runtime_scenarios.rs` as the module root, with cases in `runtime_scenarios/cases.rs` and private support modules under `runtime_scenarios/support/`. They are named ingress, checkpoint, claim, lease, fault, and commit phases. Run with `cargo test -p lash-core runtime_scenario`.
- Standard Protocol Scenarios live in `crates/lash-protocol-standard/tests/protocol_scenarios.rs`. Run with `cargo test -p lash-protocol-standard --test protocol_scenarios`.
- RLM Protocol Scenarios live under `crates/lash-protocol-rlm/tests/protocol_drivers/`, with the `protocol_drivers.rs` test root declaring `support`, `scenarios`, `prompt_history`, and `driver_mechanics` as sibling modules. Run with `cargo test -p lash-protocol-rlm --test protocol_drivers`.
- Agent Scenarios live in `crates/lash/src/tests/agent_scenarios/`. Run with `cargo test -p lash-runtime --features rlm,testing agent_scenarios`.

## Ownership Boundaries

- Runtime Scenarios own protocol-agnostic host/runtime invariants: queued-work ordering, active-checkpoint process-wake eligibility, session-command gates, turn-input redrive/cancel behavior, checkpoint deferral, observation replay, queue completion, and representative lease/fence rejection or reclaim at the runtime persistence boundary.
- Persistence conformance owns backend permutations for the same storage concepts: source-key idempotence, cross-session isolation, claim expiry/reclaim, backend-specific fence behavior, and schema compatibility.
- Full runtime tests in `crates/lash-core/src/runtime/tests/turns.rs` own live scheduler behavior that cannot be reduced to a store-level scenario: public `stream_next_queued_work` return shape, provider prompt contents, event streams, phase-probe lease loss, plugin checkpoint hooks, process-wake history, and cancellation through an active turn.
- Facade turn-streaming tests own app-facing stream and projection behavior at the `lash` API boundary.
- CLI e2e tests own rendered UI/operator behavior and process-level invocation of the binary.

## Scenario Coverage Index

The canonical case-to-boundary index lives with the scenario code so renames and ownership changes are reviewed with the tests:

| Harness | Code-owned index |
| --- | --- |
| Runtime Scenario | `RUNTIME_SCENARIO_COVERAGE` in `crates/lash-core/src/runtime/tests/runtime_scenarios/cases.rs` |
| Standard Protocol Scenario | `STANDARD_PROTOCOL_SCENARIO_COVERAGE` in `crates/lash-protocol-standard/tests/protocol_scenarios.rs` |
| RLM Protocol Scenario | `RLM_PROTOCOL_SCENARIO_COVERAGE` in `crates/lash-protocol-rlm/tests/protocol_drivers/scenarios.rs` |
| Agent Scenario | `AGENT_SCENARIO_COVERAGE` in `crates/lash/src/tests/agent_scenarios/cases.rs` |

Each index records the test name, scenario display name, and owned invariant/boundary. Coverage entries are created with macros that derive `test_name` from the actual test function identifier and store a function pointer, so deleting or renaming a listed test fails compilation instead of leaving a stale string behind. Each harness also has a metadata test requiring unique names and non-empty ownership text. RLM Protocol Scenario tests use the `rlm_protocol_scenario_*` prefix; prompt/history focused checks use `RLM_PROMPT_HISTORY_FOCUSED_CHECKS` and the `rlm_prompt_history_*` prefix in `protocol_drivers/prompt_history.rs` so they stay visibly outside the user-visible protocol scenario index. Other focused exceptions remain explicit in their modules: RLM white-box driver mechanics live in `protocol_drivers/driver_mechanics.rs`, and live scheduler/provider/stream runtime assertions stay in `crates/lash-core/src/runtime/tests/turns.rs`.

`lash-perf` uses `RuntimePerfScenario::METADATA` as the single source for scenario name, execution mode, scenario harness kind, classification rationale, and direct correctness coverage links when a measured path has an obvious scenario counterpart. The runtime-perf report tests include a golden projection for the four harness groups so the CLI-facing report shape cannot silently drop Runtime, Standard Protocol, RLM Protocol, or Agent Scenario grouping.

## Behavior Transcripts

When one of these harnesses ships an inline expect test, it renders into the
shared behavior-transcript vocabulary of
`lash_core::testing::behavior_transcript` (ADR 0050) rather than a private
format. The Standard and RLM harnesses share one projection from the sans-io
effect stream (`lash_core::testing::sansio_transcript`); the Agent harness
projects streamed `TurnActivity`, observed checkpoint commits, and the terminal
process fold. Ownership boundaries above are unchanged: a transcript is added
alongside a scenario's assertions, never in place of them.

## Review Workflow

When new scenario files are still untracked, use intent-to-add for review visibility without staging content for commit:

```sh
git add -N crates/lash-core/src/runtime/tests/runtime_scenarios.rs \
  crates/lash-core/src/runtime/tests/runtime_scenarios/support.rs \
  crates/lash-core/src/runtime/tests/runtime_scenarios/support/*.rs \
  crates/lash-core/src/runtime/tests/runtime_scenarios/cases.rs \
  crates/lash-protocol-standard/tests/protocol_scenarios.rs \
  crates/lash-protocol-rlm/tests/protocol_drivers.rs \
  crates/lash-protocol-rlm/tests/protocol_drivers/*.rs \
  crates/lash/src/tests/agent_scenarios \
  docs/adr/0007-four-layer-scenario-harnesses.md
git diff
```

Do not assume staged-only diffs are the review surface for this migration; `git diff` with intent-to-add shows the new files and rename context while leaving actual content unstaged.

## Agent Scenario Seeds

The original `lash_e2e_*` cases seeded the Agent Scenario Harness and now use `agent_scenario_*` test names and `agent-scenario-*` session ids:

| Legacy e2e seed | Agent Scenario |
| --- | --- |
| `lash_e2e_foreground_labeled_tool_call` | `agent_scenario_foreground_labeled_tool_call` |
| `lash_e2e_started_process_labeled_tool_call` | `agent_scenario_started_process_labeled_tool_call` |
| `lash_e2e_process_durable_input_request_tool` | `agent_scenario_process_durable_input_request_tool` |
| `lash_e2e_shell_nonzero_and_pipeline_results_are_data` | `agent_scenario_shell_nonzero_and_pipeline_results_are_data` |
| `lash_e2e_shell_output_survives_print_projection_in_variable` | `agent_scenario_shell_output_survives_print_projection_in_variable` |
| `lash_e2e_started_process_labeled_subagent_spawn` | `agent_scenario_started_process_labeled_subagent_spawn` |
| `lash_e2e_nested_process_start_await` | `agent_scenario_nested_process_start_await` |
| `lash_e2e_session_turn_process_child` | `agent_scenario_session_turn_process_child` |
| `lash_e2e_failed_child_preserves_failure_graph` | `agent_scenario_failed_child_preserves_failure_graph` |
| `lash_e2e_parallel_spawn_and_join` | `agent_scenario_parallel_spawn_and_join` |
| `lash_e2e_tuple_values_finish_as_json_arrays` | `agent_scenario_tuple_values_finish_as_json_arrays` |

## Focused Tests That Stay Focused

- `lash-core` persistence conformance tests keep backend trait guarantees such as source-key idempotence, cross-session isolation, and backend-specific fence behavior. Runtime Scenarios cover cross-cutting host/runtime invariants, not every backend permutation.
- Pending-input cancellation is intentionally covered at both levels with different ownership: Runtime Scenarios assert host-level redrive/cancel behavior such as active input deferral and later idle-claim suppression, while persistence conformance keeps storage-level permutations such as source-key replay, cross-session isolation, claim expiry, and backend fence semantics.
- `lash-core` small runtime tests keep narrow turn-loop, projection, tracing, and assembler assertions when a scenario would obscure the single invariant being tested.
- `lash-protocol-rlm` direct `TurnMachine` tests stay for malformed turn options, checkpoint restore, and driver-state ownership mutation because those tests intentionally inspect or corrupt white-box state.
- `lash-protocol-standard` focused native-tool and builder tests stay outside Standard Protocol Scenarios when they validate a single helper or internal projection rule rather than an end-to-end protocol loop.
- `lash` facade tests that require full plugins, tools, process graphs, subagents, or app-facing final values belong to Agent Scenarios; smaller CLI/config/turn-streaming tests are not part of this harness migration.
