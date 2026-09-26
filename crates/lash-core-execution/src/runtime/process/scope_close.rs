//! The process registry's scope-close adapter (FIG-3607 item 7, R9, R10): the
//! owner of lifetime scopes the drive reports a closed root or session to.
//!
//! Closing a scope writes its row in the registry's scope-close ledger. The
//! row is what requests cancellation of every process living `Until` the
//! scope, and what refuses a start that names the scope as its starter or its
//! lifetime afterwards (R11). Writing a row that already exists keeps the
//! first, so the drive's at-least-once call is idempotent per root and per
//! session.
//!
//! A sink built with a process-work port also **applies** the plan the row
//! records (FIG-3822): it delivers `ParentEnded` to each live `Until` child
//! through the engine before recording the child's cancel request, then
//! settles the row. A sink without one only records; a host that runs no
//! processes has nothing to deliver to, and the reconcile tick applies the
//! plan of any close whose apply was lost.

use std::sync::Arc;

use crate::engine::ScopeCloseSink;
use crate::store::{ControlIntentId, RootTerminal, StoreError};
use crate::{
    Clock, ProcessRegistry, ProcessWorkSubstrate, ScopeId, SessionId, TurnId,
    apply_parent_end_plan, end_session_roots,
};

/// Closes lifetime scopes in a process registry's scope-close ledger, and —
/// when built over a process-work port — applies the plan each row records.
#[derive(Clone)]
pub struct RegistryScopeClose {
    registry: Arc<dyn ProcessRegistry>,
    delivery: Option<Arc<dyn ProcessWorkSubstrate>>,
    clock: Arc<dyn Clock>,
}

impl RegistryScopeClose {
    /// The adapter over `registry`: record-only, for a host that runs no
    /// processes. The reconcile pass applies what the row leaves pending.
    #[must_use]
    pub fn new(registry: Arc<dyn ProcessRegistry>, clock: Arc<dyn Clock>) -> Self {
        Self {
            registry,
            delivery: None,
            clock,
        }
    }

    /// The adapter over `registry` whose closes also apply the plan they
    /// record, delivering `ParentEnded` through `delivery` (FIG-3822).
    #[must_use]
    pub fn with_delivery(
        registry: Arc<dyn ProcessRegistry>,
        delivery: Arc<dyn ProcessWorkSubstrate>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            registry,
            delivery: Some(delivery),
            clock,
        }
    }

    /// Record `scope`'s end and, when this sink delivers, apply its plan.
    /// Recording is idempotent and keeps the first `ended_at_ms`, and the
    /// application is idempotent per child, so an at-least-once close ends a
    /// scope once.
    async fn close(&self, scope: &ScopeId) -> Result<(), StoreError> {
        let result = if let Some(delivery) = &self.delivery {
            match self.registry.record_parent_end(scope).await {
                Ok(()) => apply_parent_end_plan(
                    self.registry.as_ref(),
                    delivery.as_ref(),
                    scope,
                    self.clock.timestamp_ms(),
                )
                .await
                .map(|_| ()),
                Err(error) => Err(error),
            }
        } else {
            self.registry.record_parent_end(scope).await.map(|_| ())
        };
        result.map_err(|error| StoreError::Backend(format!("close scope `{scope}`: {error}")))
    }
}

#[async_trait::async_trait]
impl ScopeCloseSink for RegistryScopeClose {
    async fn close_root_scope(&self, terminal: &RootTerminal) -> Result<(), StoreError> {
        self.close(&ScopeId::turn(
            terminal.session_id.clone(),
            terminal.root.clone(),
        ))
        .await
    }

    /// The session's roots close before the session itself: a start that
    /// names a root is refused from the moment its root closes, and the
    /// session's own row is the last fact the close writes.
    async fn close_session_scope(
        &self,
        session: &SessionId,
        _intent: ControlIntentId,
        roots: &[TurnId],
    ) -> Result<(), StoreError> {
        if let Some(delivery) = &self.delivery {
            end_session_roots(
                self.registry.as_ref(),
                delivery.as_ref(),
                session,
                roots,
                self.clock.timestamp_ms(),
            )
            .await
            .map_err(|error| {
                StoreError::Backend(format!("close session `{session}` roots: {error}"))
            })?;
            self.close(&ScopeId::session(session.clone())).await
        } else {
            for root in roots {
                self.close(&ScopeId::turn(session.clone(), root.clone()))
                    .await?;
            }
            self.close(&ScopeId::session(session.clone())).await
        }
    }
}
