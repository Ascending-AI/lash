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

async fn record_cancel(registry: &Arc<dyn ProcessRegistry>, process_id: &str, requester: &str) {
    registry
        .append_event(
            &ProcessId::from(process_id),
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &registry
                    .resolve_process_ref(&ProcessId::from(process_id))
                    .await
                    .expect("retained cancellation target"),
                &lash_core::CancelRequest::new(
                    lash_core::CancelOrigin::OperatorRequested,
                    requester,
                    11,
                ),
            ),
        )
        .await
        .expect("record the cancel");
}

/// A segment redriven in its handover gap — after it sent its successor, before
/// it recorded the cancel it forwards — still finds its own handover, even
/// once its successor has handed over in turn: a put never retires an older
/// handover. The redrive forwards the cancel that landed in the gap and only
/// then retires its handover (FIG-3673).
#[tokio::test]
pub(super) async fn a_cancel_in_the_handover_gap_is_forwarded_after_the_successor_hands_over() {
    let process_id = "p16-handover-gap-cancel";
    let (registry, continuations) = process_stores();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register the segmented process");
    let (execution_authority, started) = invocation_started(
        &ProcessId::from(process_id),
        "p16-handover-gap-execution",
        1,
    );
    registry
        .record_first_started_with_authority(
            &ProcessId::from(process_id),
            started,
            &execution_authority,
        )
        .await
        .expect("record the process's start");
    continuations
        .put_segment_handover(
            &ProcessId::from(process_id),
            lash_core::PersistedSegmentHandover {
                writer: String::new(),
                segment_ordinal: 1,
                handover: boundary_handover(1),
            },
        )
        .await
        .expect("hand over to segment 1");
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
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 1,
        journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
    };
    let key = process_segment_workflow_key(&ProcessId::from(process_id), 1);

    let admission = admission_journal(&endpoint, &key, &input)
        .await
        .expect("segment 1 admits");
    let in_the_gap = invoke_endpoint_body_with_json_call_responses_then_suspend(
        &endpoint,
        "LashProcessWorkflow",
        "run",
        admitted_invocation_body(&key, &input, &admission).expect("splice the admission"),
        Vec::new(),
    )
    .await
    .expect("segment 1 suspends after sending its successor");
    assert!(
        restate_message_types(&in_the_gap)
            .expect("decode the gap attempt")
            .contains(&0x040E),
        "the attempt sent its successor"
    );

    // The cancel lands in the gap, and the successor hands over to segment 3
    // before segment 1 is redriven.
    record_cancel(&registry, process_id, "actor:fixture:handover-gap").await;
    continuations
        .put_segment_handover(
            &ProcessId::from(process_id),
            lash_core::PersistedSegmentHandover {
                writer: String::new(),
                segment_ordinal: 3,
                handover: boundary_handover(3),
            },
        )
        .await
        .expect("segment 2 hands over to segment 3");
    assert!(
        continuations
            .get_segment_handover(&ProcessId::from(process_id), 1)
            .await
            .expect("read handover 1")
            .is_some(),
        "a later put keeps segment 1's handover while segment 1 has not retired it"
    );

    let replay = encode_process_segment_send_replay(&key, &input, &in_the_gap)
        .and_then(|replay| with_admission(&replay, &admission))
        .expect("splice the gap journal");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "LashProcessWorkflow",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("the redrive forwards and retires");
    assert_eq!(
        restate_call_frames(&output)
            .expect("decode the forward")
            .iter()
            .map(|call| (call.key.as_str(), call.handler.as_str()))
            .collect::<Vec<_>>(),
        vec![("p16-handover-gap-cancel#2", "deliver_cancel")],
        "the cancel that landed in the gap reaches the successor"
    );
    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&output),
        Some(RestateProcessWorkflowOutput::SegmentChained {
            next_segment_ordinal: 2,
        }),
        "endpoint error: {:?}",
        restate_error_message(&output)
    );
    assert!(
        continuations
            .get_segment_handover(&ProcessId::from(process_id), 1)
            .await
            .expect("read handover 1")
            .is_none(),
        "segment 1 retires its handover once its forward is recorded"
    );
    for ordinal in [2, 3] {
        assert!(
            continuations
                .get_segment_handover(&ProcessId::from(process_id), ordinal)
                .await
                .expect("read a later handover")
                .is_some(),
            "segment 1 retires nothing after its own: handover {ordinal} stays"
        );
    }
}

