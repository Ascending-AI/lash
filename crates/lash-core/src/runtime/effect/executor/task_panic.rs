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
            }),
            triggers: Vec::new(),
        }),
        None => Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::EffectPanicked,
            message,
        )),
    };
    drop(payload);
    result
}

pub(super) fn map_process_task_join(
    join: Result<
        Result<super::ProcessEffectOutcome, RuntimeEffectControllerError>,
        tokio::task::JoinError,
    >,
) -> Result<super::ProcessEffectOutcome, RuntimeEffectControllerError> {
    match join {
        Ok(result) => result,
        Err(err) if err.is_panic() => {
            let payload = err.into_panic();
            let message = crate::panic_containment::payload_message(payload.as_ref());
            let result = Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::ProcessPanicked,
                message,
            ));
            crate::panic_containment::enforce_loudness(payload);
            result
        }
        Err(err) => Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectProcessTaskJoin,
            format!("inline process effect task failed: {err}"),
        )),
    }
}
