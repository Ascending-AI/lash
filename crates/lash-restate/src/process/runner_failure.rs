//! Runtime classification and exact error causes at process boundaries.

use super::*;

/// Converts Lash failures at the Restate process-handler boundary without
/// inheriting the SDK's blanket retry policy for arbitrary Rust errors.
///
/// Lash's runtime classification is the authority: explicitly retryable
/// runtime errors request redelivery, while every other plugin/runtime failure
/// terminates the invocation so deterministic failures cannot loop forever.
pub(crate) fn handler_error_from_plugin(error: PluginError) -> HandlerError {
    if error.is_retryable() {
        crate::turn_handler::retried_attempt_failure(error.attempt_failure_text())
    } else {
        HandlerError::from(TerminalError::from_error(error))
    }
}

/// The one decision a recorded step makes of a plugin operation's result
/// (FIG-4649). A step's journaled answer is final: every replay answers it.
/// A value is journaled, and so is a terminal failure, which is the
/// operation's answer on every attempt. A fault of the attempt (the store did
/// not answer, a lease was lost, an opaque infrastructure failure) is never
/// an answer: the attempt ends with nothing recorded, and the engine runs the
/// step again.
pub(crate) fn journal_or_retry<T>(
    result: Result<T, PluginError>,
) -> Result<Result<T, PluginError>, String> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(error) => match error.class() {
            lash_core::PluginErrorClass::Terminal => Ok(Err(error)),
            lash_core::PluginErrorClass::Retryable | lash_core::PluginErrorClass::Redrivable => {
                Err(error.attempt_failure_text())
            }
        },
    }
}

pub(super) fn is_replay_mismatch(error: &PluginError) -> bool {
    match error {
        PluginError::Runtime(error) => error.code.is_replay_mismatch(),
        PluginError::RuntimeEffectController(error) => error.code.is_replay_mismatch(),
        _ => false,
    }
}

pub(super) fn is_terminal_runner_error(error: &PluginError, is_session_turn: bool) -> bool {
    // A SessionTurn retry replays the child's same sealed fence; only a
    // fresh root can regain authority after that child is superseded.
    error.is_terminal()
        || (is_session_turn
            && match error {
                PluginError::Runtime(error) => {
                    error.code == RuntimeErrorCode::StoreCommitSuperseded
                }
                PluginError::RuntimeEffectController(error) => {
                    error.code == RuntimeErrorCode::StoreCommitSuperseded
                }
                _ => false,
            })
}

pub(super) fn terminal_process_output(error: PluginError) -> ProcessAwaitOutput {
    let error = lash_core::RuntimeEffectControllerError::from(error);
    let mut failure = lash_core::ToolFailure::runtime(
        lash_core::ToolFailureClass::Execution,
        error.code.as_str(),
        error.message.clone(),
    );
    if error.cause.is_some() {
        failure.raw = Some(lash_core::ToolValue::untrusted_json(
            serde_json::json!({ "runtime_error": error }),
        ));
    }
    ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::failure(failure))
}
