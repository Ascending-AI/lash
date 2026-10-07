//! The facts an admitted call's observations carry.
use super::*;

impl ProductionToolHandlers<'_> {
    /// Observe an admitted call's start live, under its own trace scope,
    /// from the request its admission prepared.
    pub(super) fn started_observation(
        &self,
        request: &SingletonPreparedRequest,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let prepared: Prepared =
            serde_json::from_value(request.prepared.clone()).map_err(|error| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RecordEncodingFailed,
                    error.to_string(),
                )
            })?;
        let start = crate::session::ToolCallStart {
            call_id: &prepared.call.call_id,
            provider_call_id: prepared.call.provider_call_id.as_deref(),
            tool: &prepared.call.tool_name,
            args: &prepared.call.args,
        };
        self.context
            .with_tool_observation_attribution(&prepared.input.attribution)
            .trace_tool_call_started(start, self.context.dispatch().clock.timestamp_ms())
    }

    pub(super) fn observed_record(
        &self,
        call_id: &crate::ToolCallId,
        decision: &CallDecision,
        cause: Option<&AttributedVerdict<HookCause>>,
        capture: Option<&SingletonCapture>,
        presentation: Option<&str>,
    ) -> Result<ToolCallRecord, String> {
        let prepared = self
            .prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .ok_or("a terminal observation has no admitted preparation")?;
        let mut output = if matches!(capture, Some(SingletonCapture::Isolated { .. }))
            && matches!(decision, CallDecision::Final { .. })
        {
            ToolCallOutput::success(
                serde_json::from_str::<serde_json::Value>(
                    presentation.ok_or("isolated final has no descriptor")?,
                )
                .map_err(|error| error.to_string())?,
            )
        } else {
            terminal_output(decision, cause, capture)?
        };
        if let Some(presented) = presentation.and_then(|text| decode::<Presented>(text).ok()) {
            super::super::attempt_coordinator::project_recorded_intent_outcomes(
                &mut output,
                &presented.intent_outcomes,
            );
        }
        Ok(ToolCallRecord {
            call_id: call_id.clone(),
            provider_call_id: prepared.call.provider_call_id,
            tool: prepared.call.tool_name,
            args: prepared.call.args,
            output,
        })
    }
}

pub(super) fn terminal_output(
    decision: &CallDecision,
    cause: Option<&AttributedVerdict<HookCause>>,
    capture: Option<&SingletonCapture>,
) -> Result<ToolCallOutput, String> {
    if matches!(decision, CallDecision::Final { .. }) {
        match capture {
            Some(SingletonCapture::Interrupted) => return Ok(ToolCallOutput::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Execution, "tool_interrupted",
                "tool was interrupted by a runtime restart; it may or may not have taken effect, and may still be running.",
            ).with_cause(crate::ToolFailureCause::Interrupted))),
            Some(SingletonCapture::TimedOut { cause, evidence: None }) => return Ok(ToolCallOutput::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Timeout, "tool_timed_out", format!("tool exceeded its {cause:?} limit; it may have partly run, and may still be running."),
            ).with_cause(crate::ToolFailureCause::ExecutionLimit { cause: *cause }))),
            Some(SingletonCapture::Cancelled { evidence: None }) => return Ok(ToolCallOutput::cancelled(crate::ToolCancellation::runtime("tool cancelled"))),
            _ => {}
        }
        return capture
            .and_then(SingletonCapture::output)
            .ok_or_else(|| "the final has no canonical output".to_owned())
            .and_then(decode::<Captured>)
            .map(|captured| captured.output);
    }
    match cause.map(|cause| {
        (
            cause.verdict.error_type.as_str(),
            &cause.verdict.payload,
            &cause.callback,
        )
    }) {
        Some(("tool_failure", payload, _)) => serde_json::from_value(payload.clone())
            .map(ToolCallOutput::failure)
            .map_err(|error| error.to_string()),
        Some(("tool_cancellation", payload, _)) => serde_json::from_value(payload.clone())
            .map(ToolCallOutput::cancelled)
            .map_err(|error| error.to_string()),
        Some(("plugin_abort", payload, callback)) => {
            let abort: crate::plugin::PluginAbort =
                serde_json::from_value(payload.clone()).map_err(|error| error.to_string())?;
            let mut failure = crate::ToolFailure::runtime(
                crate::ToolFailureClass::Execution,
                abort.code.clone(),
                abort.message.clone(),
            );
            failure.source = crate::ToolFailureSource::Plugin;
            Ok(
                ToolCallOutput::failure(failure).with_control(crate::ToolControl::AbortRun {
                    code: abort.failure_code(&callback.owner.plugin),
                    message: abort.message,
                }),
            )
        }
        _ if matches!(decision, CallDecision::Cancelled) => Ok(ToolCallOutput::cancelled(
            crate::ToolCancellation::runtime("the owning Run cancelled the call"),
        )),
        _ => Ok(ToolCallOutput::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::InvalidRequest,
            "tool_call_denied",
            "a recorded tool check denied the call",
        ))),
    }
}
