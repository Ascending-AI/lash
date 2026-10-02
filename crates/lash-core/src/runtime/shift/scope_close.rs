//! The scope-close obligation relay (ADR 0109 §3): the engine half that
//! closes a terminal run's lifetime scope.
//!
//! The terminal transaction arms the run's `session_runs` row with a
//! `ScopeClose` obligation derived from `(session, run)`
//! ([`ObligationKey::id`]). The producer's own call path delivers it
//! at once through [`deliver_now`] — the shift's recorded `CloseRunScope`
//! step, a released run's control-intent engine half, or a settlement that
//! ran with no shift to record one — and the reconcile tick's due pass
//! delivers whatever an interrupted attempt left. The broad terminal-run
//! scan is gone: the armed row is the recovery owner.
//!
//! Delivery reads the terminal evidence the armed row already carries and
//! hands it to the host's [`ScopeCloseSink`], idempotent per run. A row
//! that names no terminal evidence can never settle — the obligation lives
//! on the same row the evidence does — so it is refused and stalled for an
//! operator rather than retried to its ceiling.

use std::sync::Arc;

use crate::engine::ScopeCloseSink;
use crate::store::{ObligationKey, ObligationKind, ObligationLedger, RunTerminal, StoreError};
use crate::{Clock, DeploymentStore};

use super::relay::{
    DeliveryFailure, ObligationDelivery, ObligationRelay, RelayPolicy, RelayVerdict, deliver_now,
};

/// One deployment's scope-close relay: the `session_runs` obligation
/// ledger it claims from, the catalog it reads terminal evidence from, and
/// the scope owner it delivers to.
#[derive(Clone)]
pub struct ScopeCloseRelay {
    ledger: Arc<dyn ObligationLedger>,
    sessions: Arc<dyn DeploymentStore>,
    sink: Arc<dyn ScopeCloseSink>,
    policy: RelayPolicy,
    metrics: lash_trace::telemetry::metrics::TelemetryMetrics,
}

impl ScopeCloseRelay {
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: lash_trace::telemetry::metrics::TelemetryMetrics,
    ) -> Self {
        self.metrics = metrics;
        self
    }

    /// The relay over `ledger` (must be the `ScopeClose` kind's), reading
    /// `sessions` and closing through `sink`, on the default policy.
    #[must_use]
    pub fn new(
        ledger: Arc<dyn ObligationLedger>,
        sessions: Arc<dyn DeploymentStore>,
        sink: Arc<dyn ScopeCloseSink>,
    ) -> Self {
        Self {
            ledger,
            sessions,
            sink,
            policy: RelayPolicy::default(),
            metrics: Default::default(),
        }
    }

    /// The relay over `backend`'s `ScopeClose` ledger: the one every host
    /// delivers a run's scope close through (ADR 0109 §3).
    #[must_use]
    pub fn over_backend(
        backend: &crate::Backend,
        sessions: Arc<dyn DeploymentStore>,
        sink: Arc<dyn ScopeCloseSink>,
    ) -> Self {
        Self::new(
            backend.obligation_ledger(ObligationKind::ScopeClose),
            sessions,
            sink,
        )
    }

    /// The relay on `policy` rather than the deployment default — a lever
    /// for tests that need an immediate retry or a short claim.
    #[must_use]
    pub fn with_policy(mut self, policy: RelayPolicy) -> Self {
        self.policy = policy;
        self
    }
}

#[async_trait::async_trait]
impl ObligationRelay for ScopeCloseRelay {
    fn metrics(&self) -> lash_trace::telemetry::metrics::TelemetryMetrics {
        self.metrics.clone()
    }

    fn ledger(&self) -> &dyn ObligationLedger {
        self.ledger.as_ref()
    }

    fn policy(&self) -> RelayPolicy {
        self.policy
    }

