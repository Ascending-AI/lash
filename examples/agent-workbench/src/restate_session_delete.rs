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
    match state.core.session_was_deleted(&session_id).await {
        Ok(true) => {
            eprintln!(
                "agent-workbench reconciled a failed Restate delete call to durable deletion: session_id={session_id:?} call_error={call_error}"
            );
            Ok(())
        }
        Ok(false) if call_is_definitive => match state.core.session_exists(&session_id).await {
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
