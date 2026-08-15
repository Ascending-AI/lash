#[cfg(feature = "rlm")]
use super::super::*;
#[cfg(feature = "rlm")]
use super::contracts::{
    GraphContract, assert_all_processes_terminal, assert_failed_code_block_present,
    assert_graph_lineage_connected, assert_labeled_resource_operation,
    assert_no_duplicate_label_step, assert_no_false_finishted_success,
    assert_no_forbidden_error_text, assert_subagent_bridge_exec_graphs,
};
#[cfg(feature = "rlm")]
use super::harness::{
    AgentScenario, lashlang_block, run_agent_direct_completion_attempt_retry_scenario,
    run_agent_durable_input_request_scenario, run_agent_process_llm_query_scenario,
    run_agent_session_turn_process_scenario, run_agent_turn_scenario,
    run_agent_turn_scenario_without_success_assertions,
};
#[cfg(feature = "rlm")]
use super::process_parent_atomicity::agent_scenario_public_process_parents_are_literal_and_crash_atomic_on_postgres;
#[cfg(feature = "rlm")]
use super::transcript::agent_scenario_transcript;
#[cfg(feature = "rlm")]
use lash_core::llm::types::LlmUsage;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug)]
struct AgentScenarioCoverage {
    test_name: &'static str,
    #[cfg(feature = "rlm")]
    declared_test: fn() -> Result<()>,
    scenario_name: &'static str,
    owned_boundary: &'static str,
}

macro_rules! agent_scenario_coverage {
    ($test_fn:ident, $scenario_name:literal, $owned_boundary:literal) => {
        AgentScenarioCoverage {
            test_name: stringify!($test_fn),
            #[cfg(feature = "rlm")]
            declared_test: $test_fn,
            scenario_name: $scenario_name,
            owned_boundary: $owned_boundary,
        }
    };
}

const FOREGROUND_LABELED_TOOL_CALL: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_foreground_labeled_tool_call,
    "foreground labeled tool call",
    "Facade root turn, app tool execution, label graph, final value, and remote DTO round trip."
);
const STARTED_PROCESS_LABELED_TOOL_CALL: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_started_process_labeled_tool_call,
    "started process labeled tool call",
    "Started Lashlang process calling an app tool with process graph completion."
);
const DURABLE_INPUT_REQUEST: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_process_durable_input_request_tool,
    "durable input suspension",
    "Live durable input suspension, external resolution, process event, and final value."
);
const PROCESS_LLM_QUERY: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_process_llm_query_with_typed_output,
    "process llm query with typed output",
    "A Lashlang process can await ordinary llm.query structured output end to end."
);
const DIRECT_COMPLETION_ATTEMPT_RETRY: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_direct_completion_attempt_retry_reinvokes_provider_once,
    "direct completion atomic attempt retry",
    "A tool-attempt retry re-executes its opaque direct completion exactly once."
);
const SHELL_RESULTS_ARE_DATA: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_shell_nonzero_and_pipeline_results_are_data,
    "shell nonzero and pipeline results are data",
    "Shell failures and pipelines remain data at the facade boundary."
);
const SHELL_OUTPUT_VARIABLE: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_shell_output_survives_print_projection_in_variable,
    "shell output survives print projection in variable",
    "Large shell output survives print projection and remains addressable."
);
const STARTED_PROCESS_SUBAGENT: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_started_process_labeled_subagent_spawn,
    "started process labeled subagent spawn",
    "Started process spawns a subagent and records child session execution graphs."
);
const NESTED_PROCESS_START_AWAIT: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_nested_process_start_await,
    "nested process start await",
    "Nested process start/await produces deterministic process ids and graph lineage."
);
const SESSION_TURN_PROCESS_CHILD: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_session_turn_process_child,
    "session turn process child",
    "Host session-turn process API creates and awaits a child session turn."
);
const FAILED_CHILD_PRESERVES_GRAPH: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_failed_child_preserves_failure_graph,
    "failed child preserves failure graph",
    "Child failure path preserves failure graph and avoids provider-exhaustion false failures."
);
const PARALLEL_SPAWN_AND_JOIN: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_parallel_spawn_and_join,
    "parallel process spawn and join",
    "Parallel process starts join deterministically with unique process ids."
);
const TUPLE_VALUES_AS_JSON_ARRAYS: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_tuple_values_finish_as_json_arrays,
    "tuple values finish as json arrays",
    "Facade final values preserve tuple-to-JSON array projection."
);
const POSTGRES_PROCESS_PARENT_ATOMICITY: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_public_process_parents_are_literal_and_crash_atomic_on_postgres,
    "PostgreSQL process-parent atomicity",
    "Facade worker, Standard plugin, Lashlang process graph, and durable PostgreSQL ParentEnd fault recovery."
);
const FIG1293_MIGRATED_TOOL_COMPOSITION: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_fig1293_migrated_tool_composition,
    "FIG-1293 migrated tool composition",
    "Facade composition of tracked and detached shell starts, stdin signalling, process cancellation, subagent spawn/await, and protocol batch."
);

