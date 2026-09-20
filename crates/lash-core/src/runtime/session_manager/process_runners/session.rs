use super::*;

impl RuntimeSessionServices {
    pub(in crate::runtime::session_manager::process_runners) async fn run_process_session_turn(
        &self,
        registration: crate::ProcessRegistration,
        mut create_request: crate::SessionCreateRequest,
        turn_input: crate::TurnInput,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<crate::ProcessAwaitOutput, crate::ProcessInfraError> {
        create_request = create_request.with_caused_by(crate::CausalRef::Process {
            process_id: registration.id.clone(),
        });
        let requested_child_session_id = create_request.session_id.clone();
        if cancellation.is_cancelled() {
            if let Some(child_session_id) = create_request.session_id.as_deref() {
                self.reclaim_prestart_cancelled_child_session(
                    &registration.id,
                    &SessionId::from(child_session_id),
                )
                .await?;
            }
            return Ok(cancelled_session_turn_output());
        }
        // `ProcessInput::SessionTurn` is durable input. Its `create_request`
        // carries only persisted policy, so fill an omitted provider_id from
        // the parent runtime policy before the child session is built.
        self.inherit_session_turn_provider_id(&mut create_request);
        let child = match Box::pin(self.managed.create_session(&self.current, create_request)).await
        {
            Ok(child) => child,
            Err(err) => {
                if cancellation.is_cancelled() {
                    if let Some(child_session_id) = requested_child_session_id.as_ref() {
                        self.reclaim_prestart_cancelled_child_session(
                            &registration.id,
                            child_session_id,
                        )
                        .await?;
                    }
                    return Ok(cancelled_session_turn_output());
                }
                return Ok(crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_create_failed",
                        err.to_string(),
                    )),
                ));
            }
        };
        let child_session_id = child.session_id.clone();
        // The child session's first turn is deliberately scoped by the
        // process identity that started it, so the crossing is spelled out.
        let child_turn_id = crate::TurnId::from(registration.id.as_str());
        // The process worker admitted this controller under `registration.id`.
        // Keep that execution authority through the child turn; session and
        // turn ids remain the turn's foreground routing and attribution.
        let request = match crate::SessionTurnRequest::new_process_backed(
            &child_session_id,
            &child_turn_id,
            turn_input,
            &registration.id,
            scoped_effect_controller,
        ) {
            Ok(request) => request,
            Err(err) => {
                if self
                    .close_or_reclaim_cancelled_session_turn(
                        &registration.id,
                        &child_session_id,
                        &cancellation,
                    )
                    .await?
                {
                    return Ok(cancelled_session_turn_output());
                }
                return Ok(crate::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_turn_scope_failed",
                        err.to_string(),
                    )),
                ));
            }
        };
        let mut turn = Box::pin(self.managed.start_turn(&self.current, &self.usage, request));
        let outcome = tokio::select! {
            biased;
            _ = cancellation.cancelled() => None,
            outcome = turn.as_mut() => Some(outcome),
        };
        let Some(outcome) = outcome else {
            // Dropping the managed-turn future aborts its inherited task-local
            // execution before this outer process reacquires the shared slot.
            drop(turn);
            crate::runtime::process_permit::ensure_process_execution_permit().await;
            self.reclaim_cancelled_child_session(&registration.id, &child_session_id)
                .await?;
            return Ok(cancelled_session_turn_output());
        };
        if cancellation.is_cancelled() {
            self.reclaim_cancelled_child_session(&registration.id, &child_session_id)
                .await?;
            return Ok(cancelled_session_turn_output());
        }
        Ok(match outcome {
            Ok(turn) => {
                let state = process_terminal_state_for_turn(&turn);
                if matches!(state, crate::ProcessStatus::Cancelled) {
                    self.reclaim_cancelled_child_session(&registration.id, &child_session_id)
                        .await?;
                } else if self
                    .close_or_reclaim_cancelled_session_turn(
                        &registration.id,
                        &child_session_id,
                        &cancellation,
                    )
                    .await?
                {
                    return Ok(cancelled_session_turn_output());
                }
                crate::ProcessAwaitOutput::from_tool_output(output_from_process_turn(
                    &registration,
                    &child_session_id,
                    turn,
                    state,
                ))
            }
            Err(err) => {
                if self
                    .close_or_reclaim_cancelled_session_turn(
                        &registration.id,
                        &child_session_id,
                        &cancellation,
                    )
                    .await?
                {
                    return Ok(cancelled_session_turn_output());
                }
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
                    crate::ToolFailure::tool(
                        crate::ToolFailureClass::Execution,
                        "process_session_turn_failed",
                        err.to_string(),
                    ),
                ))
            }
        })
    }

    async fn reclaim_cancelled_child_session(
        &self,
        process_id: &ProcessId,
        child_session_id: &SessionId,
    ) -> Result<(), crate::ProcessInfraError> {
        if let Some(factory) = self.current.host.session_store_factory.as_ref()
            && let Some(store) = factory
                .open_existing_store_by_id(child_session_id)
                .await
                .map_err(|error| {
                    crate::ProcessInfraError::new(crate::PluginError::Session(format!(
                        "failed to inspect cancelled child session `{child_session_id}`: {error}"
                    )))
                })?
        {
            self.require_process_owned_child_session(process_id, child_session_id, store.as_ref())
                .await?;
        }
        self.managed
            .close_session(&self.current, child_session_id)
            .await
            .map_err(crate::ProcessInfraError::new)?;
        let Some(factory) = self.current.host.session_store_factory.as_ref() else {
            return Ok(());
        };
        factory.delete_session(child_session_id).await.map_err(|failure| {
            crate::ProcessInfraError::new(crate::PluginError::Session(format!(
                "failed to reclaim cancelled child session `{child_session_id}`: {}; partial report: {:?}",
                failure.stop, failure.partial
            )))
        })?;
        Ok(())
    }

    async fn reclaim_prestart_cancelled_child_session(
        &self,
        process_id: &ProcessId,
        child_session_id: &SessionId,
    ) -> Result<(), crate::ProcessInfraError> {
        let Some(factory) = self.current.host.session_store_factory.as_ref() else {
            return Ok(());
        };
        let Some(store) = factory
            .open_existing_store_by_id(child_session_id)
            .await
            .map_err(|error| {
                crate::ProcessInfraError::new(crate::PluginError::Session(format!(
                    "failed to inspect prestart cancelled child session `{child_session_id}`: {error}"
                )))
            })?
        else {
            return Ok(());
        };
        self.require_process_owned_child_session(process_id, child_session_id, store.as_ref())
            .await?;
        self.reclaim_cancelled_child_session(process_id, child_session_id)
            .await
    }

    async fn require_process_owned_child_session(
        &self,
        process_id: &ProcessId,
        child_session_id: &SessionId,
        store: &dyn crate::store::RuntimePersistence,
    ) -> Result<(), crate::ProcessInfraError> {
        let meta = store
            .load_session_meta()
            .await
            .map_err(|error| {
                crate::ProcessInfraError::new(crate::PluginError::Session(format!(
                    "failed to inspect prestart cancelled child session `{child_session_id}` metadata: {error}"
                )))
            })?
            .ok_or_else(|| {
                crate::ProcessInfraError::new(crate::PluginError::Session(format!(
                    "refusing to reclaim prestart cancelled child session `{child_session_id}` without durable ownership metadata"
                )))
            })?;
        let owned_by_process = matches!(
            &meta.relation,
            crate::SessionRelation::Child {
                caused_by: Some(crate::CausalRef::Process {
                    process_id: owner_process_id,
                }),
                ..
            } if owner_process_id == process_id
        );
        if !owned_by_process {
            return Err(crate::ProcessInfraError::new(crate::PluginError::Session(
                format!(
                    "refusing to reclaim prestart cancelled child session `{child_session_id}` not owned by process `{process_id}`"
                ),
            )));
        }
        Ok(())
    }

    async fn close_or_reclaim_cancelled_session_turn(
        &self,
        process_id: &ProcessId,
        child_session_id: &SessionId,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, crate::ProcessInfraError> {
        if cancellation.is_cancelled() {
            self.reclaim_cancelled_child_session(process_id, child_session_id)
                .await?;
            return Ok(true);
        }
        let _ = self
            .managed
            .close_session(&self.current, child_session_id)
            .await;
        if cancellation.is_cancelled() {
            self.reclaim_cancelled_child_session(process_id, child_session_id)
                .await?;
            return Ok(true);
        }
        Ok(false)
    }

    fn inherit_session_turn_provider_id(&self, create_request: &mut crate::SessionCreateRequest) {
        let Some(policy) = create_request.policy.as_mut() else {
            return;
        };
        if policy.recorded_provider_id().is_empty() {
            policy.provider_id = self.current.policy.provider_id.clone();
        }
    }
}

