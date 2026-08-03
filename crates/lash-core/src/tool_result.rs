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

/// Configuration carried by a [`ToolResult::Pending`] result: how long the runtime
/// waits for the deferred outcome, and what to do if it times out or is cancelled.
///
/// Defaults to no deadline, [`TimeoutBehavior::ErrorAsResult`], and
/// [`CancelHint::CancelExternalWork`]. Build one with [`PendingCompletion::new`] and
/// the `with_*` adjusters.
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
}

impl Default for PendingCompletion {
    fn default() -> Self {
        Self {
            deadline: None,
            on_timeout: TimeoutBehavior::ErrorAsResult,
            on_cancel: CancelHint::CancelExternalWork,
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
}

/// The outcome a [`ToolProvider::execute`](crate::ToolProvider::execute) returns
/// for a single call.
///
/// The variant a tool returns chooses its completion mode:
///
/// - [`ToolResult::Done`] — **active await**. The result is available inline and the
///   runtime finalizes the call immediately. Construct it with [`ToolResult::ok`],
///   [`ToolResult::err`], [`ToolResult::failure`], and friends.
/// - [`ToolResult::Pending`] — **deferred / callback completion**. The tool has
///   launched out-of-band work (a webhook, a human approval, another service) and the
///   real outcome is delivered later against a completion key.
///
/// # The completion-key contract
///
/// Before returning [`ToolResult::Pending`], a tool **must** first obtain a completion
/// key by calling [`ToolContext::completion_key`](crate::ToolContext::completion_key)
/// (reachable through `call.context`). That key names the durable wait the runtime parks
/// the call on, and is what an external resolver uses to deliver the outcome. Returning
/// `Pending` *without* having taken a completion key fails the call with the internal
/// error `pending_tool_missing_completion_key`.
///
/// ```ignore
/// async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
///     // Take the key first, then hand it to whatever completes the work out-of-band.
///     let key = match call.context.completion_key().await {
///         Ok(key) => key,
///         Err(err) => return ToolResult::err_fmt(err),
///     };
///     enqueue_external_work(key);
///     ToolResult::pending(PendingCompletion::new())
/// }
/// ```
#[derive(Clone, Debug, PartialEq)]
pub enum ToolResult {
    /// Active await: the tool finished inline; this is its final output.
    Done(Box<crate::ToolCallOutput>),
    /// Deferred completion: the tool parked on a durable wait keyed by the
    /// [`ToolContext::completion_key`](crate::ToolContext::completion_key) it took
    /// before returning. The outcome arrives later through the resolve seam and is
    /// shaped by the carried [`PendingCompletion`].
    Pending(PendingCompletion),
}

impl ToolResult {
    /// Builds a `ToolResult` from output data for protocol and process-engine implementors while
    /// preparing or executing an authorized tool call.
    pub fn from_output(output: crate::ToolCallOutput) -> Self {
        Self::Done(Box::new(output))
    }

    /// Constructs the deferred outcome that protocol and process-engine implementors return when an
    /// authorized tool call will finish out of band.
    pub fn pending(pending: PendingCompletion) -> Self {
        Self::Pending(pending)
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
            retry: crate::ToolRetryDisposition::Never,
            raw: Some(crate::ToolValue::from(result)),
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
        cancellation.raw = Some(crate::ToolValue::from(raw));
        Self::from_output(crate::ToolCallOutput::cancelled(cancellation))
    }

    /// Sets the control carried by a `ToolResult` for protocol and process-engine implementors
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

    /// Projects a stable JSON value for protocol and process-engine implementors that expose tool
    /// outcomes to later protocol stages.
    pub fn value_for_projection(&self) -> serde_json::Value {
        match &self
            .as_done_output()
            .expect("pending tool result has no projection value")
            .outcome
        {
            crate::ToolCallOutcome::Success(value) => value.to_json_value(),
            crate::ToolCallOutcome::Failure(failure) => failure
                .raw
                .as_ref()
                .map(crate::ToolValue::to_json_value)
                .unwrap_or_else(|| failure.to_json_value()),
            crate::ToolCallOutcome::Cancelled(cancellation) => cancellation
                .raw
                .as_ref()
                .map(crate::ToolValue::to_json_value)
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
    pub fn as_output(&self) -> &crate::ToolCallOutput {
        self.as_done_output()
            .expect("pending tool result cannot be viewed as completed output")
    }

    /// Consumes a tool result for protocol and process-engine implementors, returning the
    /// pending-completion configuration instead of output when the call was deferred.
    pub fn into_done_output(self) -> Result<crate::ToolCallOutput, PendingCompletion> {
        match self {
            Self::Done(output) => Ok(*output),
            Self::Pending(pending) => Err(pending),
        }
    }
}

impl<T, E> From<Result<T, E>> for ToolResult
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

pub(crate) fn tool_output_from_completion_resolution(
    resolution: crate::Resolution,
) -> crate::ToolCallOutput {
    match resolution {
        crate::Resolution::Ok(value) => crate::ToolCallOutput::success(value),
        crate::Resolution::Err(err) => {
            let mut failure =
                crate::ToolFailure::tool(crate::ToolFailureClass::Execution, err.code, err.message);
            failure.raw = err.raw.map(crate::ToolValue::from);
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
        let result: ToolResult = Result::<_, std::io::Error>::Ok(vec!["alpha", "beta"]).into();
        assert!(result.is_success());
        assert_eq!(
            result.value_for_projection(),
            serde_json::json!(["alpha", "beta"])
        );
    }

    #[test]
    fn tool_result_from_result_formats_errors() {
        let result: ToolResult =
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

        let result: ToolResult = Result::<BrokenValue, std::io::Error>::Ok(BrokenValue).into();
        assert!(!result.is_success());
        assert_eq!(
            result.value_for_projection(),
            serde_json::json!("Failed to serialize tool result: boom")
        );
    }

    #[test]
    fn pending_result_is_not_completed_output() {
        let result = ToolResult::pending(PendingCompletion::new());
        assert!(result.is_pending());
        assert!(result.as_done_output().is_none());
        assert!(result.into_done_output().is_err());
    }
}