/// The `cancel` handler records the request, the child-turn stop and its
/// route as steps, so a redrive forwards to the segment the first execution
/// chose even when the latest handover has moved on since (FIG-3673).
#[tokio::test]
pub(super) async fn a_redriven_cancel_forwards_to_its_recorded_route() {
    let process_id = "p16-cancel-route";
    let (registry, continuations) = process_stores();
    let registration = rerunnable_registration(process_id);
    let record = registry
        .register_process(registration)
        .await
        .expect("register the process");
    continuations
        .put_segment_handover(
            &ProcessId::from(process_id),
            lash_core::PersistedSegmentHandover {
                writer: String::new(),
                segment_ordinal: 2,
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
        lash_core::ProcessRef::from_record(&record),
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
        endpoint_protocol::encode_invocation_body(process_id, &request)
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
        vec![("p16-cancel-route#2", "deliver_cancel")]
    );
    assert!(
        registry
            .get_process(&ProcessId::from(process_id))
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
            &ProcessId::from(process_id),
            lash_core::PersistedSegmentHandover {
                writer: String::new(),
                segment_ordinal: 3,
                handover: boundary_handover(3),
            },
        )
        .await
        .expect("segment 3 owns the process now");
    let replay = encode_recorded_commands_replay(process_id, &request, &[&first], |command| {
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
        let process_id = format!("p16-retired-{handler}");
        let registry = process_registry();
        let record = registry
            .register_process(rerunnable_registration(&process_id))
            .await
            .expect("register the process");
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
                lash_core::ProcessRef::from_record(&record),
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
            let output =
                invoke_process_workflow_endpoint(&endpoint, handler, &process_id, &request, true)
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
                .get_process(&ProcessId::from(process_id.as_str()))
                .await
                .expect("read the process")
                .expect("the process stands")
                .cancel_request
                .is_none(),
            "{handler}: a refused request records nothing"
        );
    }
}

/// Answers every ingress call with `200 null` and keeps the requests.
#[derive(Debug, Default)]
struct RecordingIngress {
    requests: Mutex<Vec<HttpRequest>>,
}

#[async_trait::async_trait]
impl HttpTransport for RecordingIngress {
    async fn send(
        &self,
        request: HttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        self.requests.lock_recover().push(request);
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: HttpResponseBody::buffered("null"),
        })
    }
}

/// A refused retired-generation segment publishes the terminal it stored to
/// the root's awaiters through the root's `complete_terminal`, a separate
/// invocation, so awaiters are released even when the refused invocation's
/// own journal can never replay (FIG-3673).
#[tokio::test]
pub(super) async fn a_retired_generation_refusal_publishes_its_stored_terminal() {
    let process_id = "p16-retired-publish";
    let registry = process_registry();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register the process");
    let ingress = Arc::new(RecordingIngress::default());
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new(
                Arc::new(Fig788SegmentBoundaryRunner),
                Arc::clone(&registry),
                continuation_store(),
                RestateIngressClient::new(RestateConnection::with_transport(
                    "https://restate.invalid",
                    ingress.clone(),
                )),
                test_restate_authority_id(),
            )
            .serve(),
        )
        .build();
    let mut input = serde_json::to_value(RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 0,
        journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
    })
    .expect("encode the input");
    input["journal_version"] = serde_json::json!(2);
    let output = invoke_process_workflow_endpoint(&endpoint, "run", process_id, &input, true)
        .await
        .unwrap_or_default();
    assert!(restate_output_failure_message(&output).is_some());
    let stored = registry
        .get_process(&ProcessId::from(process_id))
        .await
        .expect("read the refused process")
        .and_then(|record| record.outcome)
        .expect("the refusal is stored");
    let requests = ingress.requests.lock_recover();
    assert_eq!(requests.len(), 1, "one publish");
    assert_eq!(
        requests[0].url,
        "https://restate.invalid/LashProcessWorkflow/p16-retired-publish/complete_terminal"
    );
    let published: RestateProcessCompleteRequest =
        serde_json::from_slice(requests[0].body.as_ref()).expect("decode the publish");
    assert_eq!(
        published.output, stored,
        "the stored refusal is what awaiters get"
    );
}