const AGENT_SCENARIO_COVERAGE: &[AgentScenarioCoverage] = &[
    FOREGROUND_LABELED_TOOL_CALL,
    STARTED_PROCESS_LABELED_TOOL_CALL,
    DURABLE_INPUT_REQUEST,
    PROCESS_LLM_QUERY,
    DIRECT_COMPLETION_ATTEMPT_RETRY,
    SHELL_RESULTS_ARE_DATA,
    SHELL_OUTPUT_VARIABLE,
    STARTED_PROCESS_SUBAGENT,
    NESTED_PROCESS_START_AWAIT,
    SESSION_TURN_PROCESS_CHILD,
    FAILED_CHILD_PRESERVES_GRAPH,
    PARALLEL_SPAWN_AND_JOIN,
    TUPLE_VALUES_AS_JSON_ARRAYS,
    POSTGRES_PROCESS_PARENT_ATOMICITY,
    FIG1293_MIGRATED_TOOL_COMPOSITION,
];

#[test]
fn agent_scenario_coverage_metadata_is_unique_and_complete() {
    assert_eq!(AGENT_SCENARIO_COVERAGE.len(), 15);
    let mut names = BTreeSet::new();
    for coverage in AGENT_SCENARIO_COVERAGE {
        #[cfg(feature = "rlm")]
        let _declared_test = coverage.declared_test;
        assert!(
            coverage.test_name.starts_with("agent_scenario_"),
            "unexpected Agent Scenario test name {}",
            coverage.test_name
        );
        assert!(!coverage.scenario_name.trim().is_empty());
        assert!(!coverage.owned_boundary.trim().is_empty());
        assert!(
            names.insert(coverage.test_name),
            "duplicate Agent Scenario coverage metadata for {}",
            coverage.test_name
        );
    }
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_fig1293_migrated_tool_composition() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-fig1293-composition", || async {
        run_agent_turn_scenario(
            AgentScenario::new(
                FIG1293_MIGRATED_TOOL_COMPOSITION.scenario_name,
                "Exercise the complete FIG-1293 migrated tool composition.",
            )
            .responses([
                lashlang_block(
                    r#"
tracked = await shell.start({ cmd: "cat", login: false })?
written = await shell.write({ process_id: tracked.process_id, chars: "fig1293\n" })?
cancelled = await processes.cancel({ process_id: tracked.process_id })?
detached = await shell.start({ cmd: "sleep 1", login: false, detach: true })?
child = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: Type { len: int }
})?
batched = await tools.batch({ tool_calls: [
  { tool: "app_lookup", parameters: {} },
  { tool: "app_lookup", parameters: {} }
] })?
finish {
  tracked_running: tracked.running,
  write_status: written.status,
  write_has_sequence: written.sequence > 0,
  cancel_status: cancelled.status,
  detached_status: detached.status,
  detached_done: detached.done,
  child_len: child.len,
  batch_count: len(batched.results)
}"#,
                ),
                lashlang_block("finish { len: len(chunk) }"),
            ])
            .expected_final_value(serde_json::json!({
                "tracked_running": true,
                "write_status": "signalled",
                "write_has_sequence": true,
                "cancel_status": "cancelled",
                "detached_status": "detached",
                "detached_done": true,
                "child_len": 2,
                "batch_count": 2
            }))
            .tool_provider(Arc::new(AppTools))
            .install_subagents()
            .install_shell_processes(),
        )
        .await?;
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_foreground_labeled_tool_call() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-foreground-tool", || async {
        let case = AgentScenario::new(
            FOREGROUND_LABELED_TOOL_CALL.scenario_name,
            "Call the app lookup tool and finish its value.",
        )
        .response(lashlang_block(
            r#"
@label(title: "Lookup app state")
value = await tools.app_lookup({})?
finish value"#,
        ))
        .expected_final_value(serde_json::json!({ "ok": true }))
        .tool_provider(Arc::new(AppTools))
        .labeled_resource("Lookup app state");

        let run = run_agent_turn_scenario(case).await?;
        assert_eq!(run.prompt_captures.len(), 1);
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_started_process_labeled_tool_call() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-started-process-tool", || async {
        let run = run_agent_turn_scenario(
            AgentScenario::new(
                STARTED_PROCESS_LABELED_TOOL_CALL.scenario_name,
                "Start a process that calls the app lookup tool.",
            )
            .response(lashlang_block(
                r#"
process lookup(tools: Tools) {
  @label(title: "Lookup app state in process")
  value = await tools.app_lookup({})?
  finish value
}
handle = start lookup(tools: tools)
result = (await handle)?
finish result"#,
            ))
            .response_usage(LlmUsage {
                input_tokens: 11,
                output_tokens: 7,
                cache_read_input_tokens: 3,
                cache_write_input_tokens: 2,
                reasoning_output_tokens: 4,
            })
            .expected_final_value(serde_json::json!({ "ok": true }))
            .tool_provider(Arc::new(AppTools))
            .labeled_resource("Lookup app state in process")
            .completed_process("lookup")
            .min_completed_process_graphs(1),
        )
        .await?;
        insta::assert_snapshot!(agent_scenario_transcript(&run, "root"), @r#"
        root         ingress   turn.start
        root         provider  model.request           iteration=0
        root         exec      cell.start              lang="lashlang"
        root         exec      cell.ok                 calls=1
        root         outcome   turn.final_value        value={"ok":true}
        root         commit    checkpoint.commit       rev=0->1
        root                     usage                 entries=1 input=11 output=7 cache_read=3 cache_write=2 reasoning=4 total=23
        root                     turn_state            stored logical=387B
        root                     tool_state            stored logical=<opaque>
        root                     plugin_snapshot       stored logical=346B
        root                     execution_state       stored logical=unknown
        process-001  outcome   process.completed       label="lookup" kind="lashlang" terminal=true
        "#);
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_process_durable_input_request_tool() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-durable-input-request", || async {
        run_agent_durable_input_request_scenario().await
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_process_llm_query_with_typed_output() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-process-llm-query", || async {
        run_agent_process_llm_query_scenario().await
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_direct_completion_attempt_retry_reinvokes_provider_once() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-direct-completion-attempt-retry", || async {
        run_agent_direct_completion_attempt_retry_scenario().await
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_shell_nonzero_and_pipeline_results_are_data() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-shell-results-are-data", || async {
        run_agent_turn_scenario(
            AgentScenario::new(
                SHELL_RESULTS_ARE_DATA.scenario_name,
                "Run shell commands and report their result metadata.",
            )
            .response(lashlang_block(
                r#"
pipe = await shell.exec({ cmd: "yes line | head -n 3", login: false })?
missing = await shell.exec({ cmd: "test -f /tmp/agent-scenario-definitely-missing-file", login: false })?
finish {
  pipe_exit: pipe.exit_code,
  pipe_output: pipe.output,
  missing_exit: missing.exit_code,
  missing_status: missing.status
}"#,
            ))
            .expected_final_value(serde_json::json!({
                "pipe_exit": 0,
                "pipe_output": "line\nline\nline\n",
                "missing_exit": 1,
                "missing_status": "completed"
            }))
            .tool_provider(Arc::new(lash_tools::shell::shell_provider(
                lash_tools::shell::StandardShell::new(),
            ))),
        )
        .await?;
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_shell_output_survives_print_projection_in_variable() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-shell-output-variable", || async {
        run_agent_turn_scenario(
            AgentScenario::new(
                SHELL_OUTPUT_VARIABLE.scenario_name,
                "Run a large shell command, inspect it, then report retained metadata.",
            )
            .responses([
                lashlang_block(
                    r#"
big = await shell.exec({ cmd: "yes x | head -c 60000", login: false })?
print big.output"#,
                ),
                lashlang_block(
                    r#"
finish {
  chars: len(big.output),
  tail: slice(big.output, 59996, null),
  has_full_output_path: big.full_output_path == null ? false : len(big.full_output_path) > 0
}"#,
                ),
            ])
            .expected_final_value(serde_json::json!({
                "chars": 60000,
                "tail": "x\nx\n",
                "has_full_output_path": true
            }))
            .tool_provider(Arc::new(lash_tools::shell::shell_provider(
                lash_tools::shell::StandardShell::new(),
            ))),
        )
        .await?;
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_started_process_labeled_subagent_spawn() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-started-process-subagent", || async {
        let run = run_agent_turn_scenario(
            AgentScenario::new(
                STARTED_PROCESS_SUBAGENT.scenario_name,
                "Run a Lashlang process that spawns a subagent and returns its value.",
            )
            .responses([
                lashlang_block(
                    r#"
process spawn_child() {
  @label(title: "Spawn subagent with web search")
  result = await agents.spawn({
    capability: "default",
    task: "Finish `{ len: len(chunk) }` using the seeded `chunk` variable.",
    seed: { chunk: ["a", "b"] },
    output: Type { len: int }
  })?
  finish result
}
handle = start spawn_child()
result = (await handle)?
finish result"#,
                ),
                lashlang_block("finish { len: len(chunk) }"),
            ])
            .expected_final_value(serde_json::json!({ "len": 2 }))
            .install_subagents()
            .labeled_resource("Spawn subagent with web search")
            .completed_process("spawn_child")
            .min_completed_child_session_exec_graphs(1)
            .min_completed_process_graphs(1),
        )
        .await?;
        insta::assert_snapshot!(agent_scenario_transcript(&run, "root"), @r#"
        root         ingress   turn.start
        root         provider  model.request           iteration=0
        root         exec      cell.start              lang="lashlang"
        root         exec      cell.ok                 calls=1
        root         outcome   turn.final_value        value={"len":2}
        root         commit    checkpoint.commit       rev=0->1
        root                     usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        root                     turn_state            stored logical=257B
        root                     tool_state            stored logical=<opaque>
        root                     plugin_snapshot       stored logical=342B
        root                     execution_state       stored logical=unknown
        session-001  commit    checkpoint.commit       rev=0->1
        session-001              usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        session-001              turn_state            stored logical=358B
        session-001              tool_state            stored logical=<opaque>
        session-001              plugin_snapshot       stored logical=342B
        session-001  commit    checkpoint.commit       rev=1->2
        session-001              usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        session-001              turn_state            stored logical=358B
        session-001              tool_state            ref (unchanged)
        session-001              plugin_snapshot       ref (unchanged)
        session-001              execution_state       stored logical=unknown
        process-001  outcome   process.completed       label="spawn" kind="subagent" terminal=true
        process-002  outcome   process.completed       label="spawn_child" kind="lashlang" terminal=true
        "#);
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_nested_process_start_await() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-nested-process", || async {
        let run = run_agent_turn_scenario(
            AgentScenario::new(
                NESTED_PROCESS_START_AWAIT.scenario_name,
                "Start a parent process that starts and awaits a child process.",
            )
            .response(lashlang_block(
                r#"
process child() {
  finish { child: "done" }
}
process parent() {
  @label(title: "Start nested child process")
  handle = start child()
  result = (await handle)?
  finish { parent: result.child }
}
handle = start parent()
result = (await handle)?
finish result"#,
            ))
            .expected_final_value(serde_json::json!({ "parent": "done" }))
            .labeled_node("Start nested child process")
            .completed_process("parent")
            .completed_process("child")
            .min_completed_process_graphs(2),
        )
        .await?;
        insta::assert_snapshot!(agent_scenario_transcript(&run, "root"), @r#"
        root         ingress   turn.start
        root         provider  model.request           iteration=0
        root         exec      cell.start              lang="lashlang"
        root         exec      cell.ok                 calls=1
        root         outcome   turn.final_value        value={"parent":"done"}
        root         commit    checkpoint.commit       rev=0->1
        root                     usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        root                     turn_state            stored logical=257B
        root                     tool_state            stored logical=<opaque>
        root                     plugin_snapshot       stored logical=267B
        root                     execution_state       stored logical=unknown
        process-001  outcome   process.completed       label="child" kind="lashlang" terminal=true
        process-002  outcome   process.completed       label="parent" kind="lashlang" terminal=true
        "#);
        assert_lashlang_process_ids_unique_for_labels(&run.final_process_list, ["parent", "child"]);
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_session_turn_process_child() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-session-turn-process", || async {
        run_agent_session_turn_process_scenario().await
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_failed_child_preserves_failure_graph() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-failed-child", || async {
        let run = run_agent_turn_scenario_without_success_assertions(
            AgentScenario::new(
                FAILED_CHILD_PRESERVES_GRAPH.scenario_name,
                "Spawn a child that fails and preserve its execution graph.",
            )
            .responses([
                lashlang_block(
                    r#"
@label(title: "Spawn failing subagent")
result = await agents.spawn({
  capability: "default",
  task: "Fail with reason child boom.",
  seed: {},
  output: Type { reason: str }
})?
finish result"#,
                ),
                lashlang_block(r#"await task.fail({ reason: "child boom" })?"#),
                lashlang_block(r#"await task.fail({ reason: "parent observed child failure" })?"#),
            ])
            .install_subagents()
            .max_turns(1),
        )
        .await?;

        // Expect test first: the failure path's shape is the review artifact —
        // which cell failed, that the child's reason surfaced, and that the
        // parent's processes still folded to a terminal state.
        insta::assert_snapshot!(agent_scenario_transcript(&run, "root"), @r#"
        root         ingress   turn.start
        root         provider  model.request           iteration=0
        root         exec      cell.start              lang="lashlang"
        root         tool      tool.start              name="spawn_agent" call=call-001
        root         tool      tool.result             name="spawn_agent" outcome=failure call=call-001
        root         exec      cell.failed             calls=1 error="`?` unwrapped failed module operation: {"class":"execution","code":"tool…"
        root         provider  model.request           iteration=1
        root         exec      cell.start              lang="lashlang"
        root         exec      cell.failed             calls=0 error="unknown name `task` --> line 1, column 7 await task.fail({ reason: "pare…"
        root         commit    checkpoint.commit       rev=0->1
        root                     usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        root                     turn_state            stored logical=257B
        root                     tool_state            stored logical=<opaque>
        root                     plugin_snapshot       stored logical=342B
        root                     execution_state       stored logical=unknown
        session-001  commit    checkpoint.commit       rev=0->1
        session-001              usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        session-001              turn_state            stored logical=363B
        session-001              tool_state            stored logical=<opaque>
        session-001              plugin_snapshot       stored logical=342B
        session-001  commit    checkpoint.commit       rev=1->2
        session-001              usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        session-001              turn_state            stored logical=363B
        session-001              tool_state            ref (unchanged)
        session-001              plugin_snapshot       ref (unchanged)
        session-001              execution_state       stored logical=unknown
        process-001  outcome   process.failed          label="spawn" kind="subagent" terminal=true
        "#);

        assert_failed_code_block_present(&run.streamed_events);
        assert_no_forbidden_error_text(&run.streamed_events);
        assert!(
            !format!("{:#?}", run.streamed_events)
                .contains("scripted agent scenario provider exhausted"),
            "failed-child scenario must fail through the child task.fail path, not provider exhaustion"
        );
        assert_no_false_finishted_success(&run);
        assert_all_processes_terminal(&run.final_process_list);
        let contract = GraphContract::from_graphs(&run.graph_snapshots);
        assert_labeled_resource_operation(
            &contract,
            "Spawn failing subagent",
            crate::tracing::TraceLashlangNodeStatus::Failed,
        );
        assert_no_duplicate_label_step(&contract, "Spawn failing subagent");
        assert_graph_lineage_connected(&contract, &run.final_process_list);
        assert_subagent_bridge_exec_graphs(
            &run,
            crate::tracing::TraceLanguageExecutionStatus::Completed,
        );

        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_parallel_spawn_and_join() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-parallel-spawn-join", || async {
        let run = run_agent_turn_scenario(
            AgentScenario::new(
                PARALLEL_SPAWN_AND_JOIN.scenario_name,
                "Start two processes, await both, and finish their joined result.",
            )
            .response(lashlang_block(
                r#"
process child(value: str) {
  finish value
}
@label(title: "Start left process")
left = start child(value: "left")
@label(title: "Start right process")
right = start child(value: "right")
left_value = (await left)?
right_value = (await right)?
finish { joined: [left_value, right_value] }"#,
            ))
            .expected_final_value(serde_json::json!({ "joined": ["left", "right"] }))
            .labeled_node("Start left process")
            .labeled_node("Start right process")
            .completed_process("child")
            .min_completed_process_graphs(2),
        )
        .await?;
        // Expect test first: the reviewable artifact is the spawn -> await ->
        // terminal fold plus what each turn actually committed, and a changed
        // shape is easier to judge than the first assertion that trips on it.
        insta::assert_snapshot!(agent_scenario_transcript(&run, "root"), @r#"
        root         ingress   turn.start
        root         provider  model.request           iteration=0
        root         exec      cell.start              lang="lashlang"
        root         exec      cell.ok                 calls=2
        root         outcome   turn.final_value        value={"joined":["left","right"]}
        root         commit    checkpoint.commit       rev=0->1
        root                     usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        root                     turn_state            stored logical=257B
        root                     tool_state            stored logical=<opaque>
        root                     plugin_snapshot       stored logical=267B
        root                     execution_state       stored logical=unknown
        process-001  outcome   process.completed       label="child" kind="lashlang" terminal=true
        process-002  outcome   process.completed       label="child" kind="lashlang" terminal=true
        "#);
        assert_lashlang_process_ids_unique_for_labels(&run.final_process_list, ["child", "child"]);

        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_tuple_values_finish_as_json_arrays() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-tuple-values", || async {
        run_agent_turn_scenario(
            AgentScenario::new(
                TUPLE_VALUES_AS_JSON_ARRAYS.scenario_name,
                "Use tuple values and finish the derived result.",
            )
            .response(lashlang_block(
                r#"
pair = "left", "right"
tail = slice(pair, 1, null)
seen = []
for item in pair {
  seen = push(seen, item)
}
finish {
  first: pair[0],
  tail: tail,
  seen: seen,
  tuple: pair,
  nested: { pair: pair }
}"#,
            ))
            .expected_final_value(serde_json::json!({
                "first": "left",
                "tail": ["right"],
                "seen": ["left", "right"],
                "tuple": ["left", "right"],
                "nested": { "pair": ["left", "right"] }
            })),
        )
        .await?;
        Ok(())
    })
}

#[cfg(feature = "rlm")]
fn assert_lashlang_process_ids_unique_for_labels<const N: usize>(
    processes: &[lash_core::ProcessHandleSummary],
    expected_labels: [&str; N],
) {
    let mut ids = BTreeSet::new();
    let mut labels = Vec::new();
    for process in processes {
        if process.kind != lash_lashlang_runtime::LASHLANG_ENGINE_KIND {
            continue;
        }
        assert!(
            process.process_id.starts_with("process:lashlang:sha256:"),
            "lashlang process `{}` did not use a deterministic process id",
            process.process_id
        );
        assert!(
            ids.insert(process.process_id.as_str()),
            "duplicate lashlang process id `{}`",
            process.process_id
        );
        labels.push(process.label.as_deref().unwrap_or("<missing>"));
    }
    labels.sort_unstable();
    let mut expected = expected_labels;
    expected.sort_unstable();
    assert_eq!(labels, expected);
}
