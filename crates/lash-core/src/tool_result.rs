/// What a pending tool call does when its `deadline` elapses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutBehavior {
    /// Resolve the call as a timeout failure the model can observe and react to.
    ErrorAsResult,
    /// Fail the whole turn instead of feeding a timeout result back to the model.
    FailTurn,
}

/// What a pending tool call signals about its out-of-band work when cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelHint {
    /// Leave the external work running; cancellation only drops the wait.
    Ignore,
    /// Request that the external work be cancelled along with the wait.
    CancelExternalWork,
}

/// One process event a deferring tool declares, which the runtime appends when
/// the call actually parks.
///
/// A recorded attempt body cannot append process events itself — its
/// [`AttemptContext`](crate::AttemptContext) has no route to them. A tool that
/// must announce its durable wait (the await key an external resolver will
/// deliver against) therefore *declares* the announcement on its
/// [`PendingCompletion`] and the runtime performs the append at park time. The
/// event exists if and only if the park happened: there is no point at which
/// the announcement is durable and the wait is merely hoped for.
///
/// The replay key is required rather than optional. The announcement is
/// re-declared on every redrive of the attempt, so the key is what makes the
/// append idempotent within the process.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PendingAnnouncement {
    /// Process event type to append, e.g. `process.yield`.
    pub event_type: String,
    /// Event payload. It usually names the await key the resolver will use.
    pub payload: serde_json::Value,
    /// Replay key making the append idempotent under redrive.
    pub replay_key: String,
}

impl PendingAnnouncement {
    /// Declares one replay-keyed process event for the runtime to append when
    /// the deferring call parks.
    pub fn new(
        event_type: impl Into<String>,
        payload: serde_json::Value,
        replay_key: impl Into<String>,
    ) -> Self {
        Self {
            event_type: event_type.into(),
            payload,
            replay_key: replay_key.into(),
        }
    }

    pub(crate) fn into_append_request(self) -> crate::ProcessEventAppendRequest {
        // An announcement is progress metadata about a wait that has not
        // settled, not a wake. The session it would reach is the session parked
        // on the announced call, and re-prompting that turn against its own
        // unsettled wait is not something a tool should be able to cause by
        // describing its park. The event is still appended and still visible to
        // observers; only the wake is withheld.
        crate::ProcessEventAppendRequest::new(self.event_type, self.payload)
            .with_replay_key(self.replay_key)
            .without_wake()
    }
}

/// Who delivers the outcome of a parked call.
///
/// A plain [`ToolOutcome::Pending`] names nobody: an out-of-band actor holds
/// the completion key and resolves it whenever it likes (the human-approval
/// shape). A call that parks on a *runtime-owned* fact names that fact here
/// instead, and the runtime arms the resolver itself — at the park and again on
/// every redrive of the parked turn, because a recorded attempt body does not
/// re-run when the turn is re-driven and an armed watcher does not survive a
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
    /// The incarnation rides inside the [`ProcessRef`](crate::ProcessRef): a
    /// wait armed against one incarnation is never resolved by a later one's
    /// terminal.
    ProcessTerminal { process_ref: crate::ProcessRef },
}

/// Configuration carried by a [`ToolOutcome::Pending`] result: how long the runtime
/// waits for the deferred outcome, what to do if it times out or is cancelled, and
/// any process event the runtime announces when the call parks.
///
/// Defaults to no deadline, [`TimeoutBehavior::ErrorAsResult`],
/// [`CancelHint::CancelExternalWork`], and no announcement. Build one with
/// [`PendingCompletion::new`] and the `with_*` adjusters.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PendingCompletion {
    /// Maximum time to wait for the deferred outcome. `None` waits indefinitely (until
    /// the turn or process is otherwise cancelled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<std::time::Duration>,
    /// What the runtime does when `deadline` elapses without a resolution.
    pub on_timeout: TimeoutBehavior,
    /// What the runtime signals about out-of-band work if the call is cancelled.
    pub on_cancel: CancelHint,
    /// Process event the runtime appends when this call parks, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub announcement: Option<PendingAnnouncement>,
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
            deadline: None,
            on_timeout: TimeoutBehavior::ErrorAsResult,
            on_cancel: CancelHint::CancelExternalWork,
            announcement: None,
            resolved_by: None,
        }
    }
}

