use super::WorkbenchSessionDeleteWorkflowRequest;
use crate::{AppError, AppState};

// Deletion is an operator-facing synchronous command, so it shares the
// workbench's two-minute durable-work wait budget instead of the generic
// six-hour attach ceiling used by long-lived workflow attachments.
const SESSION_DELETE_ATTACH_CEILING_MS: u64 = 2 * 60 * 1_000;

#[cfg(test)]
tokio::task_local! {
    static TEST_SESSION_DELETE_ATTACH_CEILING_MS: u64;
}

fn session_delete_attach_ceiling_ms() -> u64 {
    #[cfg(test)]
    if let Ok(ceiling_ms) = TEST_SESSION_DELETE_ATTACH_CEILING_MS.try_with(|value| *value) {
        return ceiling_ms;
    }
    SESSION_DELETE_ATTACH_CEILING_MS
}

#[cfg(test)]
pub(crate) async fn with_session_delete_attach_ceiling<T>(
    ceiling_ms: u64,
    future: impl std::future::Future<Output = T>,
) -> T {
    TEST_SESSION_DELETE_ATTACH_CEILING_MS
        .scope(ceiling_ms, future)
        .await
}

/// The session-delete attach ceiling in force on this task.
///
/// The reset route's retirement runs on a task of its own so an abandoned
/// browser request cannot strand it halfway, and a task does not inherit its
/// parent's task-locals. Production reads its ceiling from a constant and has
/// nothing to carry; the tests that shorten it would otherwise silently fall
/// back to the two-minute default on the far side of that boundary.
#[cfg(not(test))]
#[derive(Clone, Copy)]
pub(crate) struct AmbientAttachCeiling;
#[cfg(test)]
pub(crate) type AmbientAttachCeiling = Option<u64>;

#[cfg(not(test))]
pub(crate) fn ambient_attach_ceiling() -> AmbientAttachCeiling {
    AmbientAttachCeiling
}

#[cfg(test)]
pub(crate) fn ambient_attach_ceiling() -> AmbientAttachCeiling {
    TEST_SESSION_DELETE_ATTACH_CEILING_MS
        .try_with(|value| *value)
        .ok()
}

#[cfg(not(test))]
pub(crate) async fn carrying_attach_ceiling<T>(
    _ceiling: AmbientAttachCeiling,
    future: impl std::future::Future<Output = T>,
) -> T {
    future.await
}

#[cfg(test)]
pub(crate) async fn carrying_attach_ceiling<T>(
    ceiling: AmbientAttachCeiling,
    future: impl std::future::Future<Output = T>,
) -> T {
    match ceiling {
        Some(ceiling_ms) => with_session_delete_attach_ceiling(ceiling_ms, future).await,
        None => future.await,
    }
}

pub(crate) async fn call_session_delete(
    state: &AppState,
    request: WorkbenchSessionDeleteWorkflowRequest,
) -> Result<(), AppError> {
    let session_id = request.session_id.clone();
    let call = lash_restate::RestateIngressClient::new(
        lash_restate::RestateConnection::with_client_and_config(
            &state.restate_ingress_url,
            state.restate_http.clone(),
            lash_restate::RestateConnectionConfig {
                attach_ceiling_ms: session_delete_attach_ceiling_ms(),
                ..lash_restate::RestateConnectionConfig::default()
            },
        ),
    )
    .call_workflow_json::<_, ()>(
        "WorkbenchSessionDeleteWorkflow",
        &request.operation_id,
        "run",
        &request,
    )
    .await;
    let Err(call_error) = call else {
        return Ok(());
    };
    let call_is_definitive = matches!(
        &call_error,
        lash_restate::RestateHttpError::Status { status: 409, .. }
            | lash_restate::RestateHttpError::Encode { .. }
    );
    let durable = match state.core.session(session_id.clone()).durable().await {
        Ok(durable) => durable,
        Err(probe_error) => {
            return Err(AppError::session_delete_unconfirmed(
                &session_id,
                call_error,
                probe_error,
            ));
        }
    };
    match durable.was_deleted().await {
        Ok(true) => {
            eprintln!(
                "agent-workbench reconciled a failed Restate delete call to durable deletion: session_id={:?} call_error={call_error}",
                session_id.as_str()
            );
            Ok(())
        }
        Ok(false) if call_is_definitive => match durable.exists().await {
            Ok(true) => Err(AppError::session_delete_failed(&session_id, call_error)),
            Ok(false) => Err(AppError::session_delete_unconfirmed(
                &session_id,
                call_error,
                "the store reported neither a live session nor a durable tombstone",
            )),
            Err(probe_error) => Err(AppError::session_delete_unconfirmed(
                &session_id,
                call_error,
                probe_error,
            )),
        },
        Ok(false) => Err(AppError::session_delete_unconfirmed(
            &session_id,
            call_error,
            "the ambiguous Restate attach failure had no durable tombstone yet",
        )),
        Err(probe_error) => Err(AppError::session_delete_unconfirmed(
            &session_id,
            call_error,
            probe_error,
        )),
    }
}
