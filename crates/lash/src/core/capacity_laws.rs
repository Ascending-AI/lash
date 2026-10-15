//! Host presets resolve at the facade and govern work at the consuming boundary.
use super::runtime_host_config::tests::Sink;
use super::*;
use crate::runtime::*;
use crate::tracing::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn envelope(
    input: serde_json::Value,
) -> std::result::Result<RuntimeEffectEnvelope, crate::runtime::EffectIdentityError> {
    Ok(RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(ExecutionScope::turn("capacity", "turn"), "tool-attempt:cut")?,
            RuntimeAttribution::for_turn("capacity", "turn", 0, 0),
            "tool-attempt:cut",
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: Box::new(crate::tools::PreparedToolCall {
                call_id: crate::ToolCallId::fixture("cut"),
                provider_call_id: None,
                tool_id: "tool:cut".into(),
                tool_name: "cut".into(),
                args: input,
                replay: None,
                prepared_payload: serde_json::Value::Null,
            }),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    ))
}

#[tokio::test]
async fn facade_held_observation_capacity_limits_the_live_suffix() {
    let sink = Arc::new(Sink(AtomicUsize::new(0)));
    let mut builder = crate::tests::explicit_ephemeral_facets(LashCore::builder(
        crate::tests::sqlite_memory_store_backend().await,
    ))
    .trace_sink(sink.clone())
    .trace_limits(TraceLimits {
        held_observations: 1,
        ..TraceLimits::standard()
    });
    let config = builder
        .resolve_runtime_host_config(crate::durability::DataRetentionConfig::standard())
        .unwrap();
    let actor = ActorContext::unavailable()
        .scoped(AdmittedScope::turn("capacity", "turn"))
        .unwrap();
    let standing = config.tracing.shift(None, &actor);
    for _ in 0..3 {
        standing.observe_deferred(|| {
            (
                TraceContext::default(),
                TraceEvent::CompactionCompleted { summary_nodes: 0 },
            )
        });
    }
    assert_eq!(sink.0.load(Ordering::Relaxed), 0);
    let executor = actor
        .guard_local_executor(
            &envelope(serde_json::Value::Null).unwrap(),
            RuntimeEffectLocalExecutor::unavailable(),
        )
        .unwrap();
    let _live = executor.step_issue().begin_native();
    assert_eq!(sink.0.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn facade_evidence_cuts_bound_attempts_failures_and_divergences() {
    let mut builder = crate::tests::explicit_ephemeral_facets(LashCore::builder(
        crate::tests::sqlite_memory_store_backend().await,
    ))
    .trace_limits(TraceLimits {
        attempt_stream_bytes: 1,
        failure_partial_output_bytes: 1,
        diff_value_json_bytes: 1,
        diff_summary_paths: 1,
        diagnostic_error_chars: 2,
        ..TraceLimits::standard()
    });
    let config = builder
        .resolve_runtime_host_config(crate::durability::DataRetentionConfig::standard())
        .unwrap();
    let limits = config.tracing.limits();
    let recorder = AttemptStreamRecorder::start(limits.attempt_stream_bytes);
    recorder.observe(ShiftObservation {
        key: ReplayKey::new("capacity"),
        ordinal: 0,
        event: ObservedEvent::Session(crate::plugins::SessionStreamEvent::StreamBlock(
            crate::StreamBlockEvent::delta(
                crate::StreamBlockKind::AssistantText,
                crate::direct::StreamBlockIdentity::new("block", 0),
                "€",
            ),
        )),
    });
    let captured = recorder.finish();
    assert!(captured.events.is_empty());
    assert_eq!(captured.truncated.unwrap().dropped_events, 1);
    let output =
        crate::TurnFailurePartialOutput::bounded("€".into(), limits.failure_partial_output_bytes);
    assert!(output.is_truncated());
    assert!(output.text().len() <= 1);
    assert_eq!(
        limits.diagnostic_error("€abcd€"),
        "€\n\n... (4 chars omitted) ...\n\n€"
    );
    let recorded = envelope(serde_json::json!({"a": "one", "b": "two"}))
        .unwrap()
        .canonical_form()
        .unwrap();
    let reconstructed = envelope(serde_json::json!({"a": "changed", "b": "changed"}))
        .unwrap()
        .canonical_form()
        .unwrap();
    let error = crate::durability::validate_replayed_effect_envelope(
        &recorded,
        &reconstructed,
        RuntimeErrorCode::EffectReplayDivergence,
        None,
        limits,
    )
    .unwrap_err();
    assert_eq!(
        error.summary.as_ref().unwrap().first_divergent_paths.len(),
        1
    );
}

#[tokio::test]
async fn facade_observation_publisher_yields_at_the_host_batch() {
    use std::future::Future;
    let mut builder = crate::tests::explicit_ephemeral_facets(LashCore::builder(
        crate::tests::sqlite_memory_store_backend().await,
    ))
    .observation_work_limits(ObservationWorkLimits {
        publisher_batch: std::num::NonZeroUsize::MIN,
        ..Default::default()
    });
    let limits = builder
        .resolve_runtime_host_config(crate::durability::DataRetentionConfig::standard())
        .unwrap()
        .observation_work_limits;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for value in 0..3 {
        tx.send(value).unwrap();
    }
    let published = AtomicUsize::new(0);
    let mut shift = Box::pin(std::future::ready("done"));
    let mut work = Box::pin(work_with_observations(
        shift.as_mut(),
        &mut rx,
        |_| {
            published.fetch_add(1, Ordering::Relaxed);
            std::future::ready(())
        },
        limits,
    ));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(work.as_mut().poll(&mut cx).is_pending());
    assert_eq!(published.load(Ordering::Relaxed), 1);
    assert_eq!(work.await, "done");
    assert_eq!(published.load(Ordering::Relaxed), 3);
}

/// The facade resolves fetch slack before admission; cold reads require that choice.
#[tokio::test]
async fn facade_fetch_horizon_is_recorded_for_cold_resends() {
    use crate::tests::CreatedSession;
    let backend = crate::tests::sqlite_memory_store_backend().await;
    let turn = crate::TurnId::parse("fetch-turn").unwrap();
    // The call's request, read while the call is open: its settlement
    // releases it.
    let admitted = Arc::new(std::sync::Mutex::new(None));
    let provider = {
        let (backend, turn, admitted) = (backend.clone(), turn.clone(), Arc::clone(&admitted));
        crate::testing::TestProvider::builder()
            .complete(move |_| {
                let (backend, turn, admitted) =
                    (backend.clone(), turn.clone(), Arc::clone(&admitted));
                async move {
                    let row = backend
                        .durable()
                        .prompt_snapshot(&crate::prompt::PromptCallKey {
                            session: crate::SessionId::from("fetch-horizon"),
                            call: crate::prompt::ModelCallId::Turn {
                                run: turn,
                                ordinal: 1,
                            },
                        })
                        .await
                        .unwrap()
                        .unwrap();
                    *admitted.lock().unwrap() = row.request;
                    Ok(lash_core::llm::types::LlmResponse {
                        parts: vec![lash_core::llm::types::LlmOutputPart::Text {
                            text: "done".to_owned(),
                            response_meta: None,
                        }],
                        ..Default::default()
                    })
                }
            })
            .build()
            .into_handle()
    };
    let core = crate::tests::explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .delivery_fetch_horizon(crate::attachments::DeliveryFetchHorizon { millis: 17 })
        .serve_test_llm_profile(provider, crate::tests::mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .unwrap();
    let session = core
        .session("fetch-horizon".into())
        .created()
        .await
        .open()
        .await
        .unwrap();
    session
        .send(crate::TurnInput::text("hello"))
        .id(turn)
        .output()
        .await
        .unwrap();
    let request = admitted
        .lock()
        .unwrap()
        .take()
        .expect("the open call holds its request");
    let mut stored: serde_json::Value = serde_json::from_str(&request).unwrap();
    assert_eq!(stored["body"]["fetch_horizon"]["millis"], 17);
    stored["body"]
        .as_object_mut()
        .unwrap()
        .remove("fetch_horizon");
    assert!(
        serde_json::from_value::<lash_core::prompt_sections::AdmittedRequest>(stored).is_err(),
        "cold admission never invents missing slack"
    );
    core.shutdown().await.unwrap();
}
