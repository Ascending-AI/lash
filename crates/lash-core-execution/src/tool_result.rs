use lash_execution_contracts::DeclaredStart;

/// What a pending tool call signals about its out-of-band work when cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelHint {
    /// Leave the external work running; cancellation only drops the wait.
    Ignore,
    /// Request that the external work be cancelled along with the wait.
    CancelExternalWork,
}

/// Who delivers the outcome of a parked call.
///
/// A plain [`ToolOutcome::Pending`] names nobody: an out-of-band actor holds
/// the completion key and resolves it whenever it likes (the human-approval
/// shape). A call that parks on a *runtime-owned* fact names that fact here
/// instead, and the runtime arms the resolver itself — at the park and again on
/// every redrive of the parked turn, because a recorded attempt body does not
/// re-run when the turn is redriven and an armed watcher does not survive a
/// crash.
///
/// The declaration is journaled with the pending launch, so the arming is
/// replay-deterministic: the same resolver is re-derived from the same
/// journaled bytes rather than re-discovered by re-executing the tool.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PendingResolver {
    /// The terminal of a durable process resolves this wait.
    ///
    /// A [`ProcessId`](crate::ProcessId) is minted and never reused, so a
    /// wait armed against one process is never resolved by another's
    /// terminal.
    ProcessTerminal { process_id: crate::ProcessId },
    /// The terminal of the one process this call declares resolves the wait.
    ///
    /// The body cannot name that process: its id is minted only when the
    /// start registers. The runtime launches the declared start at the park,
    /// records the realized id as the call's launch receipt, and arms the
    /// terminal of that id, on the park and on every redrive.
    DeclaredStart(DeclaredStart),
}

impl PendingResolver {
    /// Whether a process terminal resolves this wait. The runtime owns that
    /// process's wait, so a cancelled or timed-out wait may cancel it (see
    /// [`CancelHint`]), and the resolution is the terminal itself.
    pub fn awaits_process_terminal(&self) -> bool {
        match self {
            Self::ProcessTerminal { .. } | Self::DeclaredStart(_) => true,
        }
    }
}

/// Configuration carried by a [`ToolOutcome::Pending`] result: what to do if
/// it is cancelled, and what resolves it.
///
/// Defaults to [`CancelHint::CancelExternalWork`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PendingCompletion {
    /// What the runtime signals about out-of-band work if the call is cancelled.
    pub on_cancel: CancelHint,
    /// Runtime-owned fact that resolves this wait, if the call named one.
    ///
    /// `None` is the out-of-band shape: something outside the runtime holds the
    /// completion key. `Some` makes the runtime responsible for arming the
    /// resolver on the park and on every redrive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<PendingResolver>,
}

impl Default for PendingCompletion {
    fn default() -> Self {
        Self {
            on_cancel: CancelHint::CancelExternalWork,
            resolved_by: None,
        }
    }
}

impl PendingCompletion {
    /// Constructs deferred-completion policy with external-work cancellation enabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use it when the outcome is a fact the runtime can already observe
    /// durably (a process terminal), rather than one an external actor
    /// delivers. The tool still takes its completion key first: the key is what
    /// the armed resolver resolves.
    pub fn resolved_by(mut self, resolver: PendingResolver) -> Self {
        self.resolved_by = Some(resolver);
        self
    }

    pub fn resolved_by_process_terminal(self, process_id: crate::ProcessId) -> Self {
        self.resolved_by(PendingResolver::ProcessTerminal { process_id })
    }

    /// Use it when the call's outcome is the terminal of a child it declares.
    /// The runtime launches the start at the park and arms its terminal; a
    /// cancelled wait cancels the child under
    /// [`CancelHint::CancelExternalWork`].
    pub fn resolved_by_declared_start(self, start: DeclaredStart) -> Self {
        self.resolved_by(PendingResolver::DeclaredStart(start))
    }
}

