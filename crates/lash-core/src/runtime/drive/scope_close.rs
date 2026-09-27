//! The scope-close obligation relay (ADR 0109 §3): the engine half that
//! closes a terminal root's lifetime scope.
//!
//! The terminal transaction arms the root's `session_roots` row with a
//! `ScopeClose` obligation derived from `(session, root)`
//! ([`scope_close_obligation_id`]). The producer's own call path delivers it
//! at once through [`deliver_now`] — the drive's recorded `CloseRootScope`
//! step, a released root's control-intent engine half, or a settlement that
//! ran with no drive to record one — and the reconcile tick's due pass
//! delivers whatever an interrupted attempt left. The broad terminal-root
//! scan is gone: the armed row is the recovery owner.
//!
//! Delivery reads the terminal evidence the armed row already carries and
//! hands it to the host's [`ScopeCloseSink`], idempotent per root. A row
//! that names no terminal evidence can never settle — the obligation lives
//! on the same row the evidence does — so it is refused and stalled for an
//! operator rather than retried to its ceiling.

use std::sync::Arc;

use crate::engine::ScopeCloseSink;
use crate::store::{
    ObligationId, ObligationKey, ObligationLedger, RootTerminal, StoreError,
    scope_close_obligation_id,
};
use crate::{Clock, SessionStoreFactory};

use super::relay::{DeliveryFailure, ObligationRelay, RelayPolicy, RelayVerdict, deliver_now};

/// One deployment's scope-close relay: the `session_roots` obligation
/// ledger it claims from, the catalog it reads terminal evidence from, and
/// the scope owner it delivers to.
#[derive(Clone)]
pub struct ScopeCloseRelay {
    ledger: Arc<dyn ObligationLedger>,
    sessions: Arc<dyn SessionStoreFactory>,
    sink: Arc<dyn ScopeCloseSink>,
    policy: RelayPolicy,
}

impl ScopeCloseRelay {
    /// The relay over `ledger` (must be the `ScopeClose` kind's), reading
    /// `sessions` and closing through `sink`, on the default policy.
    #[must_use]
    pub fn new(
        ledger: Arc<dyn ObligationLedger>,
        sessions: Arc<dyn SessionStoreFactory>,
        sink: Arc<dyn ScopeCloseSink>,
    ) -> Self {
        Self {
            ledger,
            sessions,
            sink,
            policy: RelayPolicy::default(),
        }
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
    fn ledger(&self) -> &dyn ObligationLedger {
        self.ledger.as_ref()
    }

    fn policy(&self) -> RelayPolicy {
        self.policy
    }

    async fn deliver(
        &self,
        _id: &ObligationId,
        key: &ObligationKey,
    ) -> Result<(), DeliveryFailure> {
        let ObligationKey::ScopeClose { session_id, root } = key else {
            return Err(DeliveryFailure::Undecodable(format!(
                "a scope-close delivery was handed a {} key",
                key.kind().label()
            )));
        };
        let terminal = self
            .sessions
            .root_terminal(session_id, root)
            .await
            .map_err(|error| DeliveryFailure::Retryable(error.to_string()))?
            .ok_or_else(|| {
                DeliveryFailure::Refused(format!(
                    "root `{root}` of session `{session_id}` armed a scope close but \
                     carries no terminal evidence"
                ))
            })?;
        self.sink
            .close_root_scope(&terminal)
            .await
            .map_err(|error| DeliveryFailure::Retryable(error.to_string()))
    }
}

/// The ledger's relay, when the host wires the `ScopeClose` kind's ledger:
/// `None` on a host that runs no obligation substrate, which takes the sink
/// directly.
#[must_use]
pub fn scope_close_relay(
    ledger: Option<Arc<dyn ObligationLedger>>,
    sessions: Arc<dyn SessionStoreFactory>,
    sink: Arc<dyn ScopeCloseSink>,
) -> Option<ScopeCloseRelay> {
    ledger.map(|ledger| ScopeCloseRelay::new(ledger, sessions, sink))
}

/// How the producer's immediate scope-close attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeCloseAttempt {
    /// The scope close is delivered: by this attempt, a racing claim, an
    /// already-settled row, or the sink itself on a host that runs no
    /// obligation ledger.
    Delivered,
    /// The attempt missed and the armed obligation owns what follows:
    /// `retryable` is `false` when the row stalled, `true` while the ledger
    /// still schedules retries. The caller whose own contract records the
    /// miss (a control intent's engine half) does so; every other caller's
    /// attempt is done either way.
    Owed { retryable: bool },
}

/// Deliver `terminal`'s scope close: through the obligation relay when the
/// host wires one — the armed row's immediate delivery, whose misses the
/// due pass owns — else the sink itself. A host that wires a relay over a
/// store that armed no obligation (a terminal row older than obligations)
/// takes the close itself once the ledger proves no row carries the id.
///
/// # Errors
///
/// Only a store failure; a delivery the relay retried or stalled is a
/// [`ScopeCloseAttempt::Owed`] the obligation ledger owns, not an error.
pub async fn deliver_scope_close(
    relay: Option<&dyn ObligationRelay>,
    sink: &dyn ScopeCloseSink,
    terminal: &RootTerminal,
    clock: &dyn Clock,
) -> Result<ScopeCloseAttempt, StoreError> {
    let Some(relay) = relay else {
        sink.close_root_scope(terminal).await?;
        return Ok(ScopeCloseAttempt::Delivered);
    };
    let id = scope_close_obligation_id(&terminal.session_id, &terminal.root);
    match deliver_now(relay, &id, clock).await? {
        RelayVerdict::Delivered | RelayVerdict::ClaimLost => Ok(ScopeCloseAttempt::Delivered),
        RelayVerdict::Retried { .. } => Ok(ScopeCloseAttempt::Owed { retryable: true }),
        RelayVerdict::Stalled(_) => Ok(ScopeCloseAttempt::Owed { retryable: false }),
        RelayVerdict::NotDue => {
            if relay.ledger().state(&id).await?.is_none() {
                sink.close_root_scope(terminal).await?;
            }
            Ok(ScopeCloseAttempt::Delivered)
        }
    }
}
