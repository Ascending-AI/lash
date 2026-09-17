#[cfg(feature = "rlm")]
use super::super::*;
#[cfg(feature = "rlm")]
use super::contracts::{
    GraphContract, NodeStatusFact, assert_all_processes_terminal, assert_failed_code_block_present,
    assert_graph_lineage_connected, assert_labeled_resource_operation,
    assert_no_duplicate_label_step, assert_no_false_finishted_success,
    assert_no_forbidden_error_text, assert_subagent_bridge_exec_graphs,
};
#[cfg(feature = "rlm")]
use super::harness::{
    AgentScenario, run_agent_direct_completion_attempt_retry_scenario,
    run_agent_durable_input_request_scenario, run_agent_process_llm_query_scenario,
    run_agent_session_turn_process_scenario, run_agent_turn_scenario,
    run_agent_turn_scenario_without_success_assertions, typescript_block,
};
#[cfg(feature = "rlm")]
use super::plugin_operations::agent_scenario_plugin_task_query_command;
#[cfg(feature = "rlm")]
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
const AWAITED_PROCESS_ATTACHMENT_RETENTION: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_awaited_process_attachment_is_a_parent_commit_gc_root,
    "awaited process attachment retention",
    "An attachment returned only by an awaited child process remains rooted by the parent commit."
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
const PROCESS_TOOL_COMPOSITION: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_process_tool_composition,
    "process tool composition",
    "Facade composition of process cancellation, subagent spawn/await, and protocol batch."
);

const PLUGIN_OPERATIONS: AgentScenarioCoverage = agent_scenario_coverage!(
    agent_scenario_plugin_task_query_command,
    "plugin task query command",
    "Typed facade results, owned operation events, read-only query, and in-flight cooperative cancellation."
);

const AGENT_SCENARIO_COVERAGE: &[AgentScenarioCoverage] = &[
    PLUGIN_OPERATIONS,
    FOREGROUND_LABELED_TOOL_CALL,
    STARTED_PROCESS_LABELED_TOOL_CALL,
    AWAITED_PROCESS_ATTACHMENT_RETENTION,
    DURABLE_INPUT_REQUEST,
    PROCESS_LLM_QUERY,
    DIRECT_COMPLETION_ATTEMPT_RETRY,
    STARTED_PROCESS_SUBAGENT,
    NESTED_PROCESS_START_AWAIT,
    SESSION_TURN_PROCESS_CHILD,
    FAILED_CHILD_PRESERVES_GRAPH,
    PARALLEL_SPAWN_AND_JOIN,
    TUPLE_VALUES_AS_JSON_ARRAYS,
    PROCESS_TOOL_COMPOSITION,
];

