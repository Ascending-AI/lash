use serde::Serialize;

use crate::{AppError, AppState};

pub(crate) async fn submit_restate_workflow_json<T: Serialize>(
    restate_http: &reqwest::Client,
    restate_ingress_url: &str,
    workflow: &str,
    workflow_key: &str,
    body: &T,
) -> Result<lash_restate::RestateInvocationId, AppError> {
    lash_restate::RestateIngressClient::new(lash_restate::RestateConnection::with_client(
        restate_ingress_url,
        restate_http.clone(),
    ))
    .send_workflow_json(workflow, workflow_key, "run", body)
    .await
    // Audited: this boundary receives only Restate HTTP ingress/response errors, not Lash session errors.
    .map_err(|err| AppError::internal(format!("Restate submit failed: {err}")))
}

pub(crate) async fn submit_restate_empty(
    state: &AppState,
    object: &str,
    object_key: &str,
    handler: &str,
) -> Result<(), AppError> {
    lash_restate::RestateIngressClient::new(lash_restate::RestateConnection::with_client(
        &state.restate_ingress_url,
        state.restate_http.clone(),
    ))
    .call_object_empty(object, object_key, handler)
    .await
    // Audited: this boundary receives only Restate HTTP ingress/response errors, not Lash session errors.
    .map_err(|err| AppError::internal(format!("Restate submit failed: {err}")))
}
