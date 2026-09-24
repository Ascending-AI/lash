use super::{RuntimeEffectControllerError, RuntimeEffectOutcome};

pub(super) fn map_effect_task_join(
    err: tokio::task::JoinError,
    panic_call: Option<crate::PreparedToolCall>,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    if !err.is_panic() {
        return Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectTaskJoin,
            format!("spawned local effect task failed: {err}"),
        ));
    }

    let payload = err.into_panic();
    let message = crate::panic_containment::payload_message(payload.as_ref());
    let result = match panic_call {
        Some(call) => Ok(RuntimeEffectOutcome::ToolAttempt {
            launch: Box::new(crate::ToolAttemptLaunch::Done {
                record: Box::new(crate::ToolCallRecord {
                    call_id: Some(call.call_id),
                    tool: call.tool_name,
                    args: call.args,
                    output: crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                        crate::ToolFailureClass::Internal,
                        "tool_panicked",
                        message,
                    )),
                    duration_ms: 0,
                }),
                intents: crate::ToolIntents::default(),
            }),
            triggers: Vec::new(),
            capture: None,
        }),
        None => Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::EffectPanicked,
            message,
        )),
    };
    drop(payload);
    result
}