#[test]
fn agent_scenario_coverage_metadata_is_unique_and_complete() {
    assert_eq!(AGENT_SCENARIO_COVERAGE.len(), 14);
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
fn agent_scenario_process_tool_composition() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-process-tool-composition", || async {
        run_agent_turn_scenario(
            AgentScenario::new(
                PROCESS_TOOL_COMPOSITION.scenario_name,
                "Exercise process cancellation, subagent spawn/await, and protocol batch.",
            )
            .responses([
                typescript_block(
                    r#"
const worker = async () => {
  await sleep(1000);
  return { done: true };
};
const running = await processes.start({ definition: worker });
const cancelled = await processes.cancel({ process_id: running.process_id });
const child = await agents.spawn({
  capability: "default",
  task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
  seed: { chunk: ["a", "b"] },
  output: { len: "int" }
});
const batched = await tools.batch({ tool_calls: [
  { tool: "app_lookup", parameters: {} },
  { tool: "app_lookup", parameters: {} }
] });
finish({
  cancel_status: cancelled.status,
  child_len: child.len,
  batch_count: batched.results.length
});"#,
                ),
                typescript_block("finish({ len: chunk.length });"),
            ])
            .expected_final_value(serde_json::json!({
                "cancel_status": "cancelled",
                "child_len": 2,
                "batch_count": 2
            }))
            .tool_provider(Arc::new(AppTools))
            .install_subagents()
            .install_process_composition(),
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
        .response(typescript_block(
            r#"
/** @label Lookup app state */
const value = await tools.app_lookup({});
finish(value);"#,
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
            .response(typescript_block(
                r#"
const lookup = async () => {
  /** @label Lookup app state in process */
  const value = await tools.app_lookup({});
  return value;
};
const handle = await processes.start({ definition: lookup });
const result = await handle;
finish(result);"#,
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
            .install_process_controls()
            .completed_lifted_processes(1)
            .labeled_resource("Lookup app state in process")
            .min_completed_process_graphs(1),
        )
        .await?;
        insta::assert_snapshot!(agent_scenario_transcript(&run, "root"), @r#"
        root         ingress   turn.start
        root         ingress   queued_input.accepted   inputs=1
        root         provider  model.request           iteration=0
        root         exec      cell.start              lang="typescript"
        root         tool      tool.start              name="start_process" call=call-001
        root         tool      tool.intent             call=call-001 kind="start_process" status="executed"
        root         tool      tool.result             name="start_process" outcome=success call=call-001
        root         exec      cell.ok                 calls=2
        root         outcome   turn.final_value        value={"ok":true}
        root         commit    checkpoint.commit       rev=0->1
        root                     usage                 entries=2 input=11 output=7 cache_read=3 cache_write=2 reasoning=4 total=23
        root                     turn_state            stored logical=357B
        root                     tool_state            stored logical=<opaque>
        root                     plugin_state          stored {"embed_tools":{"generation":0,"values":{}},"lash.triggers":{"generation":0,"values":{}},"processes":{"generation":0,"values":{}},"rlm_protocol":{"generation":0,"values":{}},"tool_output_budget":{"generation":0,"values":{}}}
        root                     execution_state       stored logical=unknown
        process-001  outcome   process.completed       label="__process_<hash>" kind="lashlang" terminal=true
        "#);
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
fn agent_scenario_awaited_process_attachment_is_a_parent_commit_gc_root() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-awaited-process-attachment", || async {
        let attachment = lash_core::facade_support::AttachmentRef::new(
            lash_core::AttachmentId::parse("awaited-child-only").expect("valid attachment id"),
            lash_core::MediaType::parse("image/png").expect("test media type"),
            18,
            Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1))),
            Some("awaited-child-only.png".to_string()),
        );
        let run = run_agent_turn_scenario(
            AgentScenario::new(
                AWAITED_PROCESS_ATTACHMENT_RETENTION.scenario_name,
                "Return the attachment produced by a child process.",
            )
            .response(typescript_block(
                r#"
const handle = { __handle__: "lash", id: "p.1.awaited-attachment-child" };
const attachment = await handle;
finish(attachment);"#,
            ))
            .seeded_attachment_write(
                lash_core::AttachmentId::parse("awaited-child-only").expect("valid attachment id"),
            )
            .precompleted_process(
                "awaited-attachment-child",
                lash_core::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success_tool_value(
                        lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(
                            attachment.clone(),
                        )),
                    ),
                ),
            ),
        )
        .await?;
        let parent_tool_calls = &run
            .turn_output
            .as_ref()
            .expect("root turn completed")
            .tool_calls;

        assert_eq!(
            run.committed_attachment_ids,
            vec![
                lash_core::AttachmentId::parse("awaited-child-only").expect("valid attachment id")
            ]
        );
        assert_eq!(parent_tool_calls.len(), 1);
        assert_eq!(
            parent_tool_calls[0].output.attachments(),
            vec![lash_core::AttachmentSource::stored(attachment)]
        );
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
fn agent_scenario_started_process_labeled_subagent_spawn() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-started-process-subagent", || async {
        let run = run_agent_turn_scenario(
            AgentScenario::new(
                STARTED_PROCESS_SUBAGENT.scenario_name,
                "Run a durable process that spawns a subagent and returns its value.",
            )
            .responses([
                typescript_block(
                    r#"
const spawnChild = async () => {
  /** @label Spawn subagent with web search */
  const result = await agents.spawn({
    capability: "default",
    task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
    seed: { chunk: ["a", "b"] },
    output: { len: "int" }
  });
  return result;
};
const handle = await processes.start({ definition: spawnChild });
const result = await handle;
finish(result);"#,
                ),
                typescript_block("finish({ len: chunk.length });"),
            ])
            .expected_final_value(serde_json::json!({ "len": 2 }))
            .install_subagents()
            .install_process_controls()
            .completed_lifted_processes(1)
            .labeled_resource("Spawn subagent with web search")
            .min_completed_child_session_exec_graphs(1)
            .min_completed_process_graphs(1),
        )
        .await?;
        insta::assert_snapshot!(agent_scenario_transcript(&run, "root"), @r#"
        root         ingress   turn.start
        root         ingress   queued_input.accepted   inputs=1
        root         provider  model.request           iteration=0
        root         exec      cell.start              lang="typescript"
        root         tool      tool.start              name="start_process" call=call-001
        root         tool      tool.intent             call=call-001 kind="start_process" status="executed"
        root         tool      tool.result             name="start_process" outcome=success call=call-001
        root         exec      cell.ok                 calls=2
        root         outcome   turn.final_value        value={"len":2}
        root         commit    checkpoint.commit       rev=0->1
        root                     usage                 entries=1 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        root                     turn_state            stored logical=227B
        root                     tool_state            stored logical=<opaque>
        root                     plugin_state          stored {"lash.triggers":{"generation":0,"values":{}},"processes":{"generation":0,"values":{}},"rlm_protocol":{"generation":0,"values":{}},"subagents":{"generation":0,"values":{}},"tool_output_budget":{"generation":0,"values":{}}}
        root                     execution_state       stored logical=unknown
        session-001  commit    checkpoint.commit       rev=0->1
        session-001              usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        session-001              turn_state            stored logical=354B
        session-001              tool_state            stored logical=<opaque>
        session-001              plugin_state          stored {"lash.triggers":{"generation":0,"values":{}},"processes":{"generation":0,"values":{}},"rlm_protocol":{"generation":0,"values":{}},"subagents":{"generation":0,"values":{}},"tool_output_budget":{"generation":0,"values":{}}}
        session-001  commit    checkpoint.commit       rev=1->2
        session-001              usage                 entries=1 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        session-001              turn_state            stored logical=354B
        session-001              tool_state            ref (unchanged)
        session-001              plugin_state          ref (unchanged)
        session-001              execution_state       stored logical=unknown
        process-001  outcome   process.completed       label="__process_<hash>" kind="lashlang" terminal=true
        process-002  outcome   process.completed       label="spawn" kind="subagent" terminal=true
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
            .response(typescript_block(
                r#"
const grandchild = async () => {
  return { grandchild: "done" };
};
const child = async () => {
  const started = await processes.start({ definition: grandchild });
  const inner = await started;
  return { child: inner.grandchild };
};
const parent = async () => {
  /** @label Start nested child process */
  const started = await processes.start({ definition: child });
  const inner = await started;
  return { parent: inner.child };
};
const handle = await processes.start({ definition: parent });
const result = await handle;
finish(result);"#,
            ))
            .expected_final_value(serde_json::json!({ "parent": "done" }))
            .install_process_controls()
            .completed_lifted_processes(3)
            .observer_visible_processes("lashlang", 3)
            .labeled_node("Start nested child process")
            .min_completed_process_graphs(3),
        )
        .await?;
        insta::assert_snapshot!(agent_scenario_transcript(&run, "root"), @r#"
        root         ingress   turn.start
        root         ingress   queued_input.accepted   inputs=1
        root         provider  model.request           iteration=0
        root         exec      cell.start              lang="typescript"
        root         tool      tool.start              name="start_process" call=call-001
        root         tool      tool.intent             call=call-001 kind="start_process" status="executed"
        root         tool      tool.result             name="start_process" outcome=success call=call-001
        root         exec      cell.ok                 calls=2
        root         outcome   turn.final_value        value={"parent":"done"}
        root         commit    checkpoint.commit       rev=0->1
        root                     usage                 entries=1 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        root                     turn_state            stored logical=227B
        root                     tool_state            stored logical=<opaque>
        root                     plugin_state          stored {"lash.triggers":{"generation":0,"values":{}},"processes":{"generation":0,"values":{}},"rlm_protocol":{"generation":0,"values":{}},"tool_output_budget":{"generation":0,"values":{}}}
        root                     execution_state       stored logical=unknown
        process-001  outcome   process.completed       label="__process_<hash>" kind="lashlang" terminal=true
        process-002  outcome   process.completed       label="__process_<hash>" kind="lashlang" terminal=true
        process-003  outcome   process.completed       label="__process_<hash>" kind="lashlang" terminal=true
        "#);
        assert_lashlang_process_ids_unique_for_labels(
            &run.final_process_list,
            ["parent", "child", "grandchild"],
        );
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
                typescript_block(
                    r#"
/** @label Spawn failing subagent */
const result = await agents.spawn({
  capability: "default",
  task: "Fail with reason child boom.",
  seed: {},
  output: { reason: "str" }
});
finish(result);"#,
                ),
                typescript_block(r#"await task.fail({ reason: "child boom" });"#),
                typescript_block(
                    r#"await task.fail({ reason: "parent observed child failure" });"#,
                ),
            ])
            .install_subagents()
            .expects_refused_cell()
            .max_turns(1),
        )
        .await?;

        // Expect test first: the failure path's shape is the review artifact —
        // which cell failed, that the child's reason surfaced, and that the
        // parent's processes still folded to a terminal state.
        insta::assert_snapshot!(agent_scenario_transcript(&run, "root"), @r#"
        root         ingress   turn.start
        root         ingress   queued_input.accepted   inputs=1
        root         provider  model.request           iteration=0
        root         exec      cell.start              lang="typescript"
        root         tool      tool.start              name="spawn_agent" call=call-001
        root         tool      tool.result             name="spawn_agent" outcome=failure call=call-001
        root         exec      cell.failed             calls=1 failure="program" error="`?` unwrapped failed module operation: child boom --> line 2, column 22 …"
        root         commit    checkpoint.commit       rev=0->1
        root                     usage                 entries=1 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        root                     turn_state            stored logical=227B
        root                     tool_state            stored logical=<opaque>
        root                     plugin_state          stored {"lash.triggers":{"generation":0,"values":{}},"rlm_protocol":{"generation":0,"values":{}},"subagents":{"generation":0,"values":{}},"tool_output_budget":{"generation":0,"values":{}}}
        root                     execution_state       stored logical=unknown
        session-001  commit    checkpoint.commit       rev=0->1
        session-001              usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        session-001              turn_state            stored logical=359B
        session-001              tool_state            stored logical=<opaque>
        session-001              plugin_state          stored {"lash.triggers":{"generation":0,"values":{}},"rlm_protocol":{"generation":0,"values":{}},"subagents":{"generation":0,"values":{}},"tool_output_budget":{"generation":0,"values":{}}}
        session-001  commit    checkpoint.commit       rev=1->2
        session-001              usage                 entries=1 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        session-001              turn_state            stored logical=359B
        session-001              tool_state            ref (unchanged)
        session-001              plugin_state          ref (unchanged)
        session-001              execution_state       stored logical=unknown
        process-001  outcome   process.failed          label="spawn" kind="subagent" terminal=true
        "#);

        assert_failed_code_block_present(&run.streamed_events);
        assert_no_forbidden_error_text(&run.streamed_events);
        let spawn_failure = run
            .streamed_events
            .iter()
            .find_map(|activity| match &activity.event {
                TurnEvent::ToolCallCompleted { name, output, .. } if name == "spawn_agent" => {
                    match &output.outcome {
                        lash_core::ToolCallOutcome::Failure(failure) => Some(failure),
                        _ => None,
                    }
                }
                _ => None,
            })
            .expect("the failed spawn keeps its typed tool projection");
        assert_eq!(spawn_failure.class, lash_core::ToolFailureClass::Execution);
        assert_eq!(spawn_failure.code, "tool_error");
        // FIG-2975: the child's own reason is what the parent reads. Before the
        // runner carried it, every stopped child collapsed onto one sentence.
        assert_eq!(spawn_failure.message, "child boom");
        assert_eq!(spawn_failure.source, lash_core::ToolFailureSource::Tool);
        assert_eq!(spawn_failure.retry, lash_core::ToolRetryStatus::Never);
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
            NodeStatusFact::Failed,
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
            .response(typescript_block(
                r#"
const child = async (value) => {
  return value;
};
/** @label Start left process */
const left = await processes.start({ definition: child, args: { value: "left" } });
/** @label Start right process */
const right = await processes.start({ definition: child, args: { value: "right" } });
const leftValue = await left;
const rightValue = await right;
finish({ joined: [leftValue, rightValue] });"#,
            ))
            .expected_final_value(serde_json::json!({ "joined": ["left", "right"] }))
            .install_process_controls()
            .completed_lifted_processes(2)
            .labeled_node("Start left process")
            .labeled_node("Start right process")
            .min_completed_process_graphs(2),
        )
        .await?;
        // Expect test first: the reviewable artifact is the spawn -> await ->
        // terminal fold plus what each turn actually committed, and a changed
        // shape is easier to judge than the first assertion that trips on it.
        insta::assert_snapshot!(agent_scenario_transcript(&run, "root"), @r#"
        root         ingress   turn.start
        root         ingress   queued_input.accepted   inputs=1
        root         provider  model.request           iteration=0
        root         exec      cell.start              lang="typescript"
        root         tool      tool.start              name="start_process" call=call-001
        root         tool      tool.intent             call=call-001 kind="start_process" status="executed"
        root         tool      tool.result             name="start_process" outcome=success call=call-001
        root         tool      tool.start              name="start_process" call=call-002
        root         tool      tool.intent             call=call-002 kind="start_process" status="executed"
        root         tool      tool.result             name="start_process" outcome=success call=call-002
        root         exec      cell.ok                 calls=4
        root         outcome   turn.final_value        value={"joined":["left","right"]}
        root         commit    checkpoint.commit       rev=0->1
        root                     usage                 entries=1 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
        root                     turn_state            stored logical=227B
        root                     tool_state            stored logical=<opaque>
        root                     plugin_state          stored {"lash.triggers":{"generation":0,"values":{}},"processes":{"generation":0,"values":{}},"rlm_protocol":{"generation":0,"values":{}},"tool_output_budget":{"generation":0,"values":{}}}
        root                     execution_state       stored logical=unknown
        process-001  outcome   process.completed       label="__process_<hash>" kind="lashlang" terminal=true
        process-002  outcome   process.completed       label="__process_<hash>" kind="lashlang" terminal=true
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
            .response(typescript_block(
                r#"
const pair = ["left", "right"];
const tail = pair.slice(1);
const seen = [];
for (const item of pair) {
  seen.push(item);
}
finish({
  first: pair[0],
  tail: tail,
  seen: seen,
  tuple: pair,
  nested: { pair: pair }
});"#,
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
    processes: &[lash_core::ProcessHandleView],
    expected_labels: [&str; N],
) {
    let mut ids = BTreeSet::new();
    let mut labels = Vec::new();
    for process in processes {
        if process.kind != lash_lashlang_runtime::LASHLANG_ENGINE_KIND {
            continue;
        }
        // A leaf `processes.start` derives the child's id from the declaring
        // intent's identity (FIG-2994, ADR 0095), so the deterministic id a
        // redrive reproduces is the tool-intent one, not the retired
        // `process:lashlang:` start-site spelling.
        assert!(
            process.process_id.starts_with("tool-intent:v2:blake3:"),
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
    // #1529 retired the source-level process name: a lifted literal's label is
    // its lift digest. One digest per distinct definition still holds, so the
    // scenario pins how many distinct definitions ran, and how many runs.
    labels.sort_unstable();
    assert_eq!(
        labels.len(),
        expected_labels.len(),
        "expected {} lashlang runs, got {labels:?}",
        expected_labels.len()
    );
    let distinct_labels = labels.iter().collect::<BTreeSet<_>>().len();
    let distinct_expected = expected_labels.iter().collect::<BTreeSet<_>>().len();
    assert_eq!(
        distinct_labels, distinct_expected,
        "expected {distinct_expected} distinct lifted definitions, got {labels:?}"
    );
}
