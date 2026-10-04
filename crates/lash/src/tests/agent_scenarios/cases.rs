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
    run_agent_process_llm_query_scenario, run_agent_turn_scenario,
    run_agent_turn_scenario_without_success_assertions, typescript_block,
};
#[cfg(feature = "rlm")]
use super::transcript::agent_scenario_transcript;

#[cfg(feature = "rlm")]
struct AgentScenarioCoverage {
    scenario_name: &'static str,
}

#[cfg(feature = "rlm")]
const AWAITED_PROCESS_ATTACHMENT_RETENTION: AgentScenarioCoverage = AgentScenarioCoverage {
    scenario_name: "awaited process attachment retention",
};

#[cfg(feature = "rlm")]
const FAILED_CHILD_PRESERVES_GRAPH: AgentScenarioCoverage = AgentScenarioCoverage {
    scenario_name: "failed child preserves failure graph",
};

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
const handle = { __handle__: "lash", id: "@precompleted-process-handle@" };
const attachment = await handle;
finish(attachment);"#,
            ))
            .seeded_attachment_write(
                lash_core::AttachmentId::parse("awaited-child-only").expect("valid attachment id"),
            )
            .precompleted_process(lash_core::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success_tool_value(lash_core::ToolValue::Attachment(
                    lash_core::AttachmentSource::stored(attachment.clone()),
                )),
            )),
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
fn agent_scenario_failed_child_preserves_failure_graph() -> Result<()> {
    run_async_test_on_stack_budget("agent-scenario-failed-child", || async {
        let ran_execution = run_agent_turn_scenario_without_success_assertions(
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
        insta::assert_snapshot!(agent_scenario_transcript(&ran_execution, "root"), @r#"
        root         ingress   turn.start
        root         ingress   queued_input.accepted   inputs=1
        root         provider  model.request           iteration=0
        root         exec      cell.start              lang="typescript"
        root         tool      tool.start              name="spawn_agent" call=call-001
        root         tool      tool.intent             call=call-001 kind="start_process" status="executed"
        root         tool      tool.result             name="spawn_agent" outcome=failure call=call-001
        root         exec      cell.failed             calls=1 failure="program" error="`?` unwrapped failed module operation: child boom --> line 2, column 22 …"
        root         commit    checkpoint.commit       rev=0->1
        root                     turn_state            stored logical=131B
        root                     plugin_state          stored {"lash.triggers":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":0,"receipts":{}},"values":{}},"rlm_protocol":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":0,"receipts":{}},"values":{}},"subagents":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":0,"receipts":{}},"values":{}}}
        root         commit    checkpoint.commit       rev=1->2
        root                     turn_state            stored logical=131B
        root                     tool_state            stored logical=<opaque>
        root                     plugin_state          stored {"lash.triggers":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":1,"receipts":{}},"values":{}},"rlm_protocol":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":1,"receipts":{}},"values":{}},"subagents":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":1,"receipts":{}},"values":{}}}
        root                     execution_state       stored logical=unknown
        session-001  commit    checkpoint.commit       rev=0->1
        session-001              turn_state            stored logical=131B
        session-001  commit    checkpoint.commit       rev=1->2
        session-001              turn_state            stored logical=131B
        session-001              plugin_state          stored {"lash.triggers":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":0,"receipts":{}},"values":{}},"rlm_protocol":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":0,"receipts":{}},"values":{}},"subagents":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":0,"receipts":{}},"values":{}}}
        session-001  commit    checkpoint.commit       rev=2->3
        session-001              turn_state            stored logical=131B
        session-001              tool_state            stored logical=<opaque>
        session-001              plugin_state          stored {"lash.triggers":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":1,"receipts":{}},"values":{}},"rlm_protocol":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":1,"receipts":{}},"values":{}},"subagents":{"format_version":1,"generation":0,"publication":{"applied":null,"owner_segment":1,"receipts":{}},"values":{}}}
        session-001              execution_state       stored logical=unknown
        process-001  outcome   process.failed          label="spawn" kind="subagent" terminal=true
        "#);

        assert_failed_code_block_present(&ran_execution.streamed_events);
        assert_no_forbidden_error_text(&ran_execution.streamed_events);
        let spawn_failure = ran_execution
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
        assert_eq!(spawn_failure.code, "process_session_turn_tool_error");
        // FIG-2975: the child's own reason is what the parent reads. Before the
        // runner carried it, every stopped child collapsed onto one sentence.
        assert_eq!(spawn_failure.message, "child boom");
        assert_eq!(spawn_failure.source, lash_core::ToolFailureSource::Tool);
        assert_eq!(spawn_failure.retry, lash_core::ToolRetryStatus::Never);
        assert!(
            !format!("{:#?}", ran_execution.streamed_events)
                .contains("scripted agent scenario provider exhausted"),
            "failed-child scenario must fail through the child task.fail path, not provider exhaustion"
        );
        assert_no_false_finishted_success(&ran_execution);
        assert_all_processes_terminal(&ran_execution.final_process_list);
        let contract = GraphContract::from_graphs(&ran_execution.graph_snapshots);
        assert_labeled_resource_operation(
            &contract,
            "Spawn failing subagent",
            NodeStatusFact::Failed,
        );
        assert_no_duplicate_label_step(&contract, "Spawn failing subagent");
        assert_graph_lineage_connected(&contract, &ran_execution.final_process_list);
        assert_subagent_bridge_exec_graphs(
            &ran_execution,
            crate::tracing::TraceLanguageExecutionStatus::Completed,
        );

        Ok(())
    })
}
