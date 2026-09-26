use serde_json::json;

use super::execution_context::RuntimeExecutionContext;
use super::tool_execution::ToolInvocationReply;
#[cfg(feature = "testing")]
use crate::tool_dispatch::ToolPreparationOutcome;
#[cfg(feature = "testing")]
use crate::{ProcessInput, ProcessRegistration};
use crate::{ToolCallOutput, ToolCallRecord, ToolOutcome};

enum HandleAuthority {
    RunLocalPossession,
    SessionVisible,
}

impl RuntimeExecutionContext<'_> {
    /// `process_id` rides beside the opaque id for the same reason
    /// [`ProcessHandleView`](crate::ProcessHandleView) carries it: the process
    /// tools take a `process_id`, and reading one out of the handle id is
    /// exactly what the opaque id forbids.
    pub fn process_handle_json(process_id: &crate::ProcessId) -> serde_json::Value {
        let mut record = lash_sansio::handle::handle_record_json(
            &lash_sansio::handle::HandleId::process(process_id),
        );
        record["process_id"] = json!(process_id.as_str());
        record
    }

    pub(super) fn process_status_value(status: &crate::ProcessRecord) -> serde_json::Value {
        json!({
            "process_id": status.id,
            "status": status.status.label(),
        })
    }

    /// Reads a process handle through the one parse `lash-sansio` owns
    /// (ADR 0095), so core and the language agree on what a handle is.
    pub(super) fn parse_process_handle(
        handle: &serde_json::Value,
    ) -> Result<crate::ProcessId, String> {
        crate::process_id_from_handle_json(handle)
    }

    /// FIG-653: observer validation enforces subscription relationships, not authorization.
    async fn authorize_handle(
        &self,
        process_id: &crate::ProcessId,
    ) -> Result<HandleAuthority, crate::PluginError> {
        if self.is_run_local_process(process_id) {
            return Ok(HandleAuthority::RunLocalPossession);
        }
        self.dispatch
            .processes
            .validate_visible(
                &self.session_id,
                std::slice::from_ref(process_id),
                self.process_scope(self.parent_invocation.clone()),
            )
            .await?;
        Ok(HandleAuthority::SessionVisible)
    }

    #[cfg(feature = "testing")]
    pub(crate) async fn start_tool_process(
        &self,
        call_id: String,
        tool_name: String,
        args: serde_json::Value,
    ) -> ToolInvocationReply {
        let handle_id = call_id.clone();
        let pending_call = crate::sansio::PendingToolCall {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            args: args.clone(),
            replay: None,
        };
        let prepared_call = match self
            .prepare_tool_call(pending_call, &format!("handle:{call_id}"))
            .await
        {
            ToolPreparationOutcome::Prepared(prepared) => *prepared,
            ToolPreparationOutcome::Completed(outcome) => {
                let mut record = outcome.record;
                record.call_id = Some(call_id);
                return ToolInvocationReply::from_output(record.output.clone()).with_record(record);
            }
        };
        let registration = ProcessRegistration::session_start_draft(
            ProcessInput::ToolCall {
                call: prepared_call.clone(),
            },
            // Tool-call rows are journaled and idempotent by their start
            // key, so recovery may re-execute them (ADR 0019, ADR 0107).
            crate::RecoveryContract::Rerunnable,
        )
        .with_start_key(Some(crate::StartKey::for_orchestration_call(
            self.admitted_scope().scope(),
            &handle_id,
            0,
        )));
        let (registration, env_spec) = self.process_start_execution_env(registration);
        let started = match self
            .dispatch
            .processes
            .start(
                &self.session_id,
                registration,
                crate::ProcessStartOptions::new()
                    .with_initial_observer(self.session_id.clone())
                    .with_env_spec(env_spec),
                self.process_scope(self.parent_invocation.clone()),
            )
            .await
        {
            Ok(record) => record,
            Err(err) => return ToolInvocationReply::error(json!(err.to_string())),
        };

        let handle_value = Self::process_handle_json(&started.id.clone());
        let record = ToolCallRecord {
            call_id: Some(call_id),
            tool: prepared_call.tool_name,
            args: prepared_call.args,
            output: ToolCallOutput::success(handle_value.clone()),
        };
        ToolInvocationReply::success(handle_value).with_record(record)
    }

    fn recorded_process_reply(
        call_id: String,
        tool: impl Into<String>,
        args: serde_json::Value,
        output: ToolCallOutput,
    ) -> ToolInvocationReply {
        let record = ToolCallRecord {
            call_id: Some(call_id),
            tool: tool.into(),
            args,
            output: output.clone(),
        };
        ToolInvocationReply::from_output(output).with_record(record)
    }

    fn recorded_process_error(
        call_id: String,
        tool: &'static str,
        args: serde_json::Value,
        message: impl Into<String>,
    ) -> ToolInvocationReply {
        let output = ToolInvocationReply::error(json!(message.into())).output;
        Self::recorded_process_reply(call_id, tool, args, output)
    }

    pub(crate) async fn await_process_handle(
        &self,
        call_id: String,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        let args = json!({ "handle": handle.clone() });
        let process_id = match Self::parse_process_handle(&handle) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Self::recorded_process_error(call_id, "await_process", args, err);
            }
        };
        if let Err(err) = self.authorize_handle(&process_id).await {
            return Self::recorded_process_error(call_id, "await_process", args, err.to_string());
        }
        let output = self
            .await_process_with_cancellation(
                &process_id,
                self.parent_invocation.clone(),
                self.cancellation_token.clone(),
            )
            .await;
        let output = match output {
            Ok(output) => output.into_tool_output(),
            Err(crate::PluginError::RuntimeEffectController(err)) => {
                self.record_nested_effect_error(err.clone());
                ToolInvocationReply::error(json!(err.to_string())).output
            }
            Err(err) => ToolInvocationReply::error(json!(err.to_string())).output,
        };
        let mut outcome = crate::tool_dispatch::normalized_outcome(
            self.dispatch.as_ref(),
            "await_process".to_string(),
            args,
            ToolOutcome::from_output(output),
        )
        .await;
        outcome.record.call_id = Some(call_id);
        ToolInvocationReply::from_output(outcome.record.output.clone()).with_record(outcome.record)
    }

    pub(crate) async fn signal_process_handle(
        &self,
        call_id: String,
        handle: serde_json::Value,
        signal_name: String,
        payload: serde_json::Value,
    ) -> ToolInvocationReply {
        let args = json!({
            "handle": handle.clone(),
            "signal_name": signal_name.clone(),
            "payload": payload.clone()
        });
        let process_id = match Self::parse_process_handle(&handle) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Self::recorded_process_error(call_id, "signal_process", args, err);
            }
        };
        if let Err(err) = self.authorize_handle(&process_id).await {
            return Self::recorded_process_error(call_id, "signal_process", args, err.to_string());
        }
        let signal_id = format!("process-{call_id}");
        let result = self
            .dispatch
            .processes
            .signal_possessed(
                &self.session_id,
                &process_id,
                signal_name,
                signal_id,
                payload,
                self.process_scope(self.parent_invocation.clone()),
            )
            .await;
        let output = match result {
            Ok(event) => ToolCallOutput::success(json!({
                "process_id": event.process_id,
                "sequence": event.sequence,
            })),
            Err(err) => ToolInvocationReply::error(json!(format!("signal failed: {err}"))).output,
        };
        Self::recorded_process_reply(call_id, "signal_process", args, output)
    }

    pub(crate) async fn cancel_process_handle(
        &self,
        call_id: String,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        let args = json!({ "handle": handle.clone() });
        let process_id = match Self::parse_process_handle(&handle) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Self::recorded_process_error(call_id, "cancel_process", args, err);
            }
        };
        if let Err(err) = self.authorize_handle(&process_id).await {
            return Self::recorded_process_error(call_id, "cancel_process", args, err.to_string());
        }
        let result = self
            .dispatch
            .processes
            .cancel(
                &self.session_id,
                &process_id,
                self.process_scope(self.parent_invocation.clone()),
            )
            .await;
        let output = match result {
            Ok(status) => ToolCallOutput::success(Self::process_status_value(&status)),
            Err(err) => ToolInvocationReply::error(json!(format!("cancel failed: {err}"))).output,
        };
        Self::recorded_process_reply(call_id, "cancel_process", args, output)
    }
}