/// The outcome a [`ToolProvider::execute`](crate::ToolProvider::execute) returns
/// for a single call.
///
/// The variant a tool returns chooses its completion mode:
///
/// - [`ToolOutcome::Done`] — **active await**. The result is available inline and the
///   runtime finalizes the call immediately. Construct it with [`ToolOutcome::ok`],
///   [`ToolOutcome::err`], [`ToolOutcome::failure`], and friends.
/// - [`ToolOutcome::Pending`] — **deferred / callback completion**. The tool has
///   launched out-of-band work (a webhook, a human approval, another service) and the
///   real outcome is delivered later against a completion key.
///
/// # The completion-key contract
///
/// A tool that returns [`ToolOutcome::Pending`] declares `may_defer` on its
/// manifest's [`ToolDeclaration`](crate::ToolDeclaration); an undeclared tool
/// that parks fails the call with `tool_outcome_not_declared`. Before
/// returning `Pending`, it **must** first obtain a completion key by calling
/// [`AttemptContext::completion_key`](crate::AttemptContext::completion_key)
/// (reachable through `call.context`). That key names the durable wait the runtime parks
/// the call on, and is what an external resolver uses to deliver the outcome. Returning
/// `Pending` *without* having taken a completion key fails the call with the internal
/// error `pending_tool_missing_completion_key`.
///
/// ```ignore
/// async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
///     // Take the key first, then hand it to whatever completes the work out-of-band.
///     let key = match call.context.completion_key() {
///         Ok(key) => key,
///         Err(err) => return ToolOutcome::err_fmt(err),
///     };
///     enqueue_external_work(key);
///     ToolOutcome::pending(PendingCompletion::new())
/// }
/// ```
#[derive(Clone, Debug, PartialEq)]
pub enum ToolOutcome {
    /// Active await: the tool finished inline; this is its final output.
    Done(Box<crate::ToolCallOutput>),
    /// Deferred completion: the tool parked on a durable wait keyed by the
    /// [`AttemptContext::completion_key`](crate::AttemptContext::completion_key) it took
    /// before returning. The outcome arrives later through the resolve seam and is
    /// shaped by the carried [`PendingCompletion`].
    ///
    /// Boxed so that the outcome every tool attempt returns — and that several
    /// seams carry as a `Result` error arm — stays a pointer wide, rather than
    /// growing to the full parked-completion shape at every call site.
    Pending(Box<PendingCompletion>),
}

impl ToolOutcome {
    /// Builds a `ToolOutcome` from output data for protocol and process-engine implementors while
    /// preparing or executing an authorized tool call.
    pub fn from_output(output: crate::ToolCallOutput) -> Self {
        Self::Done(Box::new(output))
    }

    /// Constructs the deferred outcome that protocol and process-engine implementors return when an
    /// authorized tool call will finish out of band.
    pub fn pending(pending: PendingCompletion) -> Self {
        Self::Pending(Box::new(pending))
    }

    /// Constructs a successful JSON outcome for protocol and process-engine implementors returning
    /// from an authorized tool call.
    pub fn ok(result: serde_json::Value) -> Self {
        Self::from_output(crate::ToolCallOutput::success(result))
    }

    /// Constructs a tool execution failure with code `tool_error` and a plain message.
    ///
    /// Read its plain message with [`crate::ToolFailure::model_text`], never by
    /// stringifying [`Self::value_for_projection`].
    /// For a structured refusal, use
    /// `ToolOutcome::failure(ToolFailure::new(code, message).with_raw(data))`.
    pub fn err(message: impl Into<String>) -> Self {
        Self::failure(crate::ToolFailure::new("tool_error", message))
    }

    /// Formats an error-as-result outcome for protocol and process-engine implementors returning
    /// from an authorized tool call.
    pub fn err_fmt(msg: impl std::fmt::Display) -> Self {
        Self::err(msg.to_string())
    }

