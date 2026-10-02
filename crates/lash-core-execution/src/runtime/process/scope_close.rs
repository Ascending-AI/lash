//! The process registry's scope-close adapter (FIG-3607 item 7, R9, R10): the
//! owner of lifetime scopes the shift reports a closed run or session to.
//!
//! Closing a scope writes its row in the registry's scope-close ledger. The
//! row is what requests cancellation of every process living `Until` the
//! scope, and what refuses a start that names the scope as its starter or its
//! lifetime afterwards (R11). Writing a row that already exists keeps the
//! first, so the shift's at-least-once call is idempotent per run and per
//! session.
//!
//! When the session factory is installed, a run close also reads the turn
//! scopes of inputs bound to that run. Those joined inputs never become
//! runs, so their scopes close with the admitting run. The session close
//! still covers turn scopes whose inputs were never admitted (FIG-3948).
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
use crate::store::{ControlIntentId, RunTerminal, RunTerminalCause, StoreError};
use crate::{
    Clock, DeploymentStore, EffectHost, ProcessRegistry, ProcessWorkSubstrate, ScopeId, SessionId,
    TurnId, apply_parent_end_plan, end_session_runs,
};

/// Closes lifetime scopes in a process registry's scope-close ledger, and —
/// when built over a process-work port — applies the plan each row records.
#[derive(Clone)]
pub struct RegistryScopeClose {
    registry: Arc<dyn ProcessRegistry>,
    delivery: Option<Arc<dyn ProcessWorkSubstrate>>,
    clock: Arc<dyn Clock>,
    effect_host: Option<Arc<dyn EffectHost>>,
    sessions: Option<Arc<dyn DeploymentStore>>,
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
            sessions: None,
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
            sessions: None,
        }
    }

    /// Retire the closed run's wait-index rows through the same engine host
    /// that issued them. This runs after the registry records the scope end.
    #[must_use]
    pub fn with_effect_host(mut self, effect_host: Arc<dyn EffectHost>) -> Self {
        self.effect_host = Some(effect_host);
        self
    }

    /// Read the run's admitted input bindings from the session catalog when
    /// its terminal closes. The binding identifies joined turn scopes that
    /// never get their own run terminal.
    #[must_use]
    pub fn with_session_store_factory(mut self, sessions: Arc<dyn DeploymentStore>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    async fn retire_run_waits(
        &self,
        session: &SessionId,
        run: &TurnId,
        committed_turn: Option<&TurnId>,
    ) -> Result<(), StoreError> {
        if let Some(host) = &self.effect_host {
            host.retire_closed_run_waits(session, run, committed_turn)
                .await
                .map_err(|error| {
                    StoreError::Backend(format!(
                        "retire wait index for run `{run}` of session `{session}`: {error}"
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
    async fn close_run_scope(&self, terminal: &RunTerminal) -> Result<(), StoreError> {
        let joined = if let Some(sessions) = &self.sessions {
            sessions
                .bound_turn_scopes(&terminal.session_id, &terminal.run)
                .await?
        } else {
            Vec::new()
        };
        self.close(&ScopeId::turn(
            terminal.session_id.clone(),
            terminal.run.clone(),
        ))
        .await?;
        for turn in joined {
            if turn != terminal.run {
                self.close(&ScopeId::turn(terminal.session_id.clone(), turn))
                    .await?;
            }
        }
        let committed_turn = match &terminal.cause {
            RunTerminalCause::Committed { turn, .. } => Some(turn),
            _ => None,
        };
        self.retire_run_waits(&terminal.session_id, &terminal.run, committed_turn)
            .await
    }

    /// The session's runs close before the session itself: a start that
    /// names a run is refused from the moment its run closes, and the
    /// session's own row is the last fact the close writes. That row also
    /// closes every scope inside the session with no row of its own — a turn
    /// that never became a run (FIG-3948): it refuses a start naming one,
    /// and its plan cancels their children.
    async fn close_session_scope(
        &self,
        session: &SessionId,
        _intent: ControlIntentId,
        runs: &[TurnId],
    ) -> Result<(), StoreError> {
        if let Some(delivery) = &self.delivery {
            end_session_runs(
                self.registry.as_ref(),
                delivery.as_ref(),
                session,
                runs,
                self.clock.timestamp_ms(),
            )
            .await
            .map_err(|error| {
                StoreError::Backend(format!("close session `{session}` runs: {error}"))
            })?;
            for run in runs {
                self.retire_run_waits(session, run, None).await?;
            }
            self.close(&ScopeId::session(session.clone())).await
        } else {
            for run in runs {
                self.close(&ScopeId::turn(session.clone(), run.clone()))
                    .await?;
                self.retire_run_waits(session, run, None).await?;
            }
            self.close(&ScopeId::session(session.clone())).await
        }
    }
}
