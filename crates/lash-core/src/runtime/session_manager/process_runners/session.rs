use super::*;

impl RuntimeSessionServices {
    /// Run a `ProcessInput::SessionTurn`: initialize the recorded child
    /// session and drive its first turn through the shared session-turn path.
    ///
    /// Cancellation never tears the session down. The process token is the
    /// turn's own cancellation token inside the port, so a cancelled process
    /// leaves an ordinary cancelled turn inside a retained, reusable child
    /// session — and no pending or held turn input. Only the caller records
    /// the process terminal; the committed child turn is not itself a
    /// recorded process result.
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
        // `ProcessInput::SessionTurn` is durable input. Its `create_request`
        // carries only persisted policy, so fill an omitted provider_id from
        // the parent runtime policy before the child session is built.
        self.inherit_session_turn_provider_id(&mut create_request);
        // The child session's first turn is deliberately scoped by the
        // process identity that started it, so the crossing is spelled out.
        // The process worker admitted this controller under `registration.id`.
        // Keep that execution authority through the child turn; session and
        // turn ids remain the turn's foreground routing and attribution.
        let child_turn_id = crate::TurnId::from(registration.id.as_str());
        match Box::pin(self.initialize_session_and_run_turn(
            create_request,
            &registration.id,
            child_turn_id,
            turn_input,
            scoped_effect_controller,
            cancellation,
        ))
        .await
        {
            Ok(run) => {
                let child_session_id = run.session_id.clone();
                let state = process_terminal_state_for_turn(&run.turn);
                Ok(crate::ProcessAwaitOutput::from_tool_output(
                    output_from_process_turn(&registration, &child_session_id, run.turn, state),
                ))
            }
            Err(err) => {
                if let Some(session_id) = err.retained_session_id() {
                    tracing::debug!(
                        process_id = %registration.id,
                        session_id = %session_id,
                        "process session turn left a retained child session"
                    );
                }
                match err {
                    // A cancelled output is only produced once the port has
                    // settled (or never accepted) this turn's child input, so
                    // the substrate's cancelled terminal cannot strand a
                    // claimable input inside the retained session.
                    session_init::SessionTurnInitError::CancelledBeforeCreate
                    | session_init::SessionTurnInitError::CancelledAfterCreate { .. } => {
                        Ok(cancelled_session_turn_output())
                    }
                    // Authority validation is deterministic: retrying the
                    // attempt cannot change it, so it stays an ordinary
                    // terminal failure.
                    session_init::SessionTurnInitError::Request { source, .. } => {
                        Ok(crate::ProcessAwaitOutput::from_tool_output(
                            crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                                crate::ToolFailureClass::Execution,
                                "process_session_turn_scope_failed",
                                source.to_string(),
                            )),
                        ))
                    }
                    // Create, turn-commit, and reconcile failures are
                    // infrastructure failures: the durable child may hold
                    // uncommitted or unsettled state. The process must stay
                    // recoverable so a later attempt can resume or settle the
                    // child rather than recording a terminal over it.
                    session_init::SessionTurnInitError::Create { source, .. }
                    | session_init::SessionTurnInitError::Turn { source, .. }
                    | session_init::SessionTurnInitError::Reconcile { source, .. } => {
                        Err(crate::ProcessInfraError::new(*source))
                    }
                }
            }
        }
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
        EmptyTools, MockCall, mock_provider, named_turn_scope, native_process_scope,
        runtime_with_plugins_and_tools_and_host,
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

    async fn cancelled_mid_turn_subagent_retains_durable_child_session(case: &str) {
        let child_session_id = SessionId::from(format!("cancelled-{case}-subagent-child"));
        let process_id = ProcessId::from(format!("process:subagent:cancelled-{case}"));
        let factory = crate::InMemorySessionStoreFactory::new();
        let host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        ))
        .with_session_store_factory(Arc::new(factory.clone()));
        let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(1);
        let transport = mock_provider(vec![
            MockCall {
                stream_events: vec![LlmStreamEvent::Part(crate::LlmOutputPart::ToolCall {
                    call_id: format!("park-{case}"),
                    tool_name: "park_forever".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                })],
                response: Ok(crate::LlmResponse::default()),
            },
            // The retained child session runs an ordinary follow-up turn after
            // the cancelled first turn.
            MockCall {
                stream_events: Vec::new(),
                response: Ok(crate::LlmResponse {
                    parts: vec![crate::LlmOutputPart::Text {
                        text: "follow-up answered".to_string(),
                        response_meta: None,
                    }],
                    ..Default::default()
                }),
            },
        ]);
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
        let foreign_output = services
            .run_process_session_turn(
                foreign_registration,
                foreign_create_request,
                crate::TurnInput::text("must not run"),
                native_process_scope(&foreign_process_id),
                foreign_cancellation,
            )
            .await
            .expect("a pre-cancelled process settles cancelled, not an error");
        assert!(
            matches!(
                foreign_output.into_tool_output().outcome,
                crate::ToolCallOutcome::Cancelled(_)
            ),
            "a pre-cancelled process returns a cancelled output without touching the named session"
        );
        assert!(
            factory
                .raw_store_for_testing(&foreign_session_id)
                .and_then(|store| store.raw_session_meta_for_testing())
                .is_some(),
            "a cancelled process must leave the unrelated session durable and reopenable"
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
            native_process_scope(&process_id),
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
                .expect("cancelled child turn settles")
                .into_tool_output()
                .outcome,
            crate::ToolCallOutcome::Cancelled(_)
        ));

        // The child session is retained — lash never deletes a session because
        // a process was cancelled — and the cancelled turn's commit settles
        // its accepted input: the row remains only as a terminal receipt.
        assert!(
            child_store.raw_session_meta_for_testing().is_some(),
            "cancelled {case} subagent child keeps its durable session row"
        );
        assert!(
            child_store
                .raw_pending_turn_inputs_for_testing()
                .iter()
                .all(|row| row.2.kind().is_terminal() && row.3.is_none()),
            "cancelled {case} child leaves only settled turn-input receipts; before={before:?}"
        );
        let child_store = factory
            .open_existing_store_by_id(&child_session_id)
            .await
            .expect("inspect retained child")
            .expect("cancelled {case} child store stays openable");
        assert!(
            crate::store::TurnInputStore::list_pending_turn_inputs(
                child_store.as_ref(),
                &child_session_id,
            )
            .await
            .expect("list retained child inputs")
            .is_empty(),
            "a reopened read of the retained child finds no claimable input"
        );

        // The retained session is reusable: an ordinary follow-up turn runs.
        // The reopen replays the process-stamped relation the run recorded.
        let plan = crate::runtime::session_manager::session_init::resolve_session_init(
            &services.current,
            create_request
                .clone()
                .with_caused_by(crate::CausalRef::Process {
                    process_id: process_id.clone(),
                }),
        )
        .await
        .expect("resolve the retained child's init plan");
        let reopened = crate::runtime::session_manager::session_init::reopen_initialized_session(
            &services.current,
            &plan,
            child_store,
        )
        .await
        .expect("reopen the retained child through the ordinary path");
        let follow_up_turn_id = format!("{case}-follow-up-turn");
        reopened
            .handle
            .runtime
            .lock()
            .await
            .run_turn_assembled(
                crate::TurnInput::text("follow up after cancellation"),
                tokio_util::sync::CancellationToken::new(),
                named_turn_scope(
                    &child_session_id,
                    &crate::TurnId::from(follow_up_turn_id.as_str()),
                ),
            )
            .await
            .expect("the retained child session runs an ordinary follow-up turn");

        // A replayed attempt against the still-cancelled token creates nothing
        // new and leaves the retained child alone.
        let replay = services
            .run_process_session_turn(
                replay_registration,
                create_request,
                crate::TurnInput::text("replayed cancelled child turn"),
                native_process_scope(&process_id),
                cancellation,
            )
            .await
            .expect("cancelled child replay settles idempotently");
        assert!(matches!(
            replay.into_tool_output().outcome,
            crate::ToolCallOutcome::Cancelled(_)
        ));
        assert!(
            factory
                .open_existing_store_by_id(&child_session_id)
                .await
                .expect("inspect replayed child")
                .is_some(),
            "cancelled {case} replay leaves the retained child openable"
        );
    }

    #[tokio::test]
    async fn cancelled_mid_turn_subagent_retains_durable_rows() {
        Box::pin(cancelled_mid_turn_subagent_retains_durable_child_session(
            "mid-turn",
        ))
        .await;
    }

    /// A process session-turn fixture that parks the child's first turn inside
    /// a never-completing tool, returning everything the cancellation
    /// regressions need. The host runs with a short session-execution-lease
    /// TTL so a crashed attempt's claim dies quickly.
    struct ParkedSessionTurn {
        // The parent runtime whose services run the child process; it must
        // stay alive for the fixture's whole span.
        _runtime: crate::runtime::LashRuntime,
        services: Arc<crate::runtime::RuntimeSessionServices>,
        factory: crate::InMemorySessionStoreFactory,
        process_id: ProcessId,
        child_session_id: SessionId,
        create_request: crate::SessionCreateRequest,
        registration: crate::ProcessRegistration,
        started: tokio::sync::mpsc::Receiver<()>,
    }

    async fn parked_session_turn(case: &str) -> ParkedSessionTurn {
        let child_session_id = SessionId::from(format!("settle-{case}-child"));
        let process_id = ProcessId::from(format!("process:subagent:settle-{case}"));
        let factory = crate::InMemorySessionStoreFactory::new();
        let host = crate::EmbeddedRuntimeHost::new(
            crate::RuntimeHostConfig::in_memory(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1),
            )
            .with_lease_timings(
                crate::LeaseTimings::from_ttl(std::time::Duration::from_millis(120))
                    .expect("short test lease timings"),
            ),
        )
        .with_session_store_factory(Arc::new(factory.clone()));
        let (started_tx, started_rx) = tokio::sync::mpsc::channel(1);
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
        ParkedSessionTurn {
            _runtime: runtime,
            services,
            factory,
            process_id,
            child_session_id,
            create_request,
            registration,
            started: started_rx,
        }
    }

    /// A cancelled process whose child's final turn commit fails must stay
    /// recoverable: the runner surfaces the infrastructure failure instead of
    /// a terminal tool result, and the retained child keeps its accepted
    /// input open until a redelivery settles it.
    #[tokio::test]
    async fn failed_final_child_commit_cancellation_stays_recoverable() {
        let fixture = Box::pin(parked_session_turn("commit-failure")).await;
        let ParkedSessionTurn {
            services,
            factory,
            process_id,
            child_session_id,
            create_request,
            registration,
            mut started,
            ..
        } = fixture;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let mut run = Box::pin(services.run_process_session_turn(
            registration.clone(),
            create_request.clone(),
            crate::TurnInput::text("park the child turn"),
            native_process_scope(&process_id),
            cancellation.clone(),
        ));
        tokio::select! {
            started = started.recv() => assert_eq!(started, Some(())),
            outcome = run.as_mut() => panic!("child turn completed before cancellation: {outcome:?}"),
        }

        // Fail the cancelled turn's final commit: the child input stays
        // accepted-and-open, so this attempt must surface the infrastructure
        // failure rather than a terminal process result.
        let child_raw = factory
            .raw_store_for_testing(&child_session_id)
            .expect("child durable store exists");
        *child_raw.fail_next_runtime_commit.lock_recover() = Some(crate::StoreError::Contended);
        cancellation.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("failed child commit attempt settles");
        assert!(
            outcome.is_err(),
            "a failed final child commit is a recovery-level failure, not a terminal output"
        );

        // Redelivery after the durable cancellation reconciles the retained
        // child: once the dead attempt's claim expires, the accepted input is
        // cancelled durably and the process may terminalize `Cancelled`.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let replay_cancellation = tokio_util::sync::CancellationToken::new();
        replay_cancellation.cancel();
        let replay = services
            .run_process_session_turn(
                registration,
                create_request,
                crate::TurnInput::text("park the child turn"),
                native_process_scope(&process_id),
                replay_cancellation,
            )
            .await
            .expect("cancelled redelivery settles the retained child input");
        assert!(matches!(
            replay.into_tool_output().outcome,
            crate::ToolCallOutcome::Cancelled(_)
        ));
        assert!(
            child_raw.raw_session_meta_for_testing().is_some(),
            "the retained child session row survives the cancelled process"
        );
        assert!(
            child_raw
                .raw_pending_turn_inputs_for_testing()
                .iter()
                .all(|row| row.2.kind().is_terminal() && row.3.is_none()),
            "the settled child leaves only terminal, unclaimed input receipts"
        );
        let child_store = factory
            .open_existing_store_by_id(&child_session_id)
            .await
            .expect("inspect retained child")
            .expect("retained child store stays openable");
        assert!(
            crate::store::TurnInputStore::list_pending_turn_inputs(
                child_store.as_ref(),
                &child_session_id,
            )
            .await
            .expect("list retained child inputs")
            .is_empty(),
            "no claimable child input remains once the process may terminalize"
        );
    }

    /// Crash after the child accepted the turn input, then redelivery with the
    /// process already durably cancelled: the early-cancelled path reconciles
    /// the existing child instead of returning blind — the open input is
    /// settled before the cancelled outcome is produced.
    #[tokio::test]
    async fn crash_after_acceptance_redelivery_settles_retained_child_input() {
        let fixture = Box::pin(parked_session_turn("crash-redelivery")).await;
        let ParkedSessionTurn {
            services,
            factory,
            process_id,
            child_session_id,
            create_request,
            registration,
            mut started,
            ..
        } = fixture;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let mut run = Box::pin(services.run_process_session_turn(
            registration.clone(),
            create_request.clone(),
            crate::TurnInput::text("park the child turn"),
            native_process_scope(&process_id),
            cancellation.clone(),
        ));
        tokio::select! {
            started = started.recv() => assert_eq!(started, Some(())),
            outcome = run.as_mut() => panic!("child turn completed before the crash: {outcome:?}"),
        }
        // The attempt dies mid-turn with the input accepted; the durable
        // cancellation lands afterwards. The redelivery must reconcile the
        // retained child's open input before producing the cancelled outcome.
        drop(run);
        let child_raw = factory
            .raw_store_for_testing(&child_session_id)
            .expect("child durable store exists");
        // The dead attempt's input claim stays live while the crashed
        // session-execution-lease generation does; reconcile refuses to
        // settle over it until the lease expires.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let replay_cancellation = tokio_util::sync::CancellationToken::new();
        replay_cancellation.cancel();
        let replay = services
            .run_process_session_turn(
                registration,
                create_request,
                crate::TurnInput::text("park the child turn"),
                native_process_scope(&process_id),
                replay_cancellation,
            )
            .await
            .expect("settled reconcile returns the cancelled outcome");
        assert!(matches!(
            replay.into_tool_output().outcome,
            crate::ToolCallOutcome::Cancelled(_)
        ));
        assert!(
            child_raw.raw_session_meta_for_testing().is_some(),
            "the retained child session row survives the cancelled process"
        );
        assert!(
            child_raw
                .raw_pending_turn_inputs_for_testing()
                .iter()
                .all(|row| row.2.kind().is_terminal() && row.3.is_none()),
            "the reconciled child leaves only terminal, unclaimed input receipts"
        );
        let child_store = factory
            .open_existing_store_by_id(&child_session_id)
            .await
            .expect("inspect retained child")
            .expect("retained child store stays openable");
        assert!(
            crate::store::TurnInputStore::list_pending_turn_inputs(
                child_store.as_ref(),
                &child_session_id,
            )
            .await
            .expect("list retained child inputs")
            .is_empty(),
            "no claimable child input remains once the process may terminalize"
        );
    }

    /// A panicking child turn is typed at the run boundary: the spawned child
    /// task's panic surfaces as `child_turn_panicked`, the process stays
    /// recoverable, and the parent runtime keeps running turns.
    #[tokio::test]
    async fn child_turn_panic_is_typed_and_the_parent_remains_alive() {
        let previous = crate::panic_containment::set_loud(false);
        let panic_once = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let panic_plugin: Arc<dyn crate::PluginFactory> =
            Arc::new(crate::plugin::StaticPluginFactory::new(
                "child-panic-test",
                crate::PluginSpec::new().with_prompt_contributor(Arc::new(move |_context| {
                    let panic_once = Arc::clone(&panic_once);
                    Box::pin(async move {
                        if panic_once.swap(false, std::sync::atomic::Ordering::SeqCst) {
                            panic!("child turn payload only");
                        }
                        Ok(Vec::new())
                    })
                })),
            ));
        let factory = crate::InMemorySessionStoreFactory::new();
        let host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        ))
        .with_session_store_factory(Arc::new(factory.clone()));
        let transport = mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(crate::LlmResponse {
                parts: vec![crate::LlmOutputPart::Text {
                    text: "parent still alive".to_string(),
                    response_meta: None,
                }],
                ..Default::default()
            }),
        }]);
        let mut runtime = runtime_with_plugins_and_tools_and_host(
            vec![panic_plugin],
            Arc::new(EmptyTools),
            transport,
            host,
        )
        .await;
        let services = runtime
            .runtime_session_services()
            .expect("runtime session services");
        let plugin_init = runtime
            .session_state_service()
            .expect("session state")
            .session_plugin_init(&SessionId::from(runtime.session_id()))
            .await
            .expect("plugin init");
        let child_session_id = SessionId::from("panicking-child");
        let process_id = ProcessId::from("process:subagent:panicking-child");
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
                turn_input: Box::new(crate::TurnInput::text("panic")),
                output_contract: crate::ToolOutputContract::Static,
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        );
        let outcome = services
            .run_process_session_turn(
                registration,
                create_request,
                crate::TurnInput::text("panic"),
                native_process_scope(&process_id),
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        crate::panic_containment::set_loud(previous);
        let err = outcome.expect_err("the panicking child turn must surface as a failure");
        assert!(
            err.to_string()
                .contains("child_turn_panicked: child turn payload only"),
            "got {err}"
        );

        let parent = runtime
            .run_turn_assembled(
                crate::TurnInput::text("continue parent"),
                tokio_util::sync::CancellationToken::new(),
                named_turn_scope(
                    &SessionId::from(runtime.session_id()),
                    &crate::TurnId::from("parent-after-child-panic"),
                ),
            )
            .await
            .expect("parent survives child panic");
        assert_eq!(parent.assistant_output.safe_text, "parent still alive");
    }

    /// FIG-3424 run-scoped residency: the process run owns the child runtime
    /// for the run's duration only. Once `run_process_session_turn` returns —
    /// here on the success path — the `Weak` captured at initialisation no
    /// longer upgrades; the durable row remains and reopens through the
    /// ordinary store open.
    #[tokio::test]
    async fn spawned_child_runtime_does_not_outlive_the_process_run() {
        let child_session_id = SessionId::from("run-scoped-child");
        let process_id = ProcessId::from("process:subagent:run-scoped-child");
        let factory = crate::InMemorySessionStoreFactory::new();
        let host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        ))
        .with_session_store_factory(Arc::new(factory.clone()));
        let transport = mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(crate::LlmResponse {
                parts: vec![crate::LlmOutputPart::Text {
                    text: "child answered".to_string(),
                    response_meta: None,
                }],
                ..Default::default()
            }),
        }]);
        let runtime = runtime_with_plugins_and_tools_and_host(
            Vec::new(),
            Arc::new(EmptyTools),
            transport,
            host,
        )
        .await;
        let services = runtime
            .runtime_session_services()
            .expect("runtime session services");
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
                turn_input: Box::new(crate::TurnInput::text("run")),
                output_contract: crate::ToolOutputContract::Static,
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        );
        let _ = crate::runtime::session_manager::take_spawned_child_runtimes();
        let output = services
            .run_process_session_turn(
                registration,
                create_request,
                crate::TurnInput::text("run"),
                native_process_scope(&process_id),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("child session turn completes");
        assert!(matches!(
            output.into_tool_output().outcome,
            crate::ToolCallOutcome::Success(_)
        ));

        let spawned: Vec<_> = crate::runtime::session_manager::take_spawned_child_runtimes()
            .into_iter()
            .filter(|(session_id, _)| *session_id == child_session_id)
            .collect();
        assert_eq!(spawned.len(), 1, "the run minted exactly one child runtime");
        assert!(
            spawned.iter().all(|(_, weak)| weak.upgrade().is_none()),
            "the child runtime must be dropped when the process run ends"
        );
        assert!(
            factory
                .open_existing_store_by_id(&child_session_id)
                .await
                .expect("inspect child store")
                .is_some(),
            "the durable child row remains and reopens through the ordinary open"
        );
    }

    /// FIG-3424 crash point: a redelivery in a new run after the create commit
    /// but before turn admission reopens the durable child through the
    /// ordinary path — it never trips the "session already exists" create
    /// refusal, and the turn runs on the reopened runtime.
    #[tokio::test]
    async fn redelivery_after_create_commit_reopens_child_and_runs_turn() {
        let child_session_id = SessionId::from("redelivered-child");
        let process_id = ProcessId::from("process:subagent:redelivered-child");
        let factory = crate::InMemorySessionStoreFactory::new();
        let host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        ))
        .with_session_store_factory(Arc::new(factory.clone()));
        let transport = mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(crate::LlmResponse {
                parts: vec![crate::LlmOutputPart::Text {
                    text: "redelivered turn answered".to_string(),
                    response_meta: None,
                }],
                ..Default::default()
            }),
        }]);
        let runtime = runtime_with_plugins_and_tools_and_host(
            Vec::new(),
            Arc::new(EmptyTools),
            transport,
            host,
        )
        .await;
        let services = runtime
            .runtime_session_services()
            .expect("runtime session services");
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

        // Leave the durable state a crashed first attempt would: the session
        // row is committed under the process-stamped relation the runner
        // records, and no turn input was ever accepted.
        runtime
            .session_lifecycle_service()
            .expect("session lifecycle")
            .create_session(
                create_request
                    .clone()
                    .with_caused_by(crate::CausalRef::Process {
                        process_id: process_id.clone(),
                    }),
            )
            .await
            .expect("durable child row, as a crashed attempt left it");

        let registration = crate::ProcessRegistration::new(
            &process_id,
            crate::ProcessInput::SessionTurn {
                definition_key: "lash-subagent-session-turn:v1".to_string(),
                create_request: Box::new(create_request.clone()),
                turn_input: Box::new(crate::TurnInput::text("run on redelivery")),
                output_contract: crate::ToolOutputContract::Static,
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        );
        let output = services
            .run_process_session_turn(
                registration,
                create_request,
                crate::TurnInput::text("run on redelivery"),
                native_process_scope(&process_id),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("redelivery reopens the committed child instead of failing create");
        assert!(matches!(
            output.into_tool_output().outcome,
            crate::ToolCallOutcome::Success(_)
        ));
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
                    native_process_scope("permit-process"),
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
