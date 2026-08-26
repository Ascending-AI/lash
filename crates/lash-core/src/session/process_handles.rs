use serde_json::json;

use super::execution_context::RuntimeExecutionContext;
use super::tool_execution::ToolInvocationReply;
#[cfg(test)]
use crate::tool_dispatch::ToolPreparationOutcome;
#[cfg(test)]
use crate::{ProcessInput, ProcessRegistration};
use crate::{ToolCallOutput, ToolCallRecord};

const PROCESS_HANDLE_KIND: &str = "process";

enum HandleAuthority {
    RunLocalPossession,
    SessionVisible,
}

impl RuntimeExecutionContext<'_> {
    #[cfg(test)]
    pub(super) fn process_handle_value(id: &str) -> serde_json::Value {
        Self::process_handle_json(id)
    }

    /// Encodes the minimal stable process handle for code-executor implementors returning a process
    /// reference through JSON.
    pub fn process_handle_json(id: &str) -> serde_json::Value {
        json!({
            "__handle__": "process",
            "id": id,
        })
    }

    pub(super) fn process_status_value(status: &crate::ProcessRecord) -> serde_json::Value {
        json!({
            "process_id": status.id,
            "status": status.status.label(),
        })
    }

    pub(super) fn parse_process_handle(handle: &serde_json::Value) -> Result<String, String> {
        let kind = handle
            .get("__handle__")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Invalid process handle: missing `__handle__`".to_string())?;
        if kind != PROCESS_HANDLE_KIND {
            return Err(format!("Invalid process handle kind: {kind}"));
        }
        let id = handle
            .get("id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "Invalid process handle: missing `id`".to_string())?;
        Ok(id.to_string())
    }

    async fn authorize_handle(
        &self,
        handle_id: &str,
    ) -> Result<HandleAuthority, crate::PluginError> {
        if self.is_run_local_process(handle_id) {
            return Ok(HandleAuthority::RunLocalPossession);
        }
        let handle_id = handle_id.to_string();
        self.dispatch
            .processes
            .validate_visible(
                &self.session_id,
                std::slice::from_ref(&handle_id),
                self.process_scope(self.parent_invocation.clone()),
            )
            .await?;
        Ok(HandleAuthority::SessionVisible)
    }

    #[cfg(test)]
    pub(super) async fn start_tool_process(
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
        let registration = match self
            .attach_captured_process_execution_env(registration)
            .await
        {
            Ok(registration) => registration,
            Err(err) => return ToolInvocationReply::error(json!(err.to_string())),
        };
        if let Err(err) = self
            .dispatch
            .processes
            .start(
                &self.session_id,
                registration,
                crate::ProcessStartOptions::new().with_initial_observer(self.session_id.clone()),
                self.process_scope(self.parent_invocation.clone()),
            )
            .await
        {
            return ToolInvocationReply::error(json!(err.to_string()));
        }

        let handle_value = Self::process_handle_value(&handle_id);
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

    pub(super) async fn await_process_handle(
        &self,
        call_id: String,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        let started = self.dispatch.clock.now();
        let args = json!({ "handle": handle.clone() });
        let handle_id = match Self::parse_process_handle(&handle) {
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
        if let Err(err) = self.authorize_handle(&handle_id).await {
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
                &handle_id,
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
        Self::recorded_process_reply(
            call_id,
            "await_process",
            args,
            output,
            self.elapsed_ms(started),
        )
    }

    pub(super) async fn signal_process_handle(
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
        let handle_id = match Self::parse_process_handle(&handle) {
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
        if let Err(err) = self.authorize_handle(&handle_id).await {
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
                &handle_id,
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

    pub(super) async fn cancel_process_handle(
        &self,
        call_id: String,
        handle: serde_json::Value,
    ) -> ToolInvocationReply {
        let started = self.dispatch.clock.now();
        let args = json!({ "handle": handle.clone() });
        let handle_id = match Self::parse_process_handle(&handle) {
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
        if let Err(err) = self.authorize_handle(&handle_id).await {
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
                &handle_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::PluginHost;
    use crate::runtime::RuntimeEffectControllerHandle;
    use crate::tool_dispatch::ToolDispatchContext;
    use crate::{
        PreparedToolCall, ProcessRegistry, ToolCall, ToolDefinition, ToolOutcome, ToolPrepareCall,
        ToolProvider,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PrepareRecordingTool {
        prepares: Arc<AtomicUsize>,
    }

    fn process_tool_definition() -> ToolDefinition {
        ToolDefinition::raw(
            "tool:process_prepare",
            "process_prepare",
            "Records preparation before background registration.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" }
                },
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
    }

    #[async_trait::async_trait]
    impl ToolProvider for PrepareRecordingTool {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![process_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "process_prepare").then(|| Arc::new(process_tool_definition().contract()))
        }

        async fn prepare_tool_call(
            &self,
            call: ToolPrepareCall<'_>,
        ) -> Result<PreparedToolCall, ToolOutcome> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            Ok(PreparedToolCall::from_parts(
                call.pending.call_id,
                call.tool_id,
                call.pending.tool_name,
                call.pending.args,
                call.pending.replay,
                serde_json::json!({ "prepared": true }),
            ))
        }

        async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::ok(serde_json::json!({
                "payload": call.context.prepared_payload().clone(),
            }))
        }
    }

    #[tokio::test]
    async fn process_handle_start_registers_prepared_tool_call() {
        let prepares = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn ToolProvider> = Arc::new(PrepareRecordingTool {
            prepares: Arc::clone(&prepares),
        });
        let plugins = PluginHost::empty()
            .build_session("root", None)
            .expect("plugin session");
        let tools = Arc::clone(&provider);
        let tool_catalog = Arc::new(crate::ToolCatalog::from_tools(
            provider.tool_manifests(),
            BTreeMap::new(),
        ));
        let host = Arc::new(crate::testing::MockSessionManager::default());
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host.clone(),
            trigger_router: None,
            effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
                crate::InlineRuntimeEffectController::default(),
            )),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: "session".to_string(),
            agent_frame_id: String::new(),
            event_tx,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            recorded_intent_outcomes:
                crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
        });
        let context = RuntimeExecutionContext::new(
            "session".to_string(),
            dispatch,
            Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
            Arc::new(crate::SessionAttachmentStore::in_memory()),
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        )
        .with_execution_env_spec(crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::runtime::tests::helpers::standard_test_policy(),
        ));

        let started = context
            .start_tool_process(
                "async-call-1".to_string(),
                "process_prepare".to_string(),
                serde_json::json!({ "input": "live" }),
            )
            .await;
        let handle_json = started.output.value_for_projection();
        let crate::ToolCallOutcome::Success(_handle) = started.output.outcome else {
            panic!("expected process handle output");
        };
        assert_eq!(
            handle_json.get("id").and_then(|value| value.as_str()),
            Some("async-call-1")
        );
        assert_eq!(prepares.load(Ordering::SeqCst), 1);
        let record = host
            .process_registry
            .get_process("async-call-1")
            .await
            .expect("read process")
            .expect("registered process");
        host.process_registry
            .remove_observer(
                "session",
                "async-call-1",
                crate::ProcessObserverBy::host("remove-test-observer"),
            )
            .await
            .expect("remove persisted observer");
        assert!(
            !host
                .process_registry
                .is_observer("session", "async-call-1")
                .await
                .expect("check removed observer"),
            "tool-started process must rely on its persisted observer edge"
        );
        let ProcessInput::ToolCall { call } = record.input.as_ref() else {
            panic!("expected prepared tool call process input");
        };
        assert_eq!(call.tool_name, "process_prepare");
        assert_eq!(call.args, serde_json::json!({ "input": "live" }));
        assert_eq!(
            call.prepared_payload,
            serde_json::json!({ "prepared": true })
        );

        let awaited = context
            .await_process_handle("await-async-call-1".to_string(), handle_json)
            .await;

        assert!(!awaited.output.is_success());
        let await_error = awaited.output.value_for_projection().to_string();
        assert!(
            await_error
                .contains("process handle `async-call-1` is not live or visible in this session"),
            "revoked tool-started handle must return the typed visibility miss: {await_error}"
        );
        let record = awaited.record.expect("await record");
        assert_eq!(record.call_id.as_deref(), Some("await-async-call-1"));
        assert_eq!(record.tool, "await_process");
    }

    #[tokio::test]
    async fn process_handle_signal_appends_event_from_foreground() {
        let provider: Arc<dyn ToolProvider> = Arc::new(PrepareRecordingTool {
            prepares: Arc::new(AtomicUsize::new(0)),
        });
        let plugins = PluginHost::empty()
            .build_session("root", None)
            .expect("plugin session");
        let tool_catalog = Arc::new(crate::ToolCatalog::from_tools(
            provider.tool_manifests(),
            BTreeMap::new(),
        ));
        let host = Arc::new(crate::testing::MockSessionManager::default());
        host.process_registry
            .register_process(
                ProcessRegistration::new(
                    "target-process",
                    ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    crate::RecoveryContract::ExternallyOwned,
                    crate::ProcessProvenance::host(),
                )
                .with_extra_event_types([crate::ProcessEventType {
                    name: "signal.ready".to_string(),
                    payload_schema: crate::LashSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                }]),
            )
            .await
            .expect("register target process");
        host.process_registry
            .add_observer(
                "session",
                "target-process",
                crate::ProcessObserverBy::host("foreground-signal-test"),
            )
            .await
            .expect("observe target process");
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools: provider,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host.clone(),
            trigger_router: None,
            effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
                crate::InlineRuntimeEffectController::default(),
            )),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: "session".to_string(),
            agent_frame_id: String::new(),
            event_tx,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            recorded_intent_outcomes:
                crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
        });
        let context = RuntimeExecutionContext::new(
            "session".to_string(),
            dispatch,
            Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
            Arc::new(crate::SessionAttachmentStore::in_memory()),
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        );

        let handle = json!({ "__handle__": "process", "id": "target-process" });
        let signalled = context
            .signal_process_handle(
                "signal-1".to_string(),
                handle,
                "ready".to_string(),
                json!({ "kind": "ping" }),
            )
            .await;

        assert!(
            signalled.output.is_success(),
            "{:?}",
            signalled.output.value_for_projection()
        );
        let record = signalled.record.expect("signal record");
        assert_eq!(record.call_id.as_deref(), Some("signal-1"));
        assert_eq!(record.tool, "signal_process");
        let events = host
            .process_registry
            .events_after("target-process", 0)
            .await
            .expect("list events");
        assert!(
            events.iter().any(|event| event.event_type == "signal.ready"
                && event.payload.get("kind") == Some(&json!("ping"))),
            "expected appended signal.ready event, got {events:?}"
        );
    }

    #[tokio::test]
    async fn process_handle_operations_share_one_authority_rule() {
        let provider: Arc<dyn ToolProvider> = Arc::new(PrepareRecordingTool {
            prepares: Arc::new(AtomicUsize::new(0)),
        });
        let plugins = PluginHost::empty()
            .build_session("root", None)
            .expect("plugin session");
        let tool_catalog = Arc::new(crate::ToolCatalog::from_tools(
            provider.tool_manifests(),
            BTreeMap::new(),
        ));
        let host = Arc::new(crate::testing::MockSessionManager::default());
        host.process_registry
            .register_process(
                ProcessRegistration::new(
                    "hidden-process",
                    ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    crate::RecoveryContract::ExternallyOwned,
                    crate::ProcessProvenance::host(),
                )
                .with_extra_event_types([crate::ProcessEventType {
                    name: "signal.ready".to_string(),
                    payload_schema: crate::LashSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                }]),
            )
            .await
            .expect("register hidden process");
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
        let dispatch = Arc::new(ToolDispatchContext {
            plugins,
            tools: provider,
            tool_registry: None,
            tool_catalog,
            sessions: host.clone(),
            session_lifecycle: host.clone(),
            session_graph: host.clone(),
            processes: host.clone(),
            trigger_router: None,
            effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
                crate::InlineRuntimeEffectController::default(),
            )),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_id: "session".to_string(),
            agent_frame_id: String::new(),
            event_tx,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            recorded_intent_outcomes:
                crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: std::sync::Arc::new(crate::SystemClock),
        });
        let context = RuntimeExecutionContext::new(
            "session".to_string(),
            dispatch,
            Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
            Arc::new(crate::SessionAttachmentStore::in_memory()),
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        );
        let handle = json!({
            "__handle__": "process",
            "id": "hidden-process"
        });

        let awaited = context
            .await_process_handle("await-hidden-process".to_string(), handle.clone())
            .await;
        let signalled = context
            .signal_process_handle(
                "signal-hidden-process".to_string(),
                handle.clone(),
                "ready".to_string(),
                serde_json::Value::Null,
            )
            .await;
        let cancelled = context
            .cancel_process_handle("cancel-hidden-process".to_string(), handle.clone())
            .await;

        let visibility_miss = awaited.output.value_for_projection();
        for (operation, reply) in [
            ("await", &awaited),
            ("signal", &signalled),
            ("cancel", &cancelled),
        ] {
            assert!(
                !reply.output.is_success(),
                "{operation} unexpectedly operated an unpossessed, unobserved handle"
            );
            let error = reply.output.value_for_projection();
            assert_eq!(
                error, visibility_miss,
                "{operation} must return the exact same typed visibility miss"
            );
            assert!(
                error.to_string().contains(
                    "process handle `hidden-process` is not live or visible in this session"
                ),
                "{operation} must return the shared typed visibility miss: {error}"
            );
        }
        assert_eq!(
            awaited
                .record
                .as_ref()
                .and_then(|record| record.call_id.as_deref()),
            Some("await-hidden-process")
        );
        assert_eq!(
            cancelled
                .record
                .as_ref()
                .and_then(|record| record.call_id.as_deref()),
            Some("cancel-hidden-process")
        );

        for process_id in ["local-signal", "local-cancel", "local-await"] {
            let mut registration = ProcessRegistration::new(
                process_id,
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
            );
            if process_id == "local-signal" {
                registration = registration.with_extra_event_types([crate::ProcessEventType {
                    name: "signal.ready".to_string(),
                    payload_schema: crate::LashSchema::any(),
                    semantics: crate::ProcessEventSemanticsSpec::default(),
                }]);
            }
            host.process_registry
                .register_process(registration)
                .await
                .expect("register run-local process without observer edge");
            context.record_started_process(process_id);
        }
        host.process_registry
            .complete_process(
                "local-await",
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!(
                    "local done"
                ))),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete run-local await process");
        let local_handle = |process_id: &str| {
            json!({
                "__handle__": "process",
                "id": process_id,
            })
        };
        let local_signal = context
            .signal_process_handle(
                "signal-local".to_string(),
                local_handle("local-signal"),
                "ready".to_string(),
                serde_json::Value::Null,
            )
            .await;
        let local_cancel = context
            .cancel_process_handle("cancel-local".to_string(), local_handle("local-cancel"))
            .await;
        let local_await = context
            .await_process_handle("await-local".to_string(), local_handle("local-await"))
            .await;
        for (operation, reply) in [
            ("await", &local_await),
            ("signal", &local_signal),
            ("cancel", &local_cancel),
        ] {
            assert!(
                reply.output.is_success(),
                "{operation} must accept run-local possession without an observer edge: {:?}",
                reply.output.value_for_projection()
            );
        }
        let local_prune = host
            .process_registry
            .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
            .await
            .expect("prune terminal run-local fixtures");
        assert_eq!(local_prune.pruned_processes, 1);

        host.process_registry
            .complete_process(
                "hidden-process",
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!(
                    "done"
                ))),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete observed process");
        host.process_registry
            .add_observer(
                "session",
                "hidden-process",
                crate::ProcessObserverBy::host("observe-hidden-process"),
            )
            .await
            .expect("observe process");
        let retained = host
            .process_registry
            .get_process("hidden-process")
            .await
            .expect("read observed process")
            .expect("observed process remains retained");
        let retained_bytes =
            serde_json::to_vec(&retained).expect("serialize retained terminal process");
        let terminal_signal = context
            .signal_process_handle(
                "signal-terminal-process".to_string(),
                handle.clone(),
                "ready".to_string(),
                serde_json::Value::Null,
            )
            .await;
        assert!(!terminal_signal.output.is_success());
        assert!(
            terminal_signal
                .output
                .value_for_projection()
                .to_string()
                .contains("already terminal"),
            "signaling a retained terminal process must return the typed terminal error"
        );
        let after_rejected_signal = host
            .process_registry
            .get_process("hidden-process")
            .await
            .expect("read terminal process after rejected signal")
            .expect("terminal process remains retained");
        assert_eq!(
            serde_json::to_vec(&after_rejected_signal)
                .expect("serialize terminal process after rejected signal"),
            retained_bytes,
            "a rejected terminal signal must leave prune eligibility byte-stable"
        );

        let prune = host
            .process_registry
            .prune_terminal_processes(
                retained.updated_at_ms.saturating_add(1),
                None,
                crate::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("prune observed process");
        assert_eq!(prune.pruned_processes, 1);
        assert!(matches!(
            host.process_registry.get_process("hidden-process").await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ));
        let pruned_await = context
            .await_process_handle("await-pruned-process".to_string(), handle.clone())
            .await;
        assert!(
            pruned_await.output.is_success(),
            "a pruned await must be information in turn history: {:?}",
            pruned_await.output.value_for_projection()
        );
        let rendered = pruned_await.output.value_for_projection().to_string();
        assert!(
            rendered.contains("process_no_longer_retained"),
            "pruned await must render the typed information code: {rendered}"
        );
        let history_record = pruned_await
            .record
            .expect("record pruned await in turn history");
        assert!(
            history_record.output.is_success(),
            "turn history must retain pruned await as information, not tool failure"
        );

        let pruned_cancel = context
            .cancel_process_handle("cancel-pruned-process".to_string(), handle.clone())
            .await;
        assert!(!pruned_cancel.output.is_success());
        assert!(
            pruned_cancel
                .output
                .value_for_projection()
                .to_string()
                .contains("outcome is no longer retained")
        );
        let pruned_signal = context
            .signal_process_handle(
                "signal-pruned-process".to_string(),
                handle,
                "ready".to_string(),
                serde_json::Value::Null,
            )
            .await;
        assert!(!pruned_signal.output.is_success());
        assert!(
            pruned_signal
                .output
                .value_for_projection()
                .to_string()
                .contains("outcome is no longer retained")
        );
    }
}
