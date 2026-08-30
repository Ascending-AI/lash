fn record_turn_failure(
    state: &AppState,
    session_id: &str,
    turn_id: &str,
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

fn terminal_handler_error(err: AppError) -> HandlerError {
    TerminalError::new(err.message).into()
}

fn session_delete_handler_error(err: AppError) -> HandlerError {
    if err.verdict == AppErrorVerdict::Retryable {
        HandlerError::from(err)
    } else {
        TerminalError::new_with_code(err.status.as_u16(), err.message).into()
    }
}

fn settlement_handler_error(err: AppError) -> HandlerError {
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

fn classified_embed_handler_error(error: lash::EmbedError) -> HandlerError {
    settlement_handler_error(AppError::runtime(error))
}

fn classified_plugin_handler_error(error: lash::plugins::PluginError) -> HandlerError {
    classified_embed_handler_error(lash::EmbedError::Plugin(error))
}
