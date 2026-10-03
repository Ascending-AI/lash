//! Session and error helpers every e2e Restate handler shares.

use lash::restate::restate_sdk;

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

/// Reach `session_id` from a Restate handler: created from the harness's
/// default spec ([`crate::e2e_session_spec`]) unless the catalog already holds it, as the handler's journaled
/// `lash.host.session` step. A replay reads the step back and touches no
/// catalog, and the returned Durable Session resolves the session only inside
/// the handler's journaled acceptance and probes, so a session deleted between
/// two attempts cannot turn the replay away from its journal (FIG-4277).
pub async fn journaled_session<'ctx, C>(
    ctx: &C,
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> restate_sdk::errors::HandlerResult<lash::DurableSession>
where
    C: lash::restate::RestateControllerContext<'ctx>,
{
    core.session(session_id)
        .create_or_use_restate(ctx, lash::SessionCreation::root(crate::e2e_session_spec()))
        .await
}

/// This worker process's incarnation: one id per process, minted on first
/// use.
pub(crate) fn process_incarnation_id() -> &'static str {
    static INCARNATION_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INCARNATION_ID.get_or_init(|| uuid::Uuid::new_v4().to_string())
}
