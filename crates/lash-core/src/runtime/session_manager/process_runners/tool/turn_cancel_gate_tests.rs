//! Runner-side deferred-tool awaits keep the process body's turn-cancel
//! observation decision instead of rebuilding an always-observing wait.

use super::*;
use lash_sansio::sync::MutexExt;

#[derive(Default)]
struct AwaitShapeRecorder {
    waits: std::sync::Mutex<Vec<(bool, Option<crate::ExecutionScope>)>>,
}

impl crate::AwaitEventResolver for AwaitShapeRecorder {}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for AwaitShapeRecorder {
    async fn execute_effect(
        &self,
        _envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let options = local_executor.into_await_event_options()?;
        self.waits
            .lock_recover()
            .push((options.observe_turn_cancel, options.turn_cancel_scope));
        Ok(crate::RuntimeEffectOutcome::AwaitEvent {
            resolution: crate::Resolution::Ok(serde_json::Value::Null),
        })
    }
}

#[tokio::test]
async fn runner_side_deferred_await_inside_a_process_body_attaches_no_turn_cancel_gate() {
    let recorder = AwaitShapeRecorder::default();
    let wait =
        crate::runtime::TurnCancelWait::unobserved(tokio_util::sync::CancellationToken::new());
    let key = crate::AwaitEventKey {
        scope: crate::ExecutionScope::process("process-1"),
        wait: crate::AwaitEventWaitIdentity::ToolCompletion {
            tool_call_id: "call-1".to_string(),
        },
        key_id: "key-1".to_string(),
        signature: "signature-1".to_string(),
    };
    let pending = crate::tool_dispatch::PendingToolDispatchOutcome {
        tool_name: "deferred".to_string(),
        args: serde_json::json!({}),
        key,
        pending: crate::PendingCompletion::default(),
        duration_ms: 0,
        attempts: Vec::new(),
    };
    let invocation = crate::RuntimeInvocation::effect(
        crate::RuntimeScope::new("session-1"),
        "process:process-1:tool:deferred:await",
        crate::RuntimeEffectKind::AwaitEvent,
        "process:process-1:tool:deferred:await",
    );

    let resolution = await_pending_process_tool(
        &recorder,
        Arc::new(crate::SystemClock),
        invocation,
        pending,
        &wait,
    )
    .await
    .expect("the deferred await resolves");

    assert!(matches!(resolution, crate::Resolution::Ok(_)));
    assert_eq!(
        *recorder.waits.lock_recover(),
        vec![(false, None)],
        "the runner-side process await must not attach the turn-cancel gate"
    );
}
