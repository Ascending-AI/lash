#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

//! A session deletion served on the Restate double the way the workbench's
//! delete workflow serves it: a session close is a journaled effect, so it
//! is issued from a workflow handler of the deployment, through the core's
//! administration over that invocation's controller.

use std::sync::Arc;

use lash::SessionId;
use restate_sdk::context::WorkflowContext;
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;

use super::SessionDeleteAttempt;

const SERVICE: &str = "WorkbenchTestSessionDelete";

#[restate_sdk::workflow]
pub(crate) trait WorkbenchTestSessionDelete {
    async fn run(session_id: Json<String>) -> HandlerResult<Json<()>>;
}

struct WorkbenchTestSessionDeleteImpl {
    administration: lash_restate::RestateSessionAdministration,
    attempt: SessionDeleteAttempt,
}

impl WorkbenchTestSessionDelete for WorkbenchTestSessionDeleteImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(session_id): Json<String>,
    ) -> HandlerResult<Json<()>> {
        let execution = self.administration.for_invocation(ctx);
        let context = execution
            .delete_context(&session_id)
            .map_err(TerminalError::from_error)?;
        (self.attempt)(context).await.map_err(TerminalError::new)?;
        Ok(Json(()))
    }
}

/// Run `attempt` over a deletion context for `session_id` in a workflow
/// handler served on `double` for `core`.
pub(crate) async fn run_session_delete_in_handler(
    double: &lash_restate_test::RestateTestBackend,
    core: &lash::LashCore,
    session_id: &SessionId,
    attempt: SessionDeleteAttempt,
) -> Result<(), String> {
    let authority = lash_restate::RestateAuthorityId::new(format!(
        "lash-restate-test-{}",
        double.server().config().seed
    ))
    .map_err(|error| error.to_string())?;
    let workflow = WorkbenchTestSessionDeleteImpl {
        administration: lash_restate::RestateSessionAdministration::new(
            core.session_administration().await,
            double.connection(),
            authority,
        ),
        attempt: Arc::clone(&attempt),
    };
    double
        .server()
        .register(
            restate_sdk::endpoint::Endpoint::builder()
                .bind(workflow.serve())
                .build(),
        )
        .await
        .map_err(|error| format!("register the test delete workflow: {error:?}"))?;
    double
        .ingress()
        .call_workflow_json::<_, ()>(
            SERVICE,
            &format!("{session_id}:{}", uuid::Uuid::new_v4()),
            "run",
            &session_id.to_string(),
        )
        .await
        .map_err(|error| error.to_string())
}
