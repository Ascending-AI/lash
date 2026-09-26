//! Tool retry policy laws: which failures retry, how a retry sleep crosses
//! the effect controller, and how the attempts keep one replay key.

use super::*;

const SEED: u64 = 0x5_2d2c;

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
    assert_eq!(
        outcome.attempts[0].outcome,
        lash_trace::TraceRetryAttemptOutcome::Failed
    );
    assert!(
        outcome.attempts[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("transient"))
    );
    assert_eq!(outcome.attempts[0].delay_ms, Some(0));
    assert_eq!(outcome.attempts[1].ordinal, 2);
    assert_eq!(
        outcome.attempts[1].outcome,
        lash_trace::TraceRetryAttemptOutcome::Completed
    );
    assert_eq!(outcome.attempts[1].delay_ms, None);
    let directory = tempfile::tempdir().expect("trace tempdir");
    let path = directory.path().join("tool-retry.trace.jsonl");
    let sink: Arc<dyn lash_trace::TraceSink> = Arc::new(lash_trace::JsonlTraceSink::new(&path));
    let tracing = crate::RuntimeExecutionTracing::new(
        sink,
        lash_trace::TraceContext::default(),
        lash_trace::TraceContext::default().for_session("tool-retry-session"),
    );
    crate::emit_tool_call_completed(
        &tracing,
        &outcome.record,
        &outcome.attempts,
        None,
        7,
        &crate::facade_support::SystemClock,
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
    context.effect_controller = RuntimeEffectControllerHandle::shared(recorder.clone());
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .tool_call_id("call-1".to_string())
        .build();

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
        assert!(
            sleeps[0]
                .effect_id()
                .is_some_and(|effect_id| effect_id.ends_with(":retry_probe:attempt:1:sleep"))
        );
        assert_eq!(
            sleeps[0].replay_key(),
            Some("lash-tool:session:call-1:retry_probe:attempt:1:sleep")
        );
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
    context.effect_controller =
        RuntimeEffectControllerHandle::shared(Arc::new(FailingSleepEffectController));
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .tool_call_id("call-1".to_string())
        .build();

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
async fn safe_retry_policy_marks_exhausted_after_final_attempt() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context(
            crate::support::double_dispatch_ports(&double, &handler),
            ToolRetryPolicy::safe(2, 0, 0),
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
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let ToolCallOutcome::Failure(failure) = outcome.record.output.outcome else {
        panic!("expected failure");
    };
    assert_eq!(failure.retry, ToolRetryStatus::Exhausted { attempts: 2 });
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
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .tool_call_id("call-1".to_string())
        .build();
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
        assert_eq!(
            keys[0].as_deref(),
            Some("lash-tool:session:call-1:retry_probe")
        );
    }
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn idempotent_retry_policy_uses_journaled_attempts_without_provider_replay_key() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context(
            crate::support::double_dispatch_ports(&double, &handler),
            ToolRetryPolicy::idempotent(3, 0, 0),
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
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    {
        let observed = observed.lock_recover();
        assert!(
            observed
                .iter()
                .all(|(_, max_attempts, replay_key)| *max_attempts == 3 && replay_key.is_none())
        );
    }
    handler.close().await.expect("close the dispatch handler");
}
