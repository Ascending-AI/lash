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
        execution_write_authority: crate::ProcessExecutionWriteAuthority,
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
        match Box::pin(
            self.initialize_session_and_run_turn(session_init::ProcessSessionTurnInit {
                create_request,
                process_id: &registration.id,
                turn_id: child_turn_id,
                turn_input,
                execution_write_authority: &execution_write_authority,
                scoped_effect_controller,
                cancellation,
            }),
        )
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
                    // A recorded request this deployment cannot initialize
                    // (a predecessor `snapshot` start kept only for decode,
                    // or a catalog that cannot resolve the recorded session
                    // by id) fails deterministically: no attempt can run it,
                    // so the refusal is a terminal failure, not a
                    // recoverable infrastructure error the substrate would
                    // retry forever.
                    session_init::SessionTurnInitError::Refused { source, .. } => {
                        Ok(crate::ProcessAwaitOutput::from_tool_output(
                            crate::ToolCallOutput::failure(crate::ToolFailure::tool(
                                crate::ToolFailureClass::Execution,
                                "process_session_turn_refused",
                                source.to_string(),
                            )),
                        ))
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
            raw.insert("code".to_string(), code.namespaced().into());
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
mod tests;