fn cancelled_session_turn_output() -> crate::ProcessAwaitOutput {
    crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
        crate::ToolCancellation::runtime("background session turn was cancelled"),
    ))
}

fn process_terminal_state_for_turn(turn: &crate::AssembledTurn) -> crate::ProcessStatus {
    match &turn.outcome {
        crate::TurnOutcome::Finished(_) | crate::TurnOutcome::AgentFrameSwitch { .. } => {
            crate::ProcessStatus::Completed
        }
        crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled { .. }) => {
            crate::ProcessStatus::Cancelled
        }
        crate::TurnOutcome::Stopped(_) => crate::ProcessStatus::Failed,
    }
}

/// Classify a non-cancelled child stop for the parent.
///
/// The `code` is the stop's own spelling, so a parent can tell a provider
/// error from a refusal without reading prose; the sentence is the fallback
/// message for a stop whose child authored no text of its own.
fn process_turn_stop_classification(
    stop: &crate::TurnStop,
) -> (crate::ToolFailureClass, &'static str, &'static str) {
    use crate::ToolFailureClass as Class;
    match stop {
        // Cancellation never reaches here: `output_from_process_turn` settles a
        // cancelled child before classifying a failure.
        crate::TurnStop::Cancelled { .. } => (
            Class::Execution,
            "process_session_turn_cancelled",
            "background session turn was cancelled",
        ),
        crate::TurnStop::Incomplete => (
            Class::Execution,
            "process_session_turn_incomplete",
            "background session turn ended before producing a result",
        ),
        crate::TurnStop::InvalidInput => (
            Class::InvalidRequest,
            "process_session_turn_invalid_input",
            "background session turn input was refused",
        ),
        crate::TurnStop::MaxTurns => (
            Class::ResourceLimit,
            "process_session_turn_max_turns",
            "background session turn reached its turn limit",
        ),
        crate::TurnStop::ToolFailure => (
            Class::Execution,
            "process_session_turn_tool_failure",
            "background session turn stopped on a failed tool call",
        ),
        crate::TurnStop::ProviderError => (
            Class::External,
            "process_session_turn_provider_error",
            "background session turn stopped on a provider error",
        ),
        crate::TurnStop::ContextOverflow => (
            Class::ResourceLimit,
            "process_session_turn_context_overflow",
            "background session turn exceeded the model's context window",
        ),
        crate::TurnStop::PluginAbort => (
            Class::Execution,
            "process_session_turn_plugin_abort",
            "background session turn was aborted by a plugin",
        ),
        crate::TurnStop::RuntimeError => (
            Class::Internal,
            "process_session_turn_runtime_error",
            "background session turn stopped on a runtime error",
        ),
        crate::TurnStop::SubmittedError { .. } => (
            Class::Execution,
            "process_session_turn_submitted_error",
            "background session turn submitted a failure without a reason",
        ),
        crate::TurnStop::ToolError { .. } => (
            Class::Execution,
            "process_session_turn_tool_error",
            "background session turn stopped on a tool error without a message",
        ),
    }
}

