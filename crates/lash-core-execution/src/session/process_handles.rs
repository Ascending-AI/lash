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
    /// The incarnation rides inside the id rather than beside it, so a handle
    /// cannot name an incarnation it was not taken against, and the coordinator
    /// no longer has to back-fill one onto a handle a plugin returned.
    ///
    /// `process_id` rides beside the opaque id for the same reason
    /// [`ProcessHandleView`](crate::ProcessHandleView) carries it: the process
    /// tools take a `process_id`, and reading one out of the handle id is
    /// exactly what the opaque id forbids.
    pub fn process_handle_json(process_ref: &crate::ProcessRef) -> serde_json::Value {
        let mut record =
            lash_sansio::handle::handle_record_json(&lash_sansio::handle::HandleId::process(
                process_ref.process_id.as_str(),
                process_ref.incarnation.registration_sequence(),
            ));
        record["process_id"] = json!(process_ref.process_id.as_str());
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
    ) -> Result<crate::ProcessRef, String> {
        crate::ProcessRef::from_handle_json(handle)
    }

    /// FIG-653: observer validation enforces subscription relationships, not authorization.
    async fn authorize_handle(
        &self,
        process_ref: &crate::ProcessRef,
    ) -> Result<HandleAuthority, crate::PluginError> {
        if self.is_run_local_process(&process_ref.process_id) {
            return Ok(HandleAuthority::RunLocalPossession);
        }
        self.dispatch
            .processes
            .validate_visible_refs(
                &self.session_id,
                std::slice::from_ref(process_ref),
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
        let prepared_call = match self.prepare_tool_call(pending_call).await {
            ToolPreparationOutcome::Prepared(prepared) => *prepared,
            ToolPreparationOutcome::Completed(outcome) => {
                let mut record = outcome.record;
                record.call_id = Some(call_id);
                return ToolInvocationReply::from_output(record.output.clone()).with_record(record);
            }
        };
        let registration = ProcessRegistration::session_start_draft(
            handle_id.clone(),
            ProcessInput::ToolCall {
                call: prepared_call.clone(),
            },
            // Tool-call rows are journaled and idempotent by process id, so
            // recovery may re-execute them (ADR 0019).
            crate::RecoveryContract::Rerunnable,
        );
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

        let handle_value = Self::process_handle_json(&crate::ProcessRef::from_record(&started));
        let record = ToolCallRecord {
            call_id: Some(call_id),
            tool: prepared_call.tool_name,
            args: prepared_call.args,
            output: ToolCallOutput::success(handle_value.clone()),
            duration_ms: 0,
        };
        ToolInvocationReply::success(handle_value).with_record(record)
    }

    fn elapsed_ms(&self, started: std::time::Instant) -> u64 {
        self.dispatch
            .clock
            .now()
            .duration_since(started)
            .as_millis() as u64
    }

    fn recorded_process_reply(
        call_id: String,
        tool: impl Into<String>,
        args: serde_json::Value,
        output: ToolCallOutput,
        duration_ms: u64,
    ) -> ToolInvocationReply {
        let record = ToolCallRecord {
            call_id: Some(call_id),
            tool: tool.into(),
            args,
            output: output.clone(),
            duration_ms,
        };
        ToolInvocationReply::from_output(output).with_record(record)
    }

    fn recorded_process_error(
        call_id: String,
        tool: &'static str,
        args: serde_json::Value,
        message: impl Into<String>,
        duration_ms: u64,
    ) -> ToolInvocationReply {
        let output = ToolInvocationReply::error(json!(message.into())).output;
        Self::recorded_process_reply(call_id, tool, args, output, duration_ms)
    }

    pub(crate) async fn await_process_handle(
        &self,
        call_id: String,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        let started = self.dispatch.clock.now();
        let args = json!({ "handle": handle.clone() });
        let process_ref = match Self::parse_process_handle(&handle) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Self::recorded_process_error(
                    call_id,
                    "await_process",
                    args,
                    err,
                    self.elapsed_ms(started),
                );
            }
        };
        if let Err(err) = self.authorize_handle(&process_ref).await {
            return Self::recorded_process_error(
                call_id,
                "await_process",
                args,
                err.to_string(),
                self.elapsed_ms(started),
            );
        }
        let output = self
            .await_process_with_cancellation(
                &process_ref,
                self.parent_invocation.clone(),
                self.cancellation_token.clone(),
            )
            .await;
        let duration_ms = self.elapsed_ms(started);
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
            duration_ms,
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
        let started = self.dispatch.clock.now();
        let args = json!({
            "handle": handle.clone(),
            "signal_name": signal_name.clone(),
            "payload": payload.clone()
        });
        let process_ref = match Self::parse_process_handle(&handle) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Self::recorded_process_error(
                    call_id,
                    "signal_process",
                    args,
                    err,
                    self.elapsed_ms(started),
                );
            }
        };
        if let Err(err) = self.authorize_handle(&process_ref).await {
            return Self::recorded_process_error(
                call_id,
                "signal_process",
                args,
                err.to_string(),
                self.elapsed_ms(started),
            );
        }
        let signal_id = format!("process-{call_id}");
        let result = self
            .dispatch
            .processes
            .signal_possessed(
                &self.session_id,
                &process_ref.process_id,
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
        Self::recorded_process_reply(
            call_id,
            "signal_process",
            args,
            output,
            self.elapsed_ms(started),
        )
    }

    pub(crate) async fn cancel_process_handle(
        &self,
        call_id: String,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        let started = self.dispatch.clock.now();
        let args = json!({ "handle": handle.clone() });
        let process_ref = match Self::parse_process_handle(&handle) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Self::recorded_process_error(
                    call_id,
                    "cancel_process",
                    args,
                    err,
                    self.elapsed_ms(started),
                );
            }
        };
        if let Err(err) = self.authorize_handle(&process_ref).await {
            return Self::recorded_process_error(
                call_id,
                "cancel_process",
                args,
                err.to_string(),
                self.elapsed_ms(started),
            );
        }
        let result = self
            .dispatch
            .processes
            .cancel(
                &self.session_id,
                &process_ref.process_id,
                self.process_scope(self.parent_invocation.clone()),
            )
            .await;
        let output = match result {
            Ok(status) => ToolCallOutput::success(Self::process_status_value(&status)),
            Err(err) => ToolInvocationReply::error(json!(format!("cancel failed: {err}"))).output,
        };
        Self::recorded_process_reply(
            call_id,
            "cancel_process",
            args,
            output,
            self.elapsed_ms(started),
        )
    }
}
