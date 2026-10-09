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

/// A host tracer sees candidate refusal immediately with no deferred capacity,
/// and repeated reconciliations when exported-identity memory is disabled.
#[test]
fn facade_otel_admission_capacities_control_defer_and_dedup() {
    use crate::tracing::otel as api;
    use api::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
    use std::sync::Mutex;
    #[derive(Clone)]
    struct Tracer(Arc<Mutex<Vec<String>>>);
    struct Span {
        context: SpanContext,
        name: String,
        ended: Arc<Mutex<Vec<String>>>,
    }
    impl api::trace::Span for Span {
        fn add_event_with_timestamp<T: Into<std::borrow::Cow<'static, str>>>(
            &mut self,
            _: T,
            _: std::time::SystemTime,
            _: Vec<api::KeyValue>,
        ) {
        }
        fn span_context(&self) -> &SpanContext {
            &self.context
        }
        fn is_recording(&self) -> bool {
            true
        }
        fn set_attribute(&mut self, _: api::KeyValue) {}
        fn set_status(&mut self, _: api::trace::Status) {}
        fn update_name<T: Into<std::borrow::Cow<'static, str>>>(&mut self, name: T) {
            self.name = name.into().into_owned();
        }
        fn add_link(&mut self, _: SpanContext, _: Vec<api::KeyValue>) {}
        fn end_with_timestamp(&mut self, _: std::time::SystemTime) {
            self.ended.lock().unwrap().push(self.name.clone());
        }
    }
    impl api::trace::Tracer for Tracer {
        type Span = Span;
        fn build_with_context(&self, builder: api::trace::SpanBuilder, cx: &api::Context) -> Span {
            let parent = cx.span().span_context().clone();
            Span {
                context: if parent.is_valid() {
                    parent
                } else {
                    SpanContext::new(
                        TraceId::from_bytes([1; 16]),
                        SpanId::from_bytes([2; 8]),
                        TraceFlags::SAMPLED,
                        false,
                        TraceState::default(),
                    )
                },
                name: builder.name.into_owned(),
                ended: self.0.clone(),
            }
        }
    }
    impl api::trace::TracerProvider for Tracer {
        type Tracer = Self;
        fn tracer_with_scope(&self, _: api::InstrumentationScope) -> Self {
            self.clone()
        }
    }
    let ended = Arc::new(Mutex::new(Vec::new()));
    let adapter = OtelTelemetry::new(
        &Tracer(ended.clone()),
        &api::metrics::noop::NoopMeterProvider::new(),
        OtelOptions {
            admission_limits: OtelAdmissionLimits {
                exported: 0,
                deferred: 0,
            },
            ..OtelOptions::standard()
        },
    );
    let scope = TraceScopeId::admission(TraceScopeOwner::Turn {
        session_id: "capacity".into(),
        turn_id: "turn".into(),
    });
    let candidate = adapter.propose(&scope, &TraceCause::Root, 1);
    let anchor = candidate.anchor();
    candidate.defer();
    assert_eq!(
        ended.lock().unwrap().len(),
        1,
        "zero deferred capacity ends the candidate immediately"
    );
    let retained = DurableTraceScope {
        scope,
        cause: TraceCause::Root,
        anchor,
        started_at_ms: 1,
    };
    adapter.export_admitted(&retained);
    adapter.export_admitted(&retained);
    assert_eq!(
        ended
            .lock()
            .unwrap()
            .iter()
            .filter(|name| name.as_str() == "lash.turn.admitted")
            .count(),
        2
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
    let core = crate::tests::explicit_ephemeral_facets(LashCore::standard_builder(
        crate::tests::sqlite_memory_store_backend().await,
    ))
    .delivery_fetch_horizon(crate::attachments::DeliveryFetchHorizon { millis: 17 })
    .serve_test_llm_profile(
        crate::testing::TestProvider::builder()
            .build()
            .into_handle(),
        crate::tests::mock_llm_profile_spec(),
    )
    .build(crate::testing::runtime_lease_owner())
    .unwrap();
    let session = core
        .session("fetch-horizon".into())
        .created()
        .await
        .open()
        .await
        .unwrap();
    let turn = crate::TurnId::parse("fetch-turn").unwrap();
    session
        .send(crate::TurnInput::text("hello"))
        .id(turn.clone())
        .output()
        .await
        .unwrap();
    let row = core
        .backend()
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
    let mut stored: serde_json::Value = serde_json::from_str(&row.snapshot).unwrap();
    assert_eq!(stored["body"]["fetch_horizon"]["millis"], 17);
    stored["body"]
        .as_object_mut()
        .unwrap()
        .remove("fetch_horizon");
    assert!(
        serde_json::from_value::<lash_core::prompt_sections::AdmittedModelCall>(stored).is_err(),
        "cold admission never invents missing slack"
    );
    core.shutdown().await.unwrap();
}