    /// Constructs a structured failure for protocol and process-engine implementors returning from
    /// an authorized tool call.
    pub fn failure(failure: crate::ToolFailure) -> Self {
        Self::from_output(crate::ToolCallOutput::failure(failure))
    }

    /// Constructs a structured failure with an optional delay suggestion.
    /// The call's admitted execution policy controls whether it repeats.
    pub fn failure_with_delay(
        class: crate::ToolFailureClass,
        code: impl Into<String>,
        message: impl Into<String>,
        after_ms: Option<u64>,
    ) -> Self {
        Self::failure(crate::ToolFailure::with_suggested_delay(
            class, code, message, after_ms,
        ))
    }

    /// Constructs a cancellation outcome for protocol and process-engine implementors whose
    /// authorized tool call did not complete.
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::from_output(crate::ToolCallOutput::cancelled(
            crate::ToolCancellation::runtime(message),
        ))
    }

    /// Constructs a cancellation outcome that retains provider evidence for protocol and
    /// process-engine implementors whose tool call did not complete.
    pub fn cancelled_with_raw(message: impl Into<String>, raw: serde_json::Value) -> Self {
        let mut cancellation = crate::ToolCancellation::runtime(message);
        cancellation.raw = Some(crate::ToolValue::untrusted_json(raw));
        Self::from_output(crate::ToolCallOutput::cancelled(cancellation))
    }

    /// A control call's result: the declared turn control alone, with no output.
    pub fn turn_control(control: crate::TurnControl) -> Self {
        Self::from_output(crate::ToolCallOutput::turn_control(control))
    }

    /// Ends the caller's turn with `value`. The tool must declare the Finish control.
    pub fn finish(value: impl Into<crate::ToolValue>) -> Self {
        Self::from_output(crate::ToolCallOutput::finish(value))
    }

    /// Switches the caller to a fresh agent frame whose follow-on runs under
    /// `next_run` ([`AttemptContext::run_spec`](crate::AttemptContext::run_spec)
    /// to keep the running turn's shape). The tool must declare the
    /// SwitchAgentFrame control.
    pub fn switch_agent_frame(
        frame_key: crate::FrameKey,
        task: impl Into<String>,
        initial_nodes: Vec<crate::SessionAppendNode>,
        next_run: crate::NextRun,
    ) -> Self {
        Self::from_output(crate::ToolCallOutput::switch_agent_frame(
            frame_key,
            task,
            initial_nodes,
            next_run,
        ))
    }

    /// Sets the control carried by a `ToolOutcome` for protocol and process-engine implementors
    /// while preparing or executing an authorized tool call.
    pub fn with_control(mut self, control: crate::ToolControl) -> Self {
        if let Self::Done(output) = &mut self {
            output.as_mut().control = Some(control);
        }
        self
    }

    /// Lets protocol and process-engine implementors distinguish successful completed tool output
    /// from failures and deferred completion.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Done(output) if output.is_success())
    }

    /// Lets protocol and process-engine implementors detect a deferred tool outcome that must be
    /// resolved through the durable wait contract.
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }

    /// Projects a stable JSON value for public or persisted protocol output.
    ///
    /// # Stable JSON projection contract
    ///
    /// **Every stable or public JSON projection MUST use this method.**
    /// [`crate::ToolValue`]'s `Serialize` implementation includes its trust
    /// envelope by design. Serializing the completed output directly leaks the
    /// internal `$lash_tool_value: "untrusted_json"` discriminant and `value`
    /// wrapper into the public shape. This method removes that envelope while
    /// preserving typed attachment projection.
    ///
    /// A failure with `raw` projects that evidence; otherwise it projects the
    /// typed failure object, including `class`, `code`, `message` and `source`.
    /// For the model's plain failure text use [`crate::ToolFailure::model_text`],
    /// never stringify this projection. For complete model content use
    /// [`crate::ModelToolReturn::from_output`] on [`Self::as_output`].
    #[expect(
        clippy::expect_used,
        reason = "the doc comment above states the contract: calling this on a pending result is a caller error"
    )]
    pub fn value_for_projection(&self) -> serde_json::Value {
        match &self
            .as_done_output()
            .expect("pending tool result has no projection value")
            .outcome
        {
            crate::ToolCallOutcome::Success(value) => tool_value_for_projection(value),
            crate::ToolCallOutcome::Failure(failure) => failure
                .raw
                .as_ref()
                .map(tool_value_for_projection)
                .unwrap_or_else(|| failure.to_json_value()),
            crate::ToolCallOutcome::Cancelled(cancellation) => cancellation
                .raw
                .as_ref()
                .map(tool_value_for_projection)
                .unwrap_or_else(|| cancellation.to_json_value()),
        }
    }

    /// Borrows completed tool output for protocol and process-engine implementors, returning `None`
    /// for deferred completion.
    pub fn as_done_output(&self) -> Option<&crate::ToolCallOutput> {
        match self {
            Self::Done(output) => Some(output.as_ref()),
            Self::Pending(_) => None,
        }
    }

    /// Borrows the immediate tool output for protocol and process-engine implementors; calling it
    /// on deferred completion is a contract violation and panics.
    #[expect(
        clippy::expect_used,
        reason = "the doc comment above states the contract: calling this on deferred completion is a caller error"
    )]
    pub fn as_output(&self) -> &crate::ToolCallOutput {
        self.as_done_output()
            .expect("pending tool result cannot be viewed as completed output")
    }

    /// Consumes a tool result for protocol and process-engine implementors, returning the
    /// pending-completion configuration instead of output when the call was deferred.
    #[allow(
        clippy::result_large_err,
        reason = "the Err arm is the parked-completion configuration itself; boxing it here would only push the unboxing onto every caller"
    )]
    pub fn into_done_output(self) -> Result<crate::ToolCallOutput, PendingCompletion> {
        match self {
            Self::Done(output) => Ok(*output),
            Self::Pending(pending) => Err(*pending),
        }
    }
}

