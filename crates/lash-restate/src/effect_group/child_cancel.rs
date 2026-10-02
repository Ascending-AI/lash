//! A dispatched child's durable cancel fact (ADR 0105 §4, FIG-3904).
//!
//! The fact is the group index's own record of the child (FIG-4344): a
//! decided cancel is a cancel, a retirement is a cancel, and a seated
//! settlement is past every cancel. The index answers it as the child's
//! `ChildCancel` notice.
//!
//! A child reads the fact three ways, one per place it runs:
//!
//! - a durable wait of the child races the notice's awakeable as a journaled
//!   arm, subscribed beside the wait's own command, so a replay takes the arm
//!   the first execution took;
//! - a tool child's shift reads it at each step boundary, one journaled shared
//!   read of the index;
//! - a recorded step body watches it live through the ingress, which the
//!   journal never sees: the body's recorded outcome is what a replay serves.

use super::*;

/// One child's cancel fact, as the controller that executes the child holds it:
/// the group and the child's position in it.
#[derive(Clone)]
pub(crate) struct GroupChildCancel {
    group_key: String,
    position: usize,
    watch: Arc<dyn lash_core::GroupChildCancelWatch>,
}

impl GroupChildCancel {
    pub(crate) fn new(
        ingress: RestateIngressClient,
        namespace: crate::RestateNamespace,
        group_key: String,
        position: usize,
    ) -> Self {
        Self {
            watch: Arc::new(IngressChildCancelWatch {
                ingress,
                namespace,
                group_key: group_key.clone(),
                position,
            }),
            group_key,
            position,
        }
    }

    /// The group whose index holds the fact.
    pub(crate) fn group_key(&self) -> &str {
        &self.group_key
    }

    /// The child's position in its group.
    pub(crate) fn position(&self) -> usize {
        self.position
    }

    /// The live watch a recorded step body of the child races.
    pub(crate) fn watch(&self) -> Arc<dyn lash_core::GroupChildCancelWatch> {
        Arc::clone(&self.watch)
    }
}

impl std::fmt::Debug for GroupChildCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupChildCancel")
            .field("group_key", &self.group_key)
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

/// The live watch: an ingress observer of the child's cancel fact, kept out
/// of the child's journal. A decided fact answers from the index's shared
/// read; otherwise every watch of one child attaches to one `await_notice`
/// invocation under the same idempotency key, however often it reattaches
/// (FIG-4345). A child whose settlement seated is past every cancel, and the
/// watch stays pending. An attach-ceiling timeout reattaches to the same
/// waiter. Other ingress failures reach the caller's shared retry ladder.
struct IngressChildCancelWatch {
    ingress: RestateIngressClient,
    namespace: crate::RestateNamespace,
    group_key: String,
    position: usize,
}

#[async_trait::async_trait]
impl lash_core::GroupChildCancelWatch for IngressChildCancelWatch {
    async fn cancelled(&self) -> Result<(), lash_core::RuntimeError> {
        let index = self
            .namespace
            .stable(crate::LashService::EffectGroupState)
            .name();
        let notice = EffectGroupNotice::ChildCancel {
            position: self.position,
        };
        let notification = loop {
            let read = self
                .ingress
                .call_lash_object::<_, Option<EffectGroupNotification>>(
                    &index,
                    &self.group_key,
                    "child_cancel",
                    &EffectGroupChildCancelRequest {
                        position: self.position,
                    },
                )
                .await;
            let observed = match read {
                Ok(Some(notification)) => Ok(notification),
                Ok(None) => {
                    super::notifications::await_group_notice_via_ingress(
                        &self.ingress,
                        &index,
                        &self.group_key,
                        &notice,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            match observed {
                Ok(notification) => break notification,
                Err(error)
                    if error.is_timeout()
                        && error.classification() == crate::RestateHttpErrorClass::Transient => {}
                Err(error) => {
                    return Err(ingress_group_error(
                        "observe effect-group child cancellation",
                        error,
                    )
                    .into_runtime_error());
                }
            }
        };
        if !notification.is_child_cancel() {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}
