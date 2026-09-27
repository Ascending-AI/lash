//! A dispatched child's durable cancel fact (ADR 0105 §4, FIG-3904).
//!
//! The fact is the child's `Cancel` group wait. The index resolves it
//! `Cancel` when a close or a retirement decides the child's cancel, `Retired`
//! when the group retires, and `Settled` once the child's own settlement is
//! seated (FIG-3709). Every terminal but `Settled` is a cancel.
//!
//! A child reads the fact three ways, one per place it runs:
//!
//! - a durable wait of the child races it as a journaled arm, a call on the
//!   cancel wait beside the wait's own command, so a replay takes the arm the
//!   first execution took;
//! - a tool child's drive peeks it at each step boundary, a journaled call;
//! - a recorded step body watches it live through the ingress, which the
//!   journal never sees: the body's recorded outcome is what a replay serves.

use super::*;

/// One child's cancel fact, as the controller that drives the child holds it.
#[derive(Clone)]
pub(crate) struct GroupChildCancel {
    key: AwaitEventKey,
    watch: Arc<dyn lash_core::GroupChildCancelWatch>,
}

impl GroupChildCancel {
    pub(crate) fn new(
        ingress: RestateIngressClient,
        namespace: crate::RestateNamespace,
        key: AwaitEventKey,
    ) -> Self {
        Self {
            watch: Arc::new(IngressChildCancelWatch {
                ingress,
                namespace,
                key: key.clone(),
            }),
            key,
        }
    }

    /// The journaled arm's request: an await on the child's cancel wait.
    pub(crate) fn await_request(&self) -> RestateDurableWaitAwaitRequest {
        RestateDurableWaitAwaitRequest {
            key: self.key.clone(),
            deadline: None,
        }
    }

    /// The peek's address and replay key.
    pub(crate) fn peek_target(&self) -> (RestateDurableWaitAddress, String) {
        (
            RestateDurableWaitAddress::for_key(&self.key),
            self.key.key_id.clone(),
        )
    }

    /// The live watch a recorded step body of the child races.
    pub(crate) fn watch(&self) -> Arc<dyn lash_core::GroupChildCancelWatch> {
        Arc::clone(&self.watch)
    }
}

impl std::fmt::Debug for GroupChildCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupChildCancel")
            .field("key", &self.key.key_id)
            .finish_non_exhaustive()
    }
}

/// Whether a resolution of a child's cancel wait is a cancel: every terminal
/// but the child's own `Settled`.
pub(crate) fn group_child_cancel_verdict(resolution: Resolution) -> bool {
    !matches!(
        decode_wait_resolution(resolution),
        Ok(EffectGroupWaitResolution::Settled)
    )
}

/// The live watch: an ingress call on the child's cancel wait, kept out of
/// the child's journal. A wait that ends `Settled` is no cancel, and the
/// watch stays pending. An ingress timeout re-attaches; any other fault is
/// the watch's, which its caller retries on the shared cancel-watch ladder.
struct IngressChildCancelWatch {
    ingress: RestateIngressClient,
    namespace: crate::RestateNamespace,
    key: AwaitEventKey,
}

#[async_trait::async_trait]
impl lash_core::GroupChildCancelWatch for IngressChildCancelWatch {
    async fn cancelled(&self) -> Result<(), lash_core::RuntimeError> {
        let address = RestateDurableWaitAddress::for_key(&self.key);
        let request = RestateDurableWaitAwaitRequest {
            key: self.key.clone(),
            deadline: None,
        };
        let resolution = loop {
            match self
                .ingress
                .call_workflow_json::<_, Resolution>(
                    &self
                        .namespace
                        .stable(crate::LashService::DurableWaitWorkflow)
                        .name(),
                    &address.workflow_key,
                    "await_resolution",
                    &request,
                )
                .await
            {
                Ok(resolution) => break resolution,
                Err(error) if error.is_timeout() => {}
                Err(error) => {
                    return Err(lash_core::RuntimeError::new(
                        lash_core::RuntimeErrorCode::EngineAwaitEventAwait,
                        format!("observe effect-group child cancellation: {error}"),
                    ));
                }
            }
        };
        if !group_child_cancel_verdict(resolution) {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}
