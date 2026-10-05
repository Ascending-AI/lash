//! Detailed tool facts attached to their accepted Run observation records.
use super::*;

impl ProductionToolHandlers<'_> {
    pub(super) fn started_observation(
        &self,
        request: &SingletonPreparedRequest,
    ) -> Result<Option<serde_json::Value>, String> {
        let prepared: Prepared =
            serde_json::from_value(request.prepared.clone()).map_err(|error| error.to_string())?;
        self.context
            .with_tool_observation_attribution(&prepared.input.attribution)
            .recorded_tool_observation(lash_trace::TraceEvent::ToolCallStarted {
                call_id: prepared.call.call_id,
                provider_call_id: prepared.call.provider_call_id,
                name: prepared.call.tool_name,
                args: prepared.call.args,
                issuing_node_id: None,
            })
    }

    pub(super) fn completed_observation(
        &self,
        call_id: &crate::ToolCallId,
        decision: &CallDecision,
        cause: Option<&AttributedVerdict<HookCause>>,
        capture: Option<&SingletonCapture>,
        presentation: Option<&str>,
    ) -> Result<Option<serde_json::Value>, String> {
        let record = self.observed_record(call_id, decision, cause, capture, presentation)?;
        let prepared = self
            .prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .ok_or("the observed call has no admitted preparation")?;
        self.context
            .with_tool_observation_attribution(&prepared.input.attribution)
            .recorded_tool_observation(lash_trace::TraceEvent::ToolCallCompleted {
                call_id: call_id.clone(),
                provider_call_id: record.provider_call_id,
                name: record.tool,
                args: record.args,
                output: crate::trace::trace_tool_call_output(&record.output),
                duration_ms: 0,
                issuing_node_id: None,
                attempts: None,
            })
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