fn tool_value_for_projection(value: &crate::ToolValue) -> serde_json::Value {
    crate::ToolCallOutput::success_tool_value(value.clone()).value_for_projection()
}

impl<T, E> From<Result<T, E>> for ToolOutcome
where
    T: serde::Serialize,
    E: std::fmt::Display,
{
    fn from(result: Result<T, E>) -> Self {
        match result {
            Ok(value) => match serde_json::to_value(value) {
                Ok(value) => Self::ok(value),
                Err(err) => Self::err_fmt(format_args!("Failed to serialize tool result: {err}")),
            },
            Err(err) => Self::err_fmt(err),
        }
    }
}

/// One wait, one shape: the value a parked call receives is the value the same
/// wait returns inline.
///
/// A wait the runtime armed on a process terminal carries that terminal as its
/// resolution payload, because a terminal is a fact and a failed or cancelled
/// process is not an error *of the wait*. The inline path
/// ([`ProcessAwaitOutput::into_tool_output`](lash_execution_contracts::process::ProcessAwaitOutput::into_tool_output))
/// is what turns that fact into the call's outcome, so the parked path calls
/// exactly it. Skipping this and letting the envelope through would hand the
/// cell a `{"type":"settled","output":{...}}` record — and with it the internal
/// `$lash_tool_value` tag — for a wait that inline answers the child's own
/// value. The language forces the swap between the two spellings inside a batch
/// (the language's batch lowering, `PROCESS_HANDLE_LEAF`),
/// so the swap has to be value-preserving.
pub fn tool_output_from_completion_resolution(
    resolution: crate::Resolution,
    resolver: Option<&crate::PendingResolver>,
) -> crate::ToolCallOutput {
    if let (Some(resolver), crate::Resolution::Ok(value)) = (resolver, &resolution)
        && resolver.awaits_process_terminal()
        && let Ok(terminal) = serde_json::from_value::<
            lash_execution_contracts::process::ProcessAwaitOutput,
        >(value.clone())
    {
        return terminal.into_tool_output();
    }
    match resolution {
        crate::Resolution::Ok(value) => crate::ToolCallOutput::success(value),
        crate::Resolution::Err(err) => {
            let mut failure = crate::ToolFailure::tool(
                crate::ToolFailureClass::Execution,
                err.code.namespaced(),
                err.message,
            );
            failure.raw = err.raw.map(crate::ToolValue::untrusted_json);
            crate::ToolCallOutput::failure(failure)
        }
        crate::Resolution::Cancelled => crate::ToolCallOutput::cancelled(
            crate::ToolCancellation::runtime("pending tool completion cancelled"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_error_exposes_its_plain_model_text_without_the_projection_envelope() {
        let outcome = ToolOutcome::err("x");
        let crate::ToolCallOutcome::Failure(failure) = &outcome.as_output().outcome else {
            panic!("a tool error must be a typed failure");
        };

        assert_eq!(failure.model_text(), "x");
        assert_eq!(
            outcome.as_output().value_for_projection(),
            serde_json::json!({
                "class": "execution",
                "code": "tool_error",
                "message": "x",
                "source": "tool",
            }),
        );
        let failure = failure
            .clone()
            .with_raw(serde_json::json!({ "detail": "provider evidence" }));
        assert_eq!(failure.model_text(), "x");
        let output = crate::ToolCallOutput::failure(failure.clone());
        let returned = crate::ModelToolReturn::from_output("tool".into(), &output);
        assert_eq!(
            returned.outcome,
            lash_protocol_types::ModelToolOutcome::Failure(failure)
        );
        assert!(returned.parts.is_empty());
    }

    // -----------------------------------------------------------------------
    // One wait, one shape (ADR 0095).
    //
    // The language refuses `await handle` inside a batch and repairs it to
    // `processes.await(handle)`; that repair must not change the value. These
    // compare the two paths directly: the inline path is
    // `ProcessAwaitOutput::into_tool_output`, the parked path is a resolution
    // carrying the same terminal through a `ProcessTerminal` resolver.
    // -----------------------------------------------------------------------

    fn awaited_process() -> crate::PendingResolver {
        crate::PendingResolver::ProcessTerminal {
            process_id: crate::process_id_for_test("child-process"),
        }
    }

    /// The durable wait carries the terminal itself; this is the payload
    /// `process_terminal_resolution` journals.
    fn terminal_resolution(
        terminal: &lash_execution_contracts::process::ProcessAwaitOutput,
    ) -> crate::Resolution {
        crate::Resolution::Ok(
            serde_json::to_value(crate::Durable(terminal)).expect("a process terminal serializes"),
        )
    }

    #[test]
    fn a_settled_process_terminal_answers_the_childs_own_value_not_the_envelope() {
        let terminal = lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
            crate::ToolCallOutput::success(serde_json::json!({ "lookup": "lookup:left" })),
        );
        let inline = terminal.clone().into_tool_output();

        let parked = tool_output_from_completion_resolution(
            terminal_resolution(&terminal),
            Some(&awaited_process()),
        );

        assert_eq!(
            parked, inline,
            "the parked wait must answer the inline value"
        );
        assert_eq!(
            parked.value_for_projection(),
            serde_json::json!({ "lookup": "lookup:left" })
        );
        // The precondition the regression is about: without the resolver the
        // same resolution hands the cell the raw envelope, `$lash_tool_value`
        // and all.
        let unconverted =
            tool_output_from_completion_resolution(terminal_resolution(&terminal), None);
        assert_ne!(unconverted, inline);
        assert!(
            unconverted
                .value_for_projection()
                .to_string()
                .contains("$lash_tool_value"),
            "{}",
            unconverted.value_for_projection()
        );
    }

    #[test]
    fn a_failed_process_terminal_answers_the_inline_failure_verbatim() {
        let mut failure = crate::ToolFailure::runtime(
            crate::ToolFailureClass::Execution,
            "process_failed",
            "child exploded",
        );
        failure.raw = Some(crate::ToolValue::untrusted_json(serde_json::json!({
            "detail": "boom"
        })));
        let terminal = lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
            crate::ToolCallOutput::failure(failure),
        );
        let inline = terminal.clone().into_tool_output();

        let parked = tool_output_from_completion_resolution(
            terminal_resolution(&terminal),
            Some(&awaited_process()),
        );

        assert_eq!(parked, inline);
        assert!(!parked.is_success());
    }

    #[test]
    fn a_cancelled_process_terminal_answers_the_inline_cancellation_verbatim() {
        let terminal = lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
            crate::ToolCallOutput::cancelled(crate::ToolCancellation::runtime(
                "child cancelled by its parent",
            )),
        );
        let inline = terminal.clone().into_tool_output();

        let parked = tool_output_from_completion_resolution(
            terminal_resolution(&terminal),
            Some(&awaited_process()),
        );

        assert_eq!(parked, inline);
        assert!(
            matches!(parked.outcome, crate::ToolCallOutcome::Cancelled(_)),
            "a cancelled child is a cancelled await on both paths"
        );
    }

    /// A declared start's resolver is the terminal of the child it launched,
    /// so its resolution unwraps exactly as a named terminal's does (ADR 0116
    /// §3.7): the call answers the child's own value, never the envelope.
    fn declared_start() -> crate::PendingResolver {
        let start = lash_execution_contracts::tool_intent::StartProcessIntent {
            owner: crate::RuntimeOwner::Session(crate::SessionId::from("parent")),
            declaration: lash_execution_contracts::testing::held_engine_declaration(
                serde_json::Value::Null,
                lash_execution_contracts::process::ProcessOriginator::host(),
                lash_execution_contracts::process::Lifetime::Detached,
            ),
        };
        let identity = lash_execution_contracts::tool_intent::derive_tool_intent_identity_under(
            &crate::RuntimeOwner::Session(crate::SessionId::from("parent")),
            "turn-1",
            &crate::ToolCallId::fixture("call-1"),
            0,
            None,
        );
        crate::PendingResolver::DeclaredStart(
            serde_json::from_value(serde_json::json!({
                "start": start,
                "identity": identity,
            }))
            .expect("a declared start decodes"),
        )
    }

    #[test]
    fn a_declared_start_terminal_answers_the_childs_own_value() {
        let terminal = lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
            crate::ToolCallOutput::success(serde_json::json!({ "summary": "done" })),
        );
        let parked = tool_output_from_completion_resolution(
            terminal_resolution(&terminal),
            Some(&declared_start()),
        );
        assert_eq!(parked, terminal.into_tool_output());
        assert_eq!(
            parked.value_for_projection(),
            serde_json::json!({ "summary": "done" })
        );
    }

    /// ADR 0116 §3.1: a replay key cannot attest to different durable identity fields.
    #[test]
    fn a_decoded_declared_start_refuses_changed_fields_with_the_declaring_replay_key() {
        let crate::PendingResolver::DeclaredStart(declared) = declared_start() else {
            unreachable!()
        };
        let declaring = declared.identity().clone();
        let mut scope = declaring.clone();
        scope.execution_scope_id = "turn-2".into();
        let mut call = declaring.clone();
        call.tool_call_id = crate::ToolCallId::fixture("call-2");
        let mut emission = declaring.clone();
        emission.minting_emission_replay_key = Some("tool:call-1:attempt:2".into());
        let mut ordinal = declaring.clone();
        ordinal.intent_index = 1;

        for recorded in [scope, call, emission, ordinal] {
            let mut bytes = serde_json::to_value(&declared).expect("encode declaration");
            bytes["identity"] = serde_json::to_value(&recorded).expect("encode identity");
            let decoded: DeclaredStart = serde_json::from_value(bytes).expect("decode declaration");
            assert_eq!(
                lash_execution_contracts::declared_start::bound_declared_start(&decoded, &declaring),
                Err(
                    lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::DeclaredStartIdentityMismatch {
                        expected: Box::new(declaring.clone()),
                        recorded: Box::new(recorded),
                    }
                ),
            );
        }
    }

    /// A decoded declaration launches only for the call that declared it:
    /// its start names the admitted session and its identity is exactly the
    /// declaring attempt's for index 0, every field of it (ADR 0116 §3.1).
    #[test]
    fn a_decoded_declared_start_is_bound_to_its_declaring_call() {
        let crate::PendingResolver::DeclaredStart(declared) = declared_start() else {
            unreachable!()
        };
        let declaring = declared.identity().clone();
        assert_eq!(
            lash_execution_contracts::declared_start::bound_declared_start(&declared, &declaring),
            Ok(())
        );

        let with_identity = |identity: crate::ToolIntentIdentity| -> DeclaredStart {
            let mut bytes = serde_json::to_value(&declared).expect("encode");
            bytes["identity"] = serde_json::to_value(identity).expect("encode identity");
            serde_json::from_value(bytes).expect("a forged declaration decodes")
        };
        let derive = |session: &str, scope: &str, call: &str, index: u32| {
            lash_execution_contracts::tool_intent::derive_tool_intent_identity_under(
                &crate::RuntimeOwner::Session(crate::SessionId::fixture(session)),
                scope,
                &crate::ToolCallId::fixture(call),
                index,
                None,
            )
        };
        let emission = crate::RuntimeInvocation::effect(
            crate::EffectAddress::new(
                crate::ExecutionScope::turn(crate::SessionId::from("parent"), "turn-1"),
                "tool:call-1:attempt:2",
            )
            .expect("a turn address"),
            crate::RuntimeAttribution::for_session("parent"),
            "tool:call-1:attempt:2",
        );
        let mut forged_key = declaring.clone();
        forged_key.replay_key = derive("parent", "turn-1", "call-2", 0).replay_key;
        let mismatches = [
            (
                "another execution scope",
                derive("parent", "turn-2", "call-1", 0),
            ),
            ("another call", derive("parent", "turn-1", "call-2", 0)),
            ("a nonzero index", derive("parent", "turn-1", "call-1", 1)),
            (
                "another minting emission",
                lash_execution_contracts::tool_intent::derive_tool_intent_identity_under(
                    &crate::RuntimeOwner::Session(crate::SessionId::from("parent")),
                    "turn-1",
                    &crate::ToolCallId::fixture("call-1"),
                    0,
                    Some(&emission),
                ),
            ),
            (
                "another session's identity",
                derive("other", "turn-1", "call-1", 0),
            ),
            ("a replay key its fields do not derive", forged_key),
        ];
        for (what, identity) in mismatches {
            assert_eq!(
                lash_execution_contracts::declared_start::bound_declared_start(&with_identity(identity.clone()), &declaring),
                Err(
                    lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::DeclaredStartIdentityMismatch {
                        expected: Box::new(declaring.clone()),
                        recorded: Box::new(identity),
                    }
                ),
                "{what} is refused"
            );
        }

        let mut bytes = serde_json::to_value(&declared).expect("encode");
        bytes["start"]["owner"] = serde_json::json!({"session": "other"});
        let foreign: DeclaredStart = serde_json::from_value(bytes).expect("decode");
        assert_eq!(
            lash_execution_contracts::declared_start::bound_declared_start(&foreign, &declaring),
            Err(lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::OwnerMismatch {
                expected: crate::RuntimeOwner::Session(crate::SessionId::from("parent")),
                recorded: crate::RuntimeOwner::Session(crate::SessionId::from("other")),
            }),
            "a start naming another session is refused"
        );
    }
}