impl PendingCompletion {
    /// Constructs deferred-completion policy for tool implementors with no deadline,
    /// error-as-result timeout handling, and external-work cancellation enabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the maximum durable wait for tool implementors; expiry follows the configured timeout
    /// behavior rather than completing the tool successfully.
    pub fn with_deadline(mut self, deadline: std::time::Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Selects turn failure on deadline expiry for tool implementors instead of returning a timeout
    /// result to the model.
    pub fn fail_turn_on_timeout(mut self) -> Self {
        self.on_timeout = TimeoutBehavior::FailTurn;
        self
    }

    /// Declares the process event the runtime appends when this call parks.
    ///
    /// Use it to announce the durable wait — typically the await key an
    /// external resolver delivers against — from a recorded attempt that
    /// cannot append process events itself. The runtime performs the append
    /// after it has taken the completion key and before the call is parked, so
    /// the announcement and the wait land together or not at all.
    pub fn announcing(mut self, announcement: PendingAnnouncement) -> Self {
        self.announcement = Some(announcement);
        self
    }

    /// Declares the runtime-owned fact that resolves this wait, making the
    /// runtime — not the tool — responsible for arming the resolver.
    ///
    /// Use it when the outcome is a fact the runtime can already observe
    /// durably (a process terminal), rather than one an external actor
    /// delivers. The tool still takes its completion key first: the key is what
    /// the armed resolver resolves.
    pub fn resolved_by(mut self, resolver: PendingResolver) -> Self {
        self.resolved_by = Some(resolver);
        self
    }

    /// Declares that the terminal of `process_ref` resolves this wait.
    pub fn resolved_by_process_terminal(self, process_ref: crate::ProcessRef) -> Self {
        self.resolved_by(PendingResolver::ProcessTerminal { process_ref })
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
/// Before returning [`ToolOutcome::Pending`], a tool **must** first obtain a completion
/// key by calling [`ToolContext::completion_key`](crate::ToolContext::completion_key)
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
    /// [`ToolContext::completion_key`](crate::ToolContext::completion_key) it took
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

    /// Constructs an error-as-result JSON outcome for protocol and process-engine implementors
    /// returning from an authorized tool call.
    pub fn err(result: serde_json::Value) -> Self {
        let message = result
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| result.to_string());
        Self::from_output(crate::ToolCallOutput::failure(crate::ToolFailure {
            class: crate::ToolFailureClass::Execution,
            code: "tool_error".to_string(),
            message,
            source: crate::ToolFailureSource::Tool,
            retry: crate::ToolRetryStatus::Never,
            raw: Some(crate::ToolValue::untrusted_json(result)),
        }))
    }

    /// Formats an error-as-result outcome for protocol and process-engine implementors returning
    /// from an authorized tool call.
    pub fn err_fmt(msg: impl std::fmt::Display) -> Self {
        Self::err(serde_json::json!(msg.to_string()))
    }

    /// Constructs a structured failure for protocol and process-engine implementors returning from
    /// an authorized tool call.
    pub fn failure(failure: crate::ToolFailure) -> Self {
        Self::from_output(crate::ToolCallOutput::failure(failure))
    }

    /// Constructs a retryable structured failure, including an optional backoff hint, for protocol
    /// and process-engine implementors returning from an authorized tool call.
    pub fn retryable_failure(
        class: crate::ToolFailureClass,
        code: impl Into<String>,
        message: impl Into<String>,
        after_ms: Option<u64>,
    ) -> Self {
        Self::failure(crate::ToolFailure::safe_retry(
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
/// ([`ProcessAwaitOutput::into_tool_output`](crate::ProcessAwaitOutput::into_tool_output))
/// is what turns that fact into the call's outcome, so the parked path calls
/// exactly it. Skipping this and letting the envelope through would hand the
/// cell a `{"type":"settled","output":{...}}` record — and with it the internal
/// `$lash_tool_value` tag — for a wait that inline answers the child's own
/// value. The language forces the swap between the two spellings inside a batch
/// (`crates/lashlang/src/runtime/vm/pending_tools.rs`, `PROCESS_HANDLE_LEAF`),
/// so the swap has to be value-preserving.
pub(crate) fn tool_output_from_completion_resolution(
    resolution: crate::Resolution,
    resolver: Option<&crate::PendingResolver>,
) -> crate::ToolCallOutput {
    if let (Some(crate::PendingResolver::ProcessTerminal { .. }), crate::Resolution::Ok(value)) =
        (resolver, &resolution)
        && let Ok(terminal) = serde_json::from_value::<crate::ProcessAwaitOutput>(value.clone())
    {
        return terminal.into_tool_output();
    }
    match resolution {
        crate::Resolution::Ok(value) => crate::ToolCallOutput::success(value),
        crate::Resolution::Err(err) => {
            let mut failure =
                crate::ToolFailure::tool(crate::ToolFailureClass::Execution, err.code, err.message);
            failure.raw = err.raw.map(crate::ToolValue::untrusted_json);
            crate::ToolCallOutput::failure(failure)
        }
        crate::Resolution::Timeout => crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::Timeout,
            "tool_completion_timeout",
            "pending tool completion timed out",
        )),
        crate::Resolution::Cancelled => crate::ToolCallOutput::cancelled(
            crate::ToolCancellation::runtime("pending tool completion cancelled"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde::ser::{Error as _, Serializer};

    use super::*;

    #[test]
    fn tool_result_from_result_serializes_success_values() {
        let result: ToolOutcome = Result::<_, std::io::Error>::Ok(vec!["alpha", "beta"]).into();
        assert!(result.is_success());
        assert_eq!(
            result.value_for_projection(),
            serde_json::json!(["alpha", "beta"])
        );
    }

    #[test]
    fn tool_result_from_result_formats_errors() {
        let result: ToolOutcome =
            Result::<serde_json::Value, _>::Err(std::io::Error::other("nope")).into();
        assert!(!result.is_success());
        assert_eq!(result.value_for_projection(), serde_json::json!("nope"));
        assert_eq!(
            result.as_output().value_for_projection()["message"],
            serde_json::json!("nope")
        );
    }

    #[test]
    fn tool_result_from_result_reports_serialize_failures() {
        struct BrokenValue;

        impl serde::Serialize for BrokenValue {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                Err(S::Error::custom("boom"))
            }
        }

        let result: ToolOutcome = Result::<BrokenValue, std::io::Error>::Ok(BrokenValue).into();
        assert!(!result.is_success());
        assert_eq!(
            result.value_for_projection(),
            serde_json::json!("Failed to serialize tool result: boom")
        );
    }

    #[test]
    fn pending_result_is_not_completed_output() {
        let result = ToolOutcome::pending(PendingCompletion::new());
        assert!(result.is_pending());
        assert!(result.as_done_output().is_none());
        assert!(result.into_done_output().is_err());
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
            process_ref: crate::ProcessRef::new(
                crate::ProcessId::from("child-process"),
                crate::ProcessIncarnation::from_registration_sequence(1),
            ),
        }
    }

    /// The durable wait carries the terminal itself; this is the payload
    /// `process_terminal_resolution` journals.
    fn terminal_resolution(terminal: &crate::ProcessAwaitOutput) -> crate::Resolution {
        crate::Resolution::Ok(
            serde_json::to_value(terminal).expect("a process terminal serializes"),
        )
    }

    #[test]
    fn a_settled_process_terminal_answers_the_childs_own_value_not_the_envelope() {
        let terminal = crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
            serde_json::json!({ "lookup": "lookup:left" }),
        ));
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
        let terminal =
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(failure));
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
        let terminal =
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
                crate::ToolCancellation::runtime("child cancelled by its parent"),
            ));
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

    #[test]
    fn a_wait_with_no_named_resolver_is_untouched() {
        let resolution = crate::Resolution::Ok(serde_json::json!({ "done": true }));
        assert_eq!(
            tool_output_from_completion_resolution(resolution, None).value_for_projection(),
            serde_json::json!({ "done": true })
        );
    }
}
