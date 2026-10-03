//! Tool retry policy laws: which failures retry, how a retry sleep crosses
//! the effect controller, and how the attempts keep one replay key.

use super::*;

const SEED: u64 = 0x5_2d2c;

#[tokio::test]
async fn an_attempt_refuses_a_context_from_another_logical_call_before_the_body() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let context = retry_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        ToolRetryPolicy::Never,
        Arc::clone(&attempts),
        1,
        false,
        Arc::clone(&observed),
    )
    .await;
    let prepared = crate::PreparedToolCall {
        call_id: crate::ToolCallId::fixture("admitted-call"),
        provider_call_id: Some("provider-call".into()),
        tool_id: crate::ToolId::from("tool:retry_probe"),
        tool_name: "retry_probe".into(),
        args: json!({ "value": "ok" }),
        replay: None,
        prepared_payload: serde_json::Value::Null,
    };
    let stale_context = tool_context_for_prepared(&context, &prepared)
        .call_id(crate::ToolCallId::fixture("another-call"));
    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        None,
        stale_context,
    )
    .await;
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert!(observed.lock_recover().is_empty());
    let ToolCallLaunch::ControllerAborted(error) = launch else {
        panic!("a mismatched attempt context must refuse before producing a result");
    };
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch
    );
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn direct_and_prepared_runners_keep_the_call_and_attempt_ordinal() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let context = Arc::new(
        retry_dispatch_context(
            crate::support::double_dispatch_ports(&double, &handler),
            ToolRetryPolicy::safe(3, 0, 0),
            Arc::clone(&attempts),
            1,
            false,
            Arc::clone(&observed),
        )
        .await,
    );
    let prepared = crate::PreparedToolCall {
        call_id: crate::ToolCallId::fixture("runner-call"),
        provider_call_id: Some("provider-correlation".into()),
        tool_id: crate::ToolId::from("tool:retry_probe"),
        tool_name: "retry_probe".into(),
        args: json!({ "value": "ok" }),
        replay: None,
        prepared_payload: json!({ "frozen": true }),
    };
    let execution = crate::RuntimeExecutionContext::new(
        Arc::clone(&context),
        double.lash_backend().process_env_store(),
        Arc::clone(&context.attachment_store),
        Arc::new(crate::ChronologicalProjection::default()),
        crate::TurnContext::default(),
        context.execution_env_spec.clone(),
    );
    for (index, label) in ["direct", "prepared"].into_iter().enumerate() {
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(context.effect_controller.execution_scope().clone(), label)
                .expect("valid attempt address"),
            crate::RuntimeAttribution::for_session("session"),
            label,
        );
        let envelope = crate::RuntimeEffectEnvelope::new(
            invocation.clone(),
            crate::RuntimeEffectCommand::ToolAttempt {
                call: Box::new(prepared.clone()),
                execution_grant: None,
                attempt: 2,
                max_attempts: 3,
            },
        );
        let executor = if label == "direct" {
            let execution = execution.clone();
            let prepared = prepared.clone();
            crate::RuntimeEffectLocalExecutor::testing(move |_| async move {
                let outcome = execution
                    .execute_prepared_tool_attempt_effect(
                        prepared,
                        None,
                        2,
                        3,
                        invocation.into_runtime_invocation(),
                        None,
                        None,
                        None,
                    )
                    .await?;
                Ok(crate::RuntimeEffectOutcome::ToolAttempt {
                    launch: Box::new(outcome.launch),
                    triggers: outcome.triggers,
                    capture: (!outcome.capture.is_empty()).then(|| Box::new(outcome.capture)),
                })
            })
        } else {
            crate::prepared_tool_attempt(
                Arc::clone(&context),
                tool_context_for_prepared(context.as_ref(), &prepared),
                None,
            )
        };
        let outcome = context
            .effect_controller
            .execute_effect(envelope, executor)
            .await
            .expect("record the attempt");
        let crate::RuntimeEffectOutcome::ToolAttempt {
            launch,
            triggers,
            capture,
        } = outcome
        else {
            panic!("an attempt result");
        };
        let crate::ToolAttemptLaunch::Done { record, intents } = *launch else {
            panic!("the probe completes inline");
        };
        assert_eq!(record.call_id, prepared.call_id);
        assert_eq!(record.provider_call_id, prepared.provider_call_id);
        assert_eq!(record.args, prepared.args);
        assert_eq!(
            record.output.outcome,
            crate::ToolCallOutcome::Success(crate::ToolValue::untrusted_json(
                json!({ "attempt": index + 1 })
            ))
        );
        assert!(intents.is_empty());
        assert!(capture.is_none());
        assert!(triggers.is_empty());
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        *observed.lock_recover(),
        vec![(2, 3, prepared.call_id.to_string()); 2]
    );
    drop(execution);
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn default_retry_policy_never_retries_safe_failures() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context(
            crate::support::double_dispatch_ports(&double, &handler),
            ToolRetryPolicy::Never,
            Arc::clone(&attempts),
            usize::MAX,
            false,
            Arc::clone(&observed),
        )
        .await,
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(observed.lock_recover()[0].0, 1);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn safe_retry_policy_retries_safe_failure_and_stops_on_success() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context(
            crate::support::double_dispatch_ports(&double, &handler),
            ToolRetryPolicy::safe(3, 0, 0),
            Arc::clone(&attempts),
            2,
            false,
            Arc::clone(&observed),
        )
        .await,
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(outcome.record.output.is_success());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].ordinal, 1);
    assert!(matches!(
        outcome.attempts[0].detail,
        lash_trace::TraceRetryAttemptDetail::Tool {
            outcome: lash_trace::TraceToolAttemptOutcome::Failed { .. }
        }
    ));
    assert!(
        matches!(&outcome.attempts[0].detail, lash_trace::TraceRetryAttemptDetail::Tool { outcome: lash_trace::TraceToolAttemptOutcome::Failed { message, .. } } if message.contains("transient"))
    );
    assert_eq!(outcome.attempts[0].delay_ms, Some(0));
    assert_eq!(outcome.attempts[1].ordinal, 2);
    assert!(matches!(
        outcome.attempts[1].detail,
        lash_trace::TraceRetryAttemptDetail::Tool {
            outcome: lash_trace::TraceToolAttemptOutcome::Completed
        }
    ));
    assert_eq!(outcome.attempts[1].delay_ms, None);
    let directory = tempfile::tempdir().expect("trace tempdir");
    let path = directory.path().join("tool-retry.trace.jsonl");
    let runtime = crate::trace::TraceRuntime::default()
        .with_trace_sink(Arc::new(lash_trace::JsonlTraceSink::new(&path)));
    let tracing = crate::RuntimeExecutionTracing::new(
        runtime.clone(),
        None,
        lash_trace::TraceContext::default().for_session("tool-retry-session"),
    );
    crate::emit_tool_call_completed(
        &tracing,
        &runtime.unreplayed(None),
        &outcome.record,
        &outcome.attempts,
        None,
        7,
    );
    let emitted: lash_trace::TraceRecord =
        lash_trace::parse_jsonl_records(&std::fs::read_to_string(path).expect("read tool trace"))
            .expect("parse emitted tool trace")
            .into_iter()
            .next()
            .expect("one emitted tool trace record");
    let lash_trace::TraceEvent::ToolCallCompleted { attempts, .. } = emitted.event else {
        panic!("expected emitted tool completion");
    };
    assert_eq!(attempts.expect("emitted attempt ladder").len(), 2);
    assert_eq!(
        observed
            .lock_recover()
            .iter()
            .map(|(attempt, max, _)| (*attempt, *max))
            .collect::<Vec<_>>(),
        vec![(1, 3), (2, 3)]
    );
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn scalar_after_tool_hook_runs_once_per_retry_attempt_before_exhaustion() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed_attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_retries = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context_with_after_observations(
            crate::support::double_dispatch_ports(&double, &handler),
            Arc::clone(&attempts),
            observed_attempts,
            Arc::clone(&observed_retries),
        )
        .await,
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        *observed_retries.lock_recover(),
        vec![
            ToolRetryStatus::Safe { after_ms: Some(0) },
            ToolRetryStatus::Safe { after_ms: Some(0) },
        ],
        "the after hook runs for each finalized attempt, before exhaustion is marked"
    );
    let ToolCallOutcome::Failure(failure) = outcome.record.output.outcome else {
        panic!("expected exhausted failure");
    };
    assert_eq!(failure.retry, ToolRetryStatus::Exhausted { attempts: 2 });
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn retry_delay_crosses_effect_controller_as_sleep_effect() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::new(SleepRecordingEffectController::default());
    let mut context = exact_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        Arc::new(RetryProbeTools {
            definition: retry_tool("retry_probe", ToolRetryPolicy::safe(3, 25, 25)),
            attempts: Arc::clone(&attempts),
            successes_after: 2,
            cancel_on_first: false,
            observed_attempts: Arc::clone(&observed),
            retry_after_ms: Some(25),
        }),
    )
    .await;
    context.effect_controller = ScopedEffectController::shared(
        recorder.clone(),
        crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
    )
    .expect("valid test runtime scope");
    let tool_context = crate::testing::ToolCallFixture::from_dispatch(Arc::new(context.clone()))
        .call_id(crate::ToolCallId::fixture("call-1"));

    let outcome = dispatch_tool_call_with_execution_context(
        &context,
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
        tool_context,
    )
    .await;

    assert!(outcome.record.output.is_success());
    {
        let sleeps = recorder.sleeps.lock_recover();
        assert_eq!(sleeps.len(), 1);
        let sleep_key = format!(
            "tool:{}:attempt:1:sleep",
            crate::ToolCallId::fixture("call-1")
        );
        assert_eq!(sleeps[0].effect_id(), Some(sleep_key.as_str()));
        assert_eq!(sleeps[0].effect_replay_key(), Some(sleep_key.as_str()));
    }
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn retry_sleep_controller_rejection_aborts_as_controller_error() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut context = exact_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        Arc::new(RetryProbeTools {
            definition: retry_tool("retry_probe", ToolRetryPolicy::safe(3, 25, 25)),
            attempts: Arc::clone(&attempts),
            successes_after: 2,
            cancel_on_first: false,
            observed_attempts: Arc::clone(&observed),
            retry_after_ms: Some(25),
        }),
    )
    .await;
    context.effect_controller = ScopedEffectController::shared(
        Arc::new(FailingSleepEffectController),
        crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
    )
    .expect("valid test runtime scope");
    let tool_context = crate::testing::ToolCallFixture::from_dispatch(Arc::new(context.clone()))
        .call_id(crate::ToolCallId::fixture("call-1"));

    let outcome = dispatch_tool_call_with_execution_context(
        &context,
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
        tool_context,
    )
    .await;

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let ToolCallOutcome::Failure(failure) = outcome.record.output.outcome else {
        panic!("expected failure");
    };
    // A live controller rejection of the retry-sleep effect is a crash-class
    // fault, not a tool-produced outcome: it carries the controller's code
    // rather than `tool_retry_sleep_failed` (FIG-3528).
    assert_eq!(failure.code, "test_sleep_rejected");
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn cancellation_stops_retry_immediately() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context(
            crate::support::double_dispatch_ports(&double, &handler),
            ToolRetryPolicy::safe(3, 0, 0),
            Arc::clone(&attempts),
            usize::MAX,
            true,
            Arc::clone(&observed),
        )
        .await,
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(matches!(
        outcome.record.output.outcome,
        ToolCallOutcome::Cancelled(_)
    ));
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn retry_context_has_stable_replay_key_across_attempts() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let context = retry_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        ToolRetryPolicy::safe(3, 0, 0),
        Arc::clone(&attempts),
        3,
        false,
        Arc::clone(&observed),
    )
    .await;
    let tool_context = crate::testing::ToolCallFixture::from_dispatch(Arc::new(context.clone()))
        .call_id(crate::ToolCallId::fixture("call-1"));
    let outcome = dispatch_tool_call_with_execution_context(
        &context,
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
        tool_context,
    )
    .await;

    assert!(outcome.record.output.is_success());
    {
        let observed = observed.lock_recover();
        assert_eq!(observed.len(), 3);
        assert_eq!(
            observed
                .iter()
                .map(|(attempt, max, _)| (*attempt, *max))
                .collect::<Vec<_>>(),
            vec![(1, 3), (2, 3), (3, 3)]
        );
        let keys = observed
            .iter()
            .map(|(_, _, key)| key.clone())
            .collect::<Vec<_>>();
        assert!(keys.iter().all(|key| key == &keys[0]));
        assert_eq!(keys[0], crate::ToolCallId::fixture("call-1").to_string());
    }
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}
