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
//! settles the row. A sink without one records the row and settles it at
//! once when no process lives `Until` the scope: its `ParentEnd` obligation
//! (ADR 0109 §3) owes no cancel. A plan with a child stays owed for the
//! relay of a deployment that can deliver it.

use std::sync::Arc;

use crate::engine::ScopeCloseSink;
use crate::store::{ControlIntentId, RootTerminal, StoreError};
use crate::{
    Clock, EffectHost, ProcessRegistry, ProcessWorkSubstrate, ScopeId, SessionId, TurnId,
    apply_parent_end_plan, end_session_roots,
};

/// Closes lifetime scopes in a process registry's scope-close ledger, and —
/// when built over a process-work port — applies the plan each row records.
#[derive(Clone)]
pub struct RegistryScopeClose {
    registry: Arc<dyn ProcessRegistry>,
    delivery: Option<Arc<dyn ProcessWorkSubstrate>>,
    clock: Arc<dyn Clock>,
    effect_host: Option<Arc<dyn EffectHost>>,
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
            effect_host: None,
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
            effect_host: None,
        }
    }

    /// Retire the closed root's wait-index rows through the same engine host
    /// that issued them. This runs after the registry records the scope end.
    #[must_use]
    pub fn with_effect_host(mut self, effect_host: Arc<dyn EffectHost>) -> Self {
        self.effect_host = Some(effect_host);
        self
    }

    async fn retire_root_waits(
        &self,
        session: &SessionId,
        root: &TurnId,
    ) -> Result<(), StoreError> {
        if let Some(host) = &self.effect_host {
            host.retire_closed_root_waits(session, root)
                .await
                .map_err(|error| {
                    StoreError::Backend(format!(
                        "retire wait index for root `{root}` of session `{session}`: {error}"
                    ))
                })?;
        }
        Ok(())
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
            match self.registry.record_parent_end(scope).await {
                Ok(()) => settle_childless_plan(self.registry.as_ref(), scope).await,
                Err(error) => Err(error),
            }
        };
        result.map_err(|error| StoreError::Backend(format!("close scope `{scope}`: {error}")))
    }
}

/// Settle `scope`'s recorded plan when no process lives `Until` it: every
/// child's cancel is then vacuously delivered, so the plan's obligation owes
/// nothing (ADR 0109 §3).
async fn settle_childless_plan(
    registry: &dyn ProcessRegistry,
    scope: &ScopeId,
) -> Result<(), crate::PluginError> {
    if registry
        .list_parent_end_children(scope, None, std::num::NonZeroUsize::MIN)
        .await?
        .is_empty()
    {
        registry.settle_parent_end_plan(scope).await?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl ScopeCloseSink for RegistryScopeClose {
    async fn close_root_scope(&self, terminal: &RootTerminal) -> Result<(), StoreError> {
        self.close(&ScopeId::turn(
            terminal.session_id.clone(),
            terminal.root.clone(),
        ))
        .await?;
        self.retire_root_waits(&terminal.session_id, &terminal.root)
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
            for root in roots {
                self.retire_root_waits(session, root).await?;
            }
            self.close(&ScopeId::session(session.clone())).await
        } else {
            for root in roots {
                self.close(&ScopeId::turn(session.clone(), root.clone()))
                    .await?;
                self.retire_root_waits(session, root).await?;
            }
            self.close(&ScopeId::session(session.clone())).await
        }
    }
}
