//! The process workflow's cancel steps and its generation gates (FIG-3673).
//!
//! The `cancel` handler records the request, the child-turn stop and the
//! segment it routes to as named steps, so a redrive forwards exactly where
//! the first execution forwarded. A segment keeps its handover until it has
//! recorded the cancel it forwards to its successor. Requests and inputs of
//! another generation are refused before anything is journaled, and a refused
//! segment still publishes its stored terminal to its awaiters.

use super::*;

const CALL_COMMAND: u16 = 0x040D;

fn boundary_handover(tag: u8) -> lash_core::SegmentHandover {
    lash_core::SegmentHandover {
        reason: lash_core::BoundaryReason::JournalBudget,
        program_hash: "p16-cancel-steps-program".to_string(),
        engine_state: vec![tag],
    }
}

/// The `cancel` handler records the request, the child-turn stop and its
/// route as steps, so a redrive forwards to the segment the first execution
/// chose even when the latest handover has moved on since (FIG-3673).
#[tokio::test]
pub(super) async fn a_redriven_cancel_forwards_to_its_recorded_route() {
    let (registry, continuations) = process_stores();
    let registration = executed_registration();
    let record = registry
        .register_process(registration)
        .await
        .expect("register the process");
    let process_id = record.id.clone();
    continuations
        .put_segment_handover(
            &process_id,
            lash_core::PersistedSegmentHandover {
                writer: String::new(),
                segment_ordinal: 2,
                written_generation: lash_core::engine::BuildGeneration::for_test("t0"),
                route: "LashProcessWorkflow".to_string(),
                handover: boundary_handover(2),
            },
        )
        .await
        .expect("segment 2 owns the process");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(Fig788SegmentBoundaryRunner),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .serve(),
        )
        .build();
    let request = RestateProcessCancelRequest::new(
        record.id.clone(),
        lash_core::CancelRequest::new(
            lash_core::CancelOrigin::OperatorRequested,
            "actor:fixture:cancel-route",
            11,
        ),
    );

    let first = invoke_endpoint_body_with_json_call_responses_then_suspend(
        &endpoint,
        "LashProcessWorkflow",
        "cancel",
        endpoint_protocol::encode_invocation_body(process_id.as_str(), &request)
            .expect("encode the cancel invocation"),
        Vec::new(),
    )
    .await
    .expect("the cancel records its steps and suspends on the forward");
    let runs = restate_recorded_commands(&first)
        .expect("decode the cancel journal")
        .into_iter()
        .filter(|command| command.message_type == RESTATE_RUN_COMMAND_MESSAGE_TYPE)
        .count();
    assert_eq!(runs, 3, "record, child-turn stop and route are steps");
    assert_eq!(
        restate_call_frames(&first)
            .expect("decode the forward")
            .iter()
            .map(|call| (call.key.as_str(), call.handler.as_str()))
            .collect::<Vec<_>>(),
        vec![(format!("{process_id}#2").as_str(), "deliver_cancel")]
    );
    assert!(
        registry
            .get_process(&process_id)
            .await
            .expect("read the process")
            .expect("the process stands")
            .cancel_request
            .is_some(),
        "the record step wrote the request"
    );

    // The process moved on before the redrive: the latest handover is 3.
    continuations
        .put_segment_handover(
            &process_id,
            lash_core::PersistedSegmentHandover {
                writer: String::new(),
                segment_ordinal: 3,
                written_generation: lash_core::engine::BuildGeneration::for_test("t0"),
                route: "LashProcessWorkflow".to_string(),
                handover: boundary_handover(3),
            },
        )
        .await
        .expect("segment 3 owns the process now");
    let replay =
        encode_recorded_commands_replay(process_id.as_str(), &request, &[&first], |command| {
            (command.message_type == CALL_COMMAND).then_some(serde_json::Value::Null)
        })
        .expect("splice the cancel journal");
    let redriven = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "LashProcessWorkflow",
        "cancel",
        replay,
        Vec::new(),
    )
    .await
    .expect("the redrive replays its recorded route");
    assert!(
        restate_call_frames(&redriven)
            .expect("decode the redrive")
            .is_empty(),
        "the redrive forwards nowhere new: the recorded route stands"
    );
    assert_eq!(
        restate_output_json::<()>(&redriven),
        Some(()),
        "endpoint error: {:?}",
        restate_error_message(&redriven)
    );
}

/// A cancel request built for another generation of the cancel handlers is
/// refused before anything is journaled, typed, and records nothing
/// (FIG-3673).
#[tokio::test]
pub(super) async fn a_retired_cancel_request_is_refused_before_any_command() {
    for handler in ["cancel", "deliver_cancel"] {
        let registry = process_registry();
        let record = registry
            .register_process(executed_registration())
            .await
            .expect("register the process");
        let process_id = record.id.clone();
        let endpoint = Endpoint::builder()
            .bind(
                LashProcessWorkflowImpl::new_for_test(
                    Arc::new(Fig788SegmentBoundaryRunner),
                    Arc::clone(&registry),
                    continuation_store(),
                )
                .serve(),
            )
            .build();
        for journal_version in [None, Some(2_u32)] {
            let mut request = serde_json::to_value(RestateProcessCancelRequest::new(
                record.id.clone(),
                lash_core::CancelRequest::new(
                    lash_core::CancelOrigin::OperatorRequested,
                    "actor:fixture:retired-cancel",
                    11,
                ),
            ))
            .expect("encode the request");
            match journal_version {
                Some(version) => request["journal_version"] = serde_json::json!(version),
                None => {
                    request
                        .as_object_mut()
                        .expect("the request is an object")
                        .remove("journal_version");
                }
            }
            let output = invoke_process_workflow_endpoint(
                &endpoint,
                handler,
                process_id.as_str(),
                &request,
                true,
            )
            .await
            .unwrap_or_default();
            assert_eq!(
                restate_recorded_commands(&output).map(|commands| {
                    commands
                        .iter()
                        .filter(|command| command.message_type != 0x0401)
                        .count()
                }),
                Some(0),
                "{handler} {journal_version:?}: nothing is journaled"
            );
            let refusal =
                restate_output_failure_message(&output).expect("the refusal is a terminal failure");
            assert!(
                refusal.contains(&format!(
                    "restate-process-journal-v{}",
                    journal_version.unwrap_or(1)
                )),
                "{handler}: the refusal names the generation: {refusal}"
            );
        }
        assert!(
            registry
                .get_process(&process_id)
                .await
                .expect("read the process")
                .expect("the process stands")
                .cancel_request
                .is_none(),
            "{handler}: a refused request records nothing"
        );
    }
}