/// The child's own text for a stop that carries one.
///
/// `submit_error` and every other `task.fail` spelling land as a projected
/// [`crate::ToolFailure`], whose human reason is `message`; a value that
/// carries a bare `reason` string (a host-authored `SubmittedError`) is read
/// from that field. Nothing is invented: a stop with no authored text yields
/// `None` and the caller falls back to the stop's own sentence.
fn authored_stop_text(value: &serde_json::Value) -> Option<String> {
    ["reason", "message"]
        .into_iter()
        .filter_map(|field| value.get(field))
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

/// The child turn's first blocking issue: the root cause the assembler
/// recorded, ahead of any consequence it recorded afterwards.
fn first_blocking_issue(turn: &crate::AssembledTurn) -> Option<&crate::TurnIssue> {
    turn.errors
        .iter()
        .find(|issue| issue.severity == crate::TurnIssueSeverity::Blocking)
}

/// Project a failed child turn onto the failure the parent's spawn result,
/// the child's process record and its terminal process event all carry.
///
/// The child's own reason is the message wherever the child authored one, the
/// stop's code keeps the categories apart, and the typed diagnostics — the
/// child's [`crate::TurnFailureKind`]/[`crate::TurnFailureCode`] and the
/// stop's projected value — ride the existing `raw` channel, bounded.
fn failure_from_process_turn(turn: &crate::AssembledTurn) -> crate::ToolFailure {
    let crate::TurnOutcome::Stopped(stop) = &turn.outcome else {
        return crate::ToolFailure::tool(
            crate::ToolFailureClass::Internal,
            "process_session_turn_failed",
            "background session turn failed",
        );
    };
    let (class, code, sentence) = process_turn_stop_classification(stop);
    let issue = first_blocking_issue(turn);
    let (authored, stop_value) = match stop {
        crate::TurnStop::SubmittedError { value } | crate::TurnStop::ToolError { value, .. } => {
            (authored_stop_text(value), Some(value))
        }
        _ => (
            issue
                .map(|issue| issue.message.trim())
                .filter(|message| !message.is_empty())
                .map(ToOwned::to_owned),
            None,
        ),
    };
    let message = match (authored, stop_value.is_some()) {
        // A child that authored its own terminal reason reaches the parent
        // verbatim: the parent model reads the child's words, not ours.
        (Some(text), true) => text,
        // A category stop has no child-authored terminal text, so the
        // assembler's blocking issue qualifies the stop's own sentence.
        (Some(text), false) => format!("{sentence}: {text}"),
        (None, _) => sentence.to_string(),
    };
    let mut failure = crate::ToolFailure::tool(
        class,
        code,
        lash_sansio::session_model::truncate_raw_error(&message),
    );
    failure.raw = process_turn_failure_raw(stop_value, issue).map(crate::ToolValue::untrusted_json);
    failure
}

/// Bounded diagnostics for a failed child turn, or `None` when the child
/// produced neither a projected stop value nor a blocking issue.
fn process_turn_failure_raw(
    stop_value: Option<&serde_json::Value>,
    issue: Option<&crate::TurnIssue>,
) -> Option<serde_json::Value> {
    let mut raw = serde_json::Map::new();
    if let Some(value) = stop_value {
        raw.insert(
            "stop".to_string(),
            serde_json::Value::String(lash_sansio::session_model::truncate_raw_error(
                &value.to_string(),
            )),
        );
    }
    if let Some(issue) = issue {
        raw.insert("kind".to_string(), issue.kind.as_str().into());
        if let Some(code) = issue.code.as_ref() {
            raw.insert("code".to_string(), code.as_str().into());
        }
        if let Some(retryable) = issue.retryable {
            raw.insert("retryable".to_string(), retryable.into());
        }
        raw.insert(
            "issue".to_string(),
            serde_json::Value::String(lash_sansio::session_model::truncate_raw_error(
                issue.message.trim(),
            )),
        );
    }
    (!raw.is_empty()).then_some(serde_json::Value::Object(raw))
}

fn output_from_process_turn(
    registration: &crate::ProcessRegistration,
    child_session_id: &SessionId,
    turn: crate::AssembledTurn,
    state: crate::ProcessStatus,
) -> crate::ToolCallOutput {
    if state == crate::ProcessStatus::Cancelled {
        let cancellation = match &turn.outcome {
            crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled { evidence }) => {
                crate::ToolCancellation {
                    message: evidence
                        .reason
                        .clone()
                        .unwrap_or_else(|| "background session turn was cancelled".to_string()),
                    source: crate::ToolFailureSource::Cancellation,
                    origin: Some(crate::CancelOrigin::TurnStopped),
                    raw: serde_json::to_value(evidence)
                        .ok()
                        .map(crate::ToolValue::untrusted_json),
                }
            }
            _ => crate::ToolCancellation::runtime("background session turn was cancelled"),
        };
        return crate::ToolCallOutput::cancelled(cancellation);
    }
    if state == crate::ProcessStatus::Failed {
        return crate::ToolCallOutput::failure(failure_from_process_turn(&turn));
    }
    crate::ToolCallOutput::success(serde_json::json!({
        "process_id": registration.id,
        "child_session_id": child_session_id,
        "turn": turn,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-2975: every non-cancelled child stop reaches the parent as its own
    /// failure. The pre-fix runner answered all ten with one code and one
    /// sentence, so a parent model could not tell a retryable provider error
    /// from a deliberate refusal.
    #[test]
    fn every_non_cancelled_child_stop_is_distinguishable_to_the_parent() {
        let stops = [
            crate::TurnStop::Incomplete,
            crate::TurnStop::InvalidInput,
            crate::TurnStop::MaxTurns,
            crate::TurnStop::ToolFailure,
            crate::TurnStop::ProviderError,
            crate::TurnStop::ContextOverflow,
            crate::TurnStop::PluginAbort,
            crate::TurnStop::RuntimeError,
            crate::TurnStop::SubmittedError {
                value: serde_json::json!({ "reason": "missing shard amber" }),
            },
            crate::TurnStop::ToolError {
                tool_name: "submit_error".to_string(),
                value: serde_json::json!({
                    "class": "execution",
                    "code": "subagent_submit_error",
                    "message": "missing shard amber",
                }),
            },
        ];

        let mut seen = std::collections::BTreeSet::new();
        for stop in stops {
            let failure = failed_child_failure(stop.clone());
            assert!(
                seen.insert(failure.code.clone()),
                "two stops share the code `{}`: {stop:?}",
                failure.code
            );
            assert_ne!(
                failure.message, "background session turn failed",
                "{stop:?} still collapses onto the shared generic message"
            );
            assert_eq!(failure.source, crate::ToolFailureSource::Tool);
        }
        assert_eq!(seen.len(), 10);
    }

    /// The child's own terminal words reach the parent verbatim: a parent
    /// model reads the child's reason, not a summary of it.
    #[test]
    fn a_child_authored_stop_reason_reaches_the_parent_verbatim() {
        let submitted = failed_child_failure(crate::TurnStop::SubmittedError {
            value: serde_json::json!({ "reason": "missing shard amber" }),
        });
        assert_eq!(submitted.message, "missing shard amber");
        assert_eq!(submitted.code, "process_session_turn_submitted_error");

        let tool_error = failed_child_failure(crate::TurnStop::ToolError {
            tool_name: "submit_error".to_string(),
            value: serde_json::json!({
                "class": "execution",
                "code": "subagent_submit_error",
                "message": "missing shard amber",
            }),
        });
        assert_eq!(tool_error.message, "missing shard amber");
        assert_eq!(tool_error.code, "process_session_turn_tool_error");
        assert!(
            tool_error
                .raw
                .as_ref()
                .map(crate::ToolValue::to_json_value)
                .is_some_and(|raw| raw["stop"]
                    .as_str()
                    .is_some_and(|stop| stop.contains("subagent_submit_error"))),
            "the projected stop rides the bounded `raw` channel: {tool_error:?}"
        );
    }

    /// A stop the child did not author text for carries the assembler's own
    /// blocking issue — including its typed kind and code — rather than a
    /// sentence with nothing behind it.
    #[test]
    fn a_category_stop_carries_the_child_turn_blocking_issue() {
        let mut turn =
            crate::testing::mock_assembled_turn(&SessionId::from("failing-child"), "unused");
        turn.outcome = crate::TurnOutcome::Stopped(crate::TurnStop::ProviderError);
        turn.errors = vec![crate::TurnIssue {
            severity: crate::TurnIssueSeverity::Blocking,
            kind: crate::TurnFailureKind::LlmProvider,
            code: Some(crate::TurnFailureCode::ContextOverflow),
            terminal_reason: None,
            message: "the request exceeded the model's context window".to_string(),
            raw: None,
            retryable: Some(false),
            provider_failure_kind: None,
        }];

        let failure = failure_from_process_turn(&turn);
        assert_eq!(failure.code, "process_session_turn_provider_error");
        assert_eq!(failure.class, crate::ToolFailureClass::External);
        assert_eq!(
            failure.message,
            "background session turn stopped on a provider error: the request exceeded the model's context window"
        );
        let raw = failure
            .raw
            .as_ref()
            .map(crate::ToolValue::to_json_value)
            .expect("typed diagnostics ride `raw`");
        assert_eq!(raw["kind"], serde_json::json!("llm_provider"));
        assert_eq!(raw["code"], serde_json::json!("context_overflow"));
        assert_eq!(raw["retryable"], serde_json::json!(false));
    }

    /// Run one stopped child turn through the runner's own projection, so the
    /// assertions cover the failure a parent, a process record and a terminal
    /// process event all read.
    fn failed_child_failure(stop: crate::TurnStop) -> crate::ToolFailure {
        let mut turn =
            crate::testing::mock_assembled_turn(&SessionId::from("failing-child"), "unused");
        turn.outcome = crate::TurnOutcome::Stopped(stop);
        let registration = crate::ProcessRegistration::new(
            "process:subagent:failing-child",
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::RecoveryContract::ExternallyOwned,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        );
        let state = process_terminal_state_for_turn(&turn);
        assert_eq!(
            state,
            crate::ProcessStatus::Failed,
            "precondition: the stop must fold to a failed process"
        );
        let output = output_from_process_turn(
            &registration,
            &SessionId::from("failing-child"),
            turn,
            state,
        );
        let crate::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("a failed child turn must project a tool failure");
        };
        failure
    }
    use crate::llm::types::LlmStreamEvent;
    use crate::runtime::tests::helpers::{
        MockCall, mock_provider, native_scope, runtime_with_plugins_and_tools_and_host,
    };
    use std::sync::Arc;

    struct ParkForever {
        started: tokio::sync::mpsc::Sender<()>,
    }

    #[async_trait::async_trait]
    impl crate::ToolProvider for ParkForever {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![park_forever_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "park_forever").then(|| Arc::new(park_forever_definition().contract()))
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            let _ = self.started.send(()).await;
            std::future::pending::<()>().await;
            unreachable!("the parked tool never completes")
        }
    }

    fn park_forever_definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:park_forever",
            "park_forever",
            "park the calling turn forever",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object", "additionalProperties": false }),
        )
    }

    async fn cancelled_mid_turn_subagent_reclaims_durable_child_rows(case: &str) {
        let child_session_id = SessionId::from(format!("cancelled-{case}-subagent-child"));
        let process_id = ProcessId::from(format!("process:subagent:cancelled-{case}"));
        let factory = crate::InMemorySessionStoreFactory::new();
        let host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        ))
        .with_session_store_factory(Arc::new(factory.clone()));
        let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(1);
        let transport = mock_provider(vec![MockCall {
            stream_events: vec![LlmStreamEvent::Part(crate::LlmOutputPart::ToolCall {
                call_id: format!("park-{case}"),
                tool_name: "park_forever".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            })],
            response: Ok(crate::LlmResponse::default()),
        }]);
        let runtime = runtime_with_plugins_and_tools_and_host(
            Vec::new(),
            Arc::new(ParkForever {
                started: started_tx,
            }),
            transport,
            host,
        )
        .await;
        let services = runtime
            .runtime_session_services()
            .expect("runtime session services");
        let foreign_session_id = SessionId::from(format!("unrelated-{case}-session"));
        factory
            .create_store(&crate::SessionStoreCreateRequest {
                session_id: foreign_session_id.clone(),
                relation: crate::SessionRelation::Root,
                pending_observer_intents: Vec::new(),
                policy: runtime.state.policy().clone(),
            })
            .await
            .expect("materialize unrelated durable session");
        let foreign_process_id = ProcessId::from(format!("process:subagent:foreign-{case}"));
        let foreign_plugin_init = runtime
            .session_state_service()
            .expect("session state")
            .session_plugin_init(&SessionId::from(runtime.session_id()))
            .await
            .expect("plugin init");
        let foreign_create_request = crate::SessionCreateRequest::child_session(
            runtime.session_id(),
            crate::SessionStartPoint::Empty,
            crate::PluginOptions::default(),
        )
        .with_session_id(&foreign_session_id)
        .with_plugin_source(crate::SessionPluginSource::ParentFork)
        .with_plugin_init(foreign_plugin_init);
        let foreign_registration = crate::ProcessRegistration::new(
            &foreign_process_id,
            crate::ProcessInput::SessionTurn {
                definition_key: "lash-subagent-session-turn:v1".to_string(),
                create_request: Box::new(foreign_create_request.clone()),
                turn_input: Box::new(crate::TurnInput::text("must not run")),
                output_contract: crate::ToolOutputContract::Static,
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        );
        let foreign_cancellation = tokio_util::sync::CancellationToken::new();
        foreign_cancellation.cancel();
        assert!(
            services
                .run_process_session_turn(
                    foreign_registration,
                    foreign_create_request,
                    crate::TurnInput::text("must not run"),
                    native_scope(crate::ExecutionScope::process(&foreign_process_id)),
                    foreign_cancellation,
                )
                .await
                .is_err(),
            "a pre-cancelled process must refuse to reclaim an unrelated session id"
        );
        assert!(
            factory
                .raw_store_for_testing(&foreign_session_id)
                .and_then(|store| store.raw_session_meta_for_testing())
                .is_some(),
            "ownership refusal must leave the unrelated parent session durable and reopenable"
        );
        let plugin_init = runtime
            .session_state_service()
            .expect("session state")
            .session_plugin_init(&SessionId::from(runtime.session_id()))
            .await
            .expect("plugin init");
        let create_request = crate::SessionCreateRequest::child_session(
            runtime.session_id(),
            crate::SessionStartPoint::Empty,
            crate::PluginOptions::default(),
        )
        .with_session_id(&child_session_id)
        .with_plugin_source(crate::SessionPluginSource::ParentFork)
        .with_plugin_init(plugin_init);
        let registration = crate::ProcessRegistration::new(
            &process_id,
            crate::ProcessInput::SessionTurn {
                definition_key: "lash-subagent-session-turn:v1".to_string(),
                create_request: Box::new(create_request.clone()),
                turn_input: Box::new(crate::TurnInput::text("park the child turn")),
                output_contract: crate::ToolOutputContract::Static,
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        );
        let replay_registration = registration.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let mut run = Box::pin(services.run_process_session_turn(
            registration,
            create_request.clone(),
            crate::TurnInput::text("park the child turn"),
            native_scope(crate::ExecutionScope::process(&process_id)),
            cancellation.clone(),
        ));
        tokio::select! {
            started = started_rx.recv() => assert_eq!(started, Some(())),
            outcome = run.as_mut() => panic!("{case} child turn completed before cancellation: {outcome:?}"),
        }

        let child_store = factory
            .raw_store_for_testing(&child_session_id)
            .expect("child durable store exists before cancellation");
        let before = [
            usize::from(child_store.raw_session_meta_for_testing().is_some()),
            usize::from(child_store.raw_head_revision_for_testing().is_some()),
            child_store.raw_graph_nodes_for_testing().len(),
            child_store.raw_pending_turn_inputs_for_testing().len(),
            child_store.raw_queued_work_for_testing().len(),
        ];
        assert!(
            before.iter().any(|count| *count > 0),
            "{case} child must materialize durable rows before cancellation"
        );

        cancellation.cancel();
        let output = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("cancelled child process settles");
        assert!(matches!(
            output
                .expect("cancelled child cleanup succeeds")
                .into_tool_output()
                .outcome,
            crate::ToolCallOutcome::Cancelled(_)
        ));

        let after = [
            usize::from(child_store.raw_session_meta_for_testing().is_some()),
            usize::from(child_store.raw_head_revision_for_testing().is_some()),
            child_store.raw_graph_nodes_for_testing().len(),
            child_store.raw_pending_turn_inputs_for_testing().len(),
            child_store.raw_queued_work_for_testing().len(),
        ];
        assert_eq!(
            after,
            [0, 0, 0, 0, 0],
            "cancelled {case} subagent child retained [session_meta, session_head, active_graph_nodes, pending_turn_inputs, queued_work_batches]; before={before:?}"
        );
        assert!(
            factory
                .open_existing_store_by_id(&child_session_id)
                .await
                .expect("inspect reclaimed child")
                .is_none(),
            "cancelled {case} child store must no longer be openable"
        );

        let replay = services
            .run_process_session_turn(
                replay_registration,
                create_request,
                crate::TurnInput::text("replayed cancelled child turn"),
                native_scope(crate::ExecutionScope::process(&process_id)),
                cancellation,
            )
            .await
            .expect("cancelled child replay cleanup is idempotent");
        assert!(matches!(
            replay.into_tool_output().outcome,
            crate::ToolCallOutcome::Cancelled(_)
        ));
        assert!(
            factory
                .open_existing_store_by_id(&child_session_id)
                .await
                .expect("inspect replayed reclaimed child")
                .is_none(),
            "cancelled {case} replay must not recreate the child store"
        );
    }

    #[tokio::test]
    async fn cancelled_mid_turn_subagent_reclaims_durable_rows() {
        Box::pin(cancelled_mid_turn_subagent_reclaims_durable_child_rows(
            "mid-turn",
        ))
        .await;
    }

    struct PermitSlots(Arc<tokio::sync::Semaphore>);

    #[async_trait::async_trait]
    impl crate::runtime::WorkerSlotSupplier for PermitSlots {
        async fn reserve_slot(
            &self,
            _: crate::runtime::WorkerSlotKind,
        ) -> crate::runtime::WorkerSlotPermit {
            crate::runtime::WorkerSlotPermit::new(self.0.clone().acquire_owned().await.unwrap())
        }
        fn try_reserve_slot(
            &self,
            _: crate::runtime::WorkerSlotKind,
        ) -> Option<crate::runtime::WorkerSlotPermit> {
            self.0
                .clone()
                .try_acquire_owned()
                .ok()
                .map(crate::runtime::WorkerSlotPermit::new)
        }
        fn available_slots(&self, _: crate::runtime::WorkerSlotKind) -> usize {
            self.0.available_permits()
        }
    }

    struct ParkPermit(tokio::sync::mpsc::Sender<()>);

    #[async_trait::async_trait]
    impl crate::ToolProvider for ParkPermit {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![park_forever_definition().manifest()]
        }
        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "park_forever").then(|| Arc::new(park_forever_definition().contract()))
        }
        async fn execute(&self, _: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            crate::runtime::process_permit::release_process_execution_permit_while(async {
                self.0.send(()).await.unwrap();
                std::future::pending::<crate::ToolAttemptOutcome>().await
            })
            .await
        }
    }

    #[tokio::test]
    async fn cancelled_session_turn_reacquires_budget_one_permit() {
        use crate::runtime::{WorkerSlotKind, WorkerSlotSupplier};
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let supplier = Arc::new(PermitSlots(semaphore.clone()));
        let permit = supplier.reserve_slot(WorkerSlotKind::Process).await;
        Box::pin(crate::runtime::process_permit::scope_process_execution_permit(
            supplier,
            permit,
            Arc::new(tokio::sync::Notify::new()),
            async {
                let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(1);
                let transport = mock_provider(vec![MockCall {
                    stream_events: vec![LlmStreamEvent::Part(crate::LlmOutputPart::ToolCall {
                        call_id: "park-slot".into(),
                        tool_name: "park_forever".into(),
                        input_json: "{}".into(),
                        replay: None,
                    })],
                    response: Ok(crate::LlmResponse::default()),
                }]);
                let host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                ))
                .with_session_store_factory(Arc::new(crate::InMemorySessionStoreFactory::new()));
                let runtime = runtime_with_plugins_and_tools_and_host(
                    Vec::new(),
                    Arc::new(ParkPermit(started_tx)),
                    transport,
                    host,
                )
                .await;
                let services = runtime.runtime_session_services().unwrap();
                let plugin_init = runtime
                    .session_state_service()
                    .expect("session state")
                    .session_plugin_init(&SessionId::from(runtime.session_id()))
                    .await
                    .expect("plugin init");
                let request = crate::SessionCreateRequest::child_session(
                    runtime.session_id(),
                    crate::SessionStartPoint::Empty,
                    crate::PluginOptions::default(),
                )
                .with_session_id("permit-child")
                .with_plugin_source(crate::SessionPluginSource::ParentFork)
                .with_plugin_init(plugin_init);
                let registration = crate::ProcessRegistration::new(
                    "permit-process",
                    crate::ProcessInput::SessionTurn {
                        definition_key: "lash-subagent-session-turn:v1".into(),
                        create_request: Box::new(request.clone()),
                        turn_input: Box::new(crate::TurnInput::text("park")),
                        output_contract: crate::ToolOutputContract::Static,
                    },
                    crate::RecoveryContract::Rerunnable,
                    crate::ProcessProvenance::host(),
                    crate::ProcessLifecyclePolicy::new(
                        crate::ParentScope::Host,
                        crate::OnParentEnd::Abandon,
                    ),
                );
                let cancellation = tokio_util::sync::CancellationToken::new();
                let mut run = Box::pin(services.run_process_session_turn(
                    registration,
                    request,
                    crate::TurnInput::text("park"),
                    native_scope(crate::ExecutionScope::process("permit-process")),
                    cancellation.clone(),
                ));
                tokio::select! {
                    started = started_rx.recv() => assert_eq!(started, Some(())),
                    result = run.as_mut() => panic!("child finished before parking: {result:?}"),
                }
                assert_eq!(
                    semaphore.available_permits(),
                    1,
                    "child parked the shared slot"
                );
                cancellation.cancel();
                let output = tokio::time::timeout(std::time::Duration::from_secs(5), run)
                    .await
                    .expect("cancelled session turn settles")
                    .unwrap();
                assert!(matches!(
                    output.into_tool_output().outcome,
                    crate::ToolCallOutcome::Cancelled(_)
                ));
                assert_eq!(
                    semaphore.available_permits(),
                    0,
                    "cancelled SessionTurn must reacquire its execution slot before returning"
                );
            },
        ))
        .await;
    }

    #[tokio::test]
    async fn child_turn_cancellation_evidence_survives_runner_record_and_parent_result() {
        use crate::{ProcessLifecycle as _, ProcessRegistrar as _};

        let process_id = crate::ProcessId::from("process:child-turn-cancellation-evidence");
        let child_session_id = crate::SessionId::from("child-turn-cancellation-evidence");
        let registration = crate::ProcessRegistration::new(
            process_id.clone(),
            crate::ProcessInput::External {
                metadata: serde_json::json!({"fixture": "child-turn-cancellation-evidence"}),
            },
            crate::RecoveryContract::ExternallyOwned,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        );
        let evidence = crate::TurnCancellationEvidence {
            request_id: "child-request-17".to_string(),
            origin: Some("opaque-host-origin".to_string()),
            reason: Some("child turn stopped by its host".to_string()),
            undelivered: crate::TurnCancelDisposition::Defer,
            mode: crate::TurnCancelMode::Immediate,
            honoured_after_step: None,
        };
        let mut turn = crate::testing::mock_assembled_turn(&child_session_id, "");
        turn.outcome = crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled {
            evidence: evidence.clone(),
        });

        let runner_output = output_from_process_turn(
            &registration,
            &child_session_id,
            turn,
            crate::ProcessStatus::Cancelled,
        );
        assert_child_turn_cancellation(&runner_output, &evidence);

        let registry = crate::TestLocalProcessRegistry::default();
        registry
            .register_process(registration)
            .await
            .expect("register child-turn process");
        let completion = registry
            .complete_process(
                &process_id,
                crate::ProcessAwaitOutput::from_tool_output(runner_output),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("persist child-turn cancellation");
        let recorded = completion
            .stored()
            .outcome
            .as_ref()
            .expect("terminal process outcome")
            .clone()
            .into_tool_output();
        assert_child_turn_cancellation(&recorded, &evidence);
        let parent_result = completion
            .stored()
            .outcome
            .clone()
            .expect("parent await result")
            .into_tool_output();
        assert_child_turn_cancellation(&parent_result, &evidence);
    }

    fn assert_child_turn_cancellation(
        output: &crate::ToolCallOutput,
        evidence: &crate::TurnCancellationEvidence,
    ) {
        let crate::ToolCallOutcome::Cancelled(cancellation) = &output.outcome else {
            panic!("expected child-turn cancellation, got {:?}", output.outcome);
        };
        assert_eq!(cancellation.origin, Some(crate::CancelOrigin::TurnStopped));
        assert_eq!(cancellation.message, evidence.reason.as_deref().unwrap());
        assert_eq!(
            cancellation
                .raw
                .as_ref()
                .map(crate::ToolValue::to_json_value),
            Some(serde_json::to_value(evidence).expect("encode turn cancellation evidence"))
        );
    }
}