    async fn deliver(&self, delivery: ObligationDelivery<'_>) -> Result<(), DeliveryFailure> {
        let ObligationDelivery {
            key, started_ms, ..
        } = delivery;
        let ObligationKey::ScopeClose { session_id, run } = key else {
            return Err(DeliveryFailure::key_mismatch(
                ObligationKind::ScopeClose,
                key,
            ));
        };
        let terminal = match self.sessions.run_terminal(session_id, run).await {
            Ok(terminal) => terminal.ok_or_else(|| {
                DeliveryFailure::row_invariant(format!(
                    "run `{run}` of session `{session_id}` armed a scope close but \
                     carries no terminal evidence"
                ))
            })?,
            Err(error) => return Err(self.failure(session_id, run, error, started_ms).await),
        };
        match self.sink.close_run_scope(&terminal).await {
            Ok(()) => Ok(()),
            Err(error) => Err(self.failure(session_id, run, error, started_ms).await),
        }
    }
}

impl ScopeCloseRelay {
    /// Why the close of `run` did not deliver. Corrupt stored data is
    /// refused, so the obligation stalls at once instead of retrying a read
    /// no attempt repairs, and it is recorded as the session's fault (ADR
    /// 0109 §9): the run's answer is already published, so the fault is how
    /// a host learns of it. A fault that could not be recorded leaves the
    /// close owed, and the next attempt records it. Every other store error
    /// is worth another attempt.
    async fn failure(
        &self,
        session_id: &crate::SessionId,
        run: &crate::TurnId,
        error: StoreError,
        at_ms: u64,
    ) -> DeliveryFailure {
        if error.runtime_code() != crate::RuntimeErrorCode::RuntimeStoreCorrupt {
            return DeliveryFailure::retryable(error);
        }
        let record = crate::store::SessionFaultRecord::new(
            crate::store::SessionFaultOrigin::ScopeClose { run: run.clone() },
            &error.runtime_error(),
        );
        match self
            .sessions
            .record_session_fault(session_id, &record, at_ms)
            .await
        {
            Ok(_) | Err(StoreError::UnsupportedStoreOperation { .. }) => {
                DeliveryFailure::refused(error)
            }
            Err(unrecorded) => DeliveryFailure::retryable(unrecorded),
        }
    }
}

/// How the producer's immediate scope-close attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeCloseAttempt {
    /// The scope close is delivered: by this attempt, a racing claim, or an
    /// already-settled row.
    Delivered,
    /// The attempt missed and the armed obligation owns what follows:
    /// `retryable` is `false` when the row stalled, `true` while the ledger
    /// still schedules retries. The caller whose own contract records the
    /// miss (a control intent's engine half) does so; every other caller's
    /// attempt is done either way.
    Owed { retryable: bool },
}

/// Deliver `terminal`'s scope close through `relay`: the armed row's
/// immediate delivery, whose misses the due pass owns. A store that armed no
/// obligation for the run (a terminal row older than obligations) takes the
/// close through `sink` once the ledger proves no row carries the id.
///
/// # Errors
///
/// Only a store failure; a delivery the relay retried or stalled is a
/// [`ScopeCloseAttempt::Owed`] the obligation ledger owns, not an error.
pub async fn deliver_scope_close(
    relay: &dyn ObligationRelay,
    sink: &dyn ScopeCloseSink,
    terminal: &RunTerminal,
    clock: &dyn Clock,
) -> Result<ScopeCloseAttempt, StoreError> {
    let id = crate::store::ObligationKey::ScopeClose {
        session_id: terminal.session_id.clone(),
        run: terminal.run.clone(),
    }
    .id();
    match deliver_now(relay, &id, clock).await? {
        RelayVerdict::Delivered | RelayVerdict::ClaimLost => Ok(ScopeCloseAttempt::Delivered),
        // Asked, and still owed until its consumer settles it.
        RelayVerdict::Retried { .. } | RelayVerdict::Requested => {
            Ok(ScopeCloseAttempt::Owed { retryable: true })
        }
        RelayVerdict::Stalled(_) => Ok(ScopeCloseAttempt::Owed { retryable: false }),
        // A scope close is never deferred; if it were, it is still owed.
        RelayVerdict::Deferred { .. } => Ok(ScopeCloseAttempt::Owed { retryable: true }),
        RelayVerdict::NotDue => {
            if relay.ledger().state(&id).await?.is_none() {
                sink.close_run_scope(terminal).await?;
            }
            Ok(ScopeCloseAttempt::Delivered)
        }
    }
}
