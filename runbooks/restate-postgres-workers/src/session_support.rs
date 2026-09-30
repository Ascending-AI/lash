//! Session and error helpers every e2e Restate handler shares.

/// A retryable lash error is not a workflow failure: it says the identical
/// invocation is safe to run again and will converge. Turning it into a
/// `TerminalError` would make an ordinary failover — where the accepted turn
/// input is momentarily held by a driver that has gone away (ADR 0069 §5) —
/// user-visible-fatal, so retryable errors leave the invocation retryable and
/// only genuinely terminal ones end it.
pub fn turn_handler_error(err: lash::EmbedError) -> restate_sdk::errors::HandlerError {
    if err.is_retryable() {
        restate_sdk::errors::HandlerError::from(anyhow::anyhow!(err.to_string()))
    } else {
        restate_sdk::errors::TerminalError::new(err.to_string()).into()
    }
}

/// Open `session_id`, creating it first when the catalog does not hold it.
/// A handler reaches its session the same way on its first delivery and on a
/// replay, so it means create-or-use; only `create` creates (FIG-4112), and
/// an existing session is the arm where creation config does not apply.
pub async fn create_or_open_session(
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> restate_sdk::errors::HandlerResult<lash::LashSession> {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::default())
        .await
    {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
        Err(error) => return Err(turn_handler_error(error)),
    }
    core.session(session_id)
        .open()
        .await
        .map_err(turn_handler_error)
}
