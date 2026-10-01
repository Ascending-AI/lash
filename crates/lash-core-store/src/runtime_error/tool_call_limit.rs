//! The session's recorded `max_tool_calls` as a runtime error (FIG-4546).

use super::{RuntimeEffectControllerError, RuntimeErrorCause, RuntimeErrorCode};

impl RuntimeEffectControllerError {
    /// A tool call refused by the session's recorded `max_tool_calls`, with
    /// its typed cause. The message is the refusal's own, so it names the
    /// limit wherever the error is shown.
    #[must_use]
    pub fn max_tool_calls_exceeded(exceeded: crate::ToolCallLimitExceeded) -> Self {
        let mut error = Self::new(RuntimeErrorCode::MaxToolCallsExceeded, exceeded.to_string());
        error.cause = Some(RuntimeErrorCause::MaxToolCallsExceeded {
            exceeded: Box::new(exceeded),
        });
        error
    }

    /// The tool-call limit this error refused a call under. `None` on any
    /// other error.
    #[must_use]
    pub fn tool_call_limit_exceeded(&self) -> Option<crate::ToolCallLimitExceeded> {
        match self.cause.as_ref()? {
            RuntimeErrorCause::MaxToolCallsExceeded { exceeded } => Some(**exceeded),
            _ => None,
        }
    }
}
