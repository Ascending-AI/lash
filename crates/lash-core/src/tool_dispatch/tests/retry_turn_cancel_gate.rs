//! The turn-cancel gate on a retry sleep follows the execution that owns the
//! tool attempt.
//!
//! Process bodies do not observe turn cancellation, while turn-driven tool
//! calls do. The retry coordinator must preserve that distinction when the
//! first attempt fails retryably and it journals the sleep before attempt two.

use super::*;

type RetrySleepShape = (bool, Option<crate::ExecutionScope>);

#[derive(Default)]
struct RetrySleepShapeRecorder {
    sleeps: std::sync::Mutex<Vec<RetrySleepShape>>,
}

impl crate::AwaitEventResolver for RetrySleepShapeRecorder {}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for RetrySleepShapeRecorder {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        if matches!(&envelope.command, crate::RuntimeEffectCommand::Sleep { .. }) {
            let options = local_executor.into_sleep_options();
            self.sleeps
                .lock_recover()
                .push((options.observe_turn_cancel, options.turn_cancel_scope));
            Ok(crate::RuntimeEffectOutcome::Sleep)
        } else {
            local_executor.execute(envelope).await
        }
    }
}

async fn retry_sleep_shape(
    identity: ToolAttemptEffectIdentity,
    turn_cancel_wait: crate::runtime::TurnCancelWait,
) -> RetrySleepShape {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed_attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::new(RetrySleepShapeRecorder::default());
    let mut context = exact_dispatch_context(Arc::new(RetryProbeTools {
        definition: retry_tool("retry_probe", ToolRetryPolicy::safe(2, 25, 25)),
        attempts: Arc::clone(&attempts),
        successes_after: 2,
        cancel_on_first: false,
        observed_attempts,
        retry_after_ms: Some(25),
    }));
    context.effect_controller = RuntimeEffectControllerHandle::shared(recorder.clone());
    let manifest =
        resolve_callable_manifest(&context, "retry_probe").expect("the retry probe is callable");
    let call = crate::PreparedToolCall::identity(
        manifest.id,
        crate::sansio::PendingToolCall {
            call_id: "retry-call".to_string(),
            tool_name: "retry_probe".to_string(),
            args: json!({ "value": "ok" }),
            replay: None,
        },
    );
    let dispatch = Arc::new(context.clone());
    let tool_context = ToolContext::from_dispatch(Arc::clone(&dispatch))
        .prepared_call(&call)
        .build();

    let coordinated = coordinate_tool_invocation(
        &context,
        call,
        None,
        ToolRetryPolicy::safe(2, 25, 25),
        identity,
        &turn_cancel_wait,
        None,
        None,
        |completion_key| {
            crate::RuntimeEffectLocalExecutor::prepared_tool_attempt(
                Arc::clone(&dispatch),
                tool_context.clone(),
                completion_key,
            )
        },
    )
    .await;

    assert!(
        matches!(coordinated.launch, ToolCallLaunch::Done(ref outcome) if outcome.record.output.is_success()),
        "the retry probe succeeds on attempt two"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let sleeps = recorder.sleeps.lock_recover().clone();
    assert_eq!(sleeps.len(), 1, "exactly one retry sleep is journaled");
    sleeps.into_iter().next().expect("the recorded retry sleep")
}

#[tokio::test]
async fn retry_sleep_inside_a_process_body_attaches_no_turn_cancel_gate() {
    let shape = retry_sleep_shape(
        ToolAttemptEffectIdentity::Process {
            parent: None,
            process_id: "process-1".to_string(),
        },
        crate::runtime::TurnCancelWait::unobserved(tokio_util::sync::CancellationToken::new()),
    )
    .await;

    assert_eq!(
        shape,
        (false, None),
        "a process-body retry sleep must not attach the turn-cancel gate"
    );
}

#[tokio::test]
async fn retry_sleep_under_a_turn_keeps_the_turn_cancel_gate() {
    let shape = retry_sleep_shape(
        ToolAttemptEffectIdentity::Scalar { parent: None },
        crate::runtime::TurnCancelWait::observing(
            tokio_util::sync::CancellationToken::new(),
            crate::ExecutionScope::runtime_operation("test-runtime-effect-controller"),
        ),
    )
    .await;

    assert!(
        shape.0,
        "a turn-driven retry sleep observes turn cancellation"
    );
    assert_eq!(
        shape.1,
        Some(crate::ExecutionScope::runtime_operation(
            "test-runtime-effect-controller"
        )),
        "the retry sleep registers the owning turn gate scope"
    );
}
