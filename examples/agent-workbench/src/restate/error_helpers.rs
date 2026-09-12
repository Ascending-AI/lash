use super::*;
use lash::SessionId;
use lash::TurnId;

pub(super) fn configured_restate_authority_id()
-> Result<lash_restate::RestateAuthorityId, TerminalError> {
    let value = std::env::var("RESTATE_AUTHORITY_ID")
        .map_err(|_| TerminalError::new("RESTATE_AUTHORITY_ID is required"))?;
    lash_restate::RestateAuthorityId::new(value).map_err(TerminalError::from_error)
}

pub(super) fn record_turn_failure(
    state: &AppState,
    session_id: &SessionId,
    turn_id: &TurnId,
    trace_name: &str,
    message: &str,
    public_message: &str,
) {
    state.trace_for_session(
        session_id,
        trace_name,
        json!({
            "session_id": session_id,
            "turn_id": turn_id,
            "error": message,
        }),
    );
    state.publish_turn_failed_with_message(session_id, turn_id, public_message);
}

pub(super) fn terminal_handler_error(err: AppError) -> HandlerError {
    TerminalError::new(err.message).into()
}

pub(super) fn session_delete_handler_error(err: AppError) -> HandlerError {
    if err.verdict == AppErrorVerdict::Retryable {
        HandlerError::from(err)
    } else {
        TerminalError::new_with_code(err.status.as_u16(), err.message).into()
    }
}

pub(super) fn settlement_handler_error(err: AppError) -> HandlerError {
    match err.verdict {
        AppErrorVerdict::Retryable => HandlerError::from(err),
        AppErrorVerdict::ReplacementAbort | AppErrorVerdict::Terminal => {
            terminal_handler_error(err)
        }
        AppErrorVerdict::Ambiguous => {
            // Ambiguous settlement failures remain retryable.
            HandlerError::from(err)
        }
    }
}

pub(super) fn classified_embed_handler_error(error: lash::EmbedError) -> HandlerError {
    settlement_handler_error(AppError::runtime(error))
}

pub(super) fn classified_plugin_handler_error(error: lash::plugins::PluginError) -> HandlerError {
    classified_embed_handler_error(lash::EmbedError::Plugin(error))
}
