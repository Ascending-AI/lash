#![allow(deprecated, reason = "Restate SDK retains the trait service API")]
//! Restate supplies the schedule; the kernel owns every recovery decision.
use restate_sdk::context::{
    ContextClient, ContextReadState, ContextSideEffects, ContextWriteState, ObjectContext,
    RunFuture as _,
};
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ReconcileRequest {
    pub version: u32,
    pub sequence: u64,
}

#[restate_sdk::object]
pub(crate) trait LashReconcile {
    async fn tick(request: Json<ReconcileRequest>) -> HandlerResult<()>;
}

pub(crate) struct LashReconcileImpl(pub(crate) crate::RestateSessionDriverSlot);

impl LashReconcile for LashReconcileImpl {
    async fn tick(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<ReconcileRequest>,
    ) -> HandlerResult<()> {
        if request.version != crate::LASH_SESSION_DRIVE_VERSION {
            return Err(TerminalError::new("unsupported reconcile generation").into());
        }
        let previous = ctx.get::<u64>("sequence").await?;
        if previous.is_some_and(|previous| previous >= request.sequence) {
            return Ok(());
        }
        let cursor = ctx
            .get::<Json<lash_core::engine::ReconcileCursor>>("cursor")
            .await?
            .map(|value| value.0)
            .unwrap_or_default();
        let driver = self.0.installed().ok_or_else(|| {
            HandlerError::from(std::io::Error::other("no reconcile driver installed"))
        })?;
        let tick = format!("{}:{}", request.version, request.sequence);
        let next = ctx
            .run(|| async {
                driver
                    .reconcile(
                        &cursor,
                        std::num::NonZeroUsize::MIN.saturating_add(63),
                        &tick,
                    )
                    .await
                    .map(Json)
                    .map_err(|error| HandlerError::from(std::io::Error::other(error.to_string())))
            })
            .name("reconcile-page")
            .await?;
        ctx.set("cursor", next);
        ctx.set("sequence", request.sequence);
        let sequence = request
            .sequence
            .checked_add(1)
            .ok_or_else(|| TerminalError::new("reconcile sequence exhausted"))?;
        ctx.object_client::<LashReconcileClient>("recovery")
            .tick(Json(ReconcileRequest {
                version: request.version,
                sequence,
            }))
            .send_after(std::time::Duration::from_secs(10));
        Ok(())
    }
}

/// Keep the startup send alive while deployment registration catches up.
/// After acceptance, the durable object owns every subsequent tick.
pub(crate) async fn start_reconciliation(ingress: crate::RestateIngressClient) {
    let request = ReconcileRequest {
        version: crate::LASH_SESSION_DRIVE_VERSION,
        sequence: 0,
    };
    let key = format!("reconcile-start:{}", request.version);
    let mut delay = std::time::Duration::from_millis(250);
    loop {
        match ingress
            .send_object_json_idempotent(
                crate::LashService::Reconcile.name(),
                "recovery",
                "tick",
                &request,
                &key,
            )
            .await
        {
            Ok(_) => return,
            Err(error) => tracing::warn!(%error, "session reconcile startup send will retry"),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(std::time::Duration::from_secs(5));
    }
}
