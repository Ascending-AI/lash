//! The engine half of a control intent (FIG-3600 S7, ADR 0104 O4, ADR 0109).
//!
//! An intent's store half commits in the transaction that records it: a
//! parked root's redrive, cancel or fork, or a session's close. That
//! transaction also arms the intent row's `ControlIntent` obligation. What
//! remains is the engine's part — resume or release the roots' executions,
//! close their scopes, ask for the drive that follows — and
//! [`ControlIntentRelay`] delivers it: immediately, for the verb that
//! recorded the intent, and through the obligation's due index after that,
//! with capped exponential backoff until the kind's attempt ceiling.
//!
//! It names no engine: the engine's part is a [`SessionControlEngine`], the
//! drive is asked of a [`SessionWorkEngine`], and the scope owner is a
//! [`ScopeCloseSink`].
//!
//! **Claim fencing.** A delivery acts under the claim its relay took: the
//! acknowledgement and the failure it writes compare the obligation's claim
//! token, so a delivery whose claim lapsed and was retaken never settles the
//! intent under the newer claim.
//!
//! **A failed engine half.** A retryable failure keeps the intent open and
//! hands the obligation back for its next attempt; while a cancel or fork is
//! open its session admits nothing, so the next root never runs beside the
//! old execution it has yet to release. A permanent failure — the engine
//! refused for good, or the root's evidence is missing — and a retryable one
//! at the attempt ceiling close the intent `Failed { retryable: false }` and
//! stall its obligation: surfaced typed for an operator, and it never wedges
//! the session. Its store half already ended the root and raised the drive
//! epoch past the old execution's fence, so that execution can neither
//! commit nor park; the session is asked to drive its next root. Re-arming
//! the stalled obligation reopens the intent.
//!
//! **A root's scope close** follows its release but is not the intent's to
//! finish: a child whose cancel keeps failing must not hold the session's
//! cancel or fork open. A close that fails here is left to the root scope's
//! own recovery owner, which closes a verb's root once its intent settled.
//!
//! **The follow-on drive** of a cancel or fork is part of the delivery, not
//! a fire-and-forget ask: the obligation is delivered only once the engine
//! accepted drive `intent:{id}`, and a refused ask retries the obligation.
//!
//! **A redrive** applies only while the root's park still names it; the
//! store settles one the root ran past (see
//! [`decide_intent_application`](crate::store::decide_intent_application)),
//! and a cancel or fork supersedes every redrive of the root still open, so
//! a redrive never resumes a root after it re-parked or ended.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use super::relay::{DeliveryFailure, ObligationRelay, RelayPolicy, deliver_now};
use super::scope_close::{ScopeCloseAttempt, deliver_scope_close};
use crate::engine::{
    DriveRequestId, EngineAck, EngineRefusal, RootRef, ScopeCloseSink, SessionControlEngine,
};
use crate::store::{
    ClaimToken, ClaimedObligation, ControlIntent, ControlIntentId, ControlIntentKind,
    ControlIntentState, IntentApplication, IntentSettle, ObligationId, ObligationKey,
    ObligationKind, ObligationLedger, ObligationSettlement, ObligationStanding, SettleOutcome,
    StalledObligation, StoreError, scope_close_obligation_id,
};
use crate::{Clock, DeploymentStore, SessionWorkEngine};

/// The drive request a cancel's or fork's delivery asks for once its intent
/// settled, and a redrive's when the engine held no execution: one per
/// intent, so every attempt of one delivery names the same drive.
#[must_use]
pub fn intent_drive_request(intent: ControlIntentId) -> DriveRequestId {
    DriveRequestId::new(format!("intent:{intent}"))
}

/// The `ControlIntent` relay (ADR 0109 §3): delivers an intent's engine half
/// and its follow-on drive under the claim its ledger took.
pub struct ControlIntentRelay {
    ledger: ClaimRecordingLedger,
    stores: Arc<dyn DeploymentStore>,
    work: Arc<dyn SessionWorkEngine>,
    scopes: Arc<dyn ScopeCloseSink>,
    scope_close: Arc<dyn ObligationRelay>,
    clock: Arc<dyn Clock>,
    policy: RelayPolicy,
}

impl ControlIntentRelay {
    /// The relay over `ledger` (the store set's `ControlIntent` ledger),
    /// applying engine halves against `work`'s control engine and `scopes`,
    /// under the default [`RelayPolicy`]. Each released root's scope close
    /// is delivered through `scope_close`, the `ScopeClose` kind's relay
    /// (ADR 0109 §3): the close is that obligation's immediate delivery,
    /// which its ledger owns from there.
    #[must_use]
    pub fn new(
        ledger: Arc<dyn ObligationLedger>,
        stores: Arc<dyn DeploymentStore>,
        work: Arc<dyn SessionWorkEngine>,
        scopes: Arc<dyn ScopeCloseSink>,
        scope_close: Arc<dyn ObligationRelay>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            ledger: ClaimRecordingLedger::new(ledger),
            stores,
            work,
            scopes,
            scope_close,
            clock,
            policy: RelayPolicy::default(),
        }
    }

    /// The same relay under `policy`.
    #[must_use]
    pub fn with_policy(mut self, policy: RelayPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Deliver `intent`'s obligation now — the recording verb's own attempt
    /// — and answer the intent's state after it: `Acknowledged`, the state a
    /// failure retained, or the state another delivery reached first.
    ///
    /// # Errors
    ///
    /// Only a store that did not answer. A failed engine half is retained on
    /// the intent and its obligation, never an error.
    pub async fn deliver_intent(
        &self,
        intent: &ControlIntent,
    ) -> Result<ControlIntentState, StoreError> {
        let obligation = match &intent.obligation {
            Some(obligation) => obligation.clone(),
            None => self
                .stores
                .load_intent(intent.id)
                .await?
                .and_then(|stored| stored.obligation)
                .ok_or_else(|| {
                    StoreError::Backend(format!(
                        "control intent {} carries no obligation to deliver",
                        intent.id
                    ))
                })?,
        };
        deliver_now(self, &obligation, self.clock.as_ref()).await?;
        Ok(self
            .stores
            .load_intent(intent.id)
            .await?
            .ok_or(StoreError::ControlIntentUnknown { intent: intent.id })?
            .state)
    }

    /// The engine half of an open `intent`.
    async fn engine_half(&self, intent: &ControlIntent) -> Result<(), EngineHalfFailure> {
        let engine = self.work.control();
        match &intent.kind {
            ControlIntentKind::CloseSession { roots } => {
                close_session_engine_half(
                    engine.as_ref(),
                    self.scopes.as_ref(),
                    self.scope_close.as_ref(),
                    intent,
                    roots,
                    self.clock.as_ref(),
                )
                .await
            }
            ControlIntentKind::Redrive { root, .. } => {
                let target = RootRef {
                    session: intent.session_id.clone(),
                    root: root.clone(),
                };
                // An engine that holds no execution for the root resumes it
                // by driving the session: the drive is part of the resume,
                // accepted before the intent settles, so a lost ask is this
                // attempt's failure.
                if matches!(
                    engine.resume_root(&target, intent.engine.as_ref()).await?,
                    EngineAck::NothingHeld
                ) {
                    self.work
                        .request_drive(&intent.session_id, intent_drive_request(intent.id))
                        .await?;
                }
                Ok(())
            }
            ControlIntentKind::Cancel { root, .. } | ControlIntentKind::Fork { root, .. } => {
                release_root_engine_half(
                    self.stores.as_ref(),
                    engine.as_ref(),
                    self.scopes.as_ref(),
                    self.scope_close.as_ref(),
                    intent,
                    root,
                    self.clock.as_ref(),
                )
                .await
            }
        }
    }

    /// The drive a settled cancel or fork owes its session: the next root
    /// runs once the old one is released, or once its release failed for
    /// good (the store half already fenced the old execution out).
    async fn follow_on(&self, intent: &ControlIntent) -> Result<(), DeliveryFailure> {
        let settled = matches!(
            intent.state,
            ControlIntentState::Acknowledged { .. }
                | ControlIntentState::Failed {
                    retryable: false,
                    ..
                }
        );
        if settled
            && matches!(
                intent.kind,
                ControlIntentKind::Cancel { .. } | ControlIntentKind::Fork { .. }
            )
        {
            self.work
                .request_drive(&intent.session_id, intent_drive_request(intent.id))
                .await
                .map_err(|refusal| DeliveryFailure::Retryable(refusal.to_string()))?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ObligationRelay for ControlIntentRelay {
    fn ledger(&self) -> &dyn ObligationLedger {
        &self.ledger
    }

    fn policy(&self) -> RelayPolicy {
        self.policy
    }

    async fn deliver(
        &self,
        id: &ObligationId,
        key: &ObligationKey,
        _attempt: u32,
    ) -> Result<(), DeliveryFailure> {
        let ObligationKey::ControlIntent { intent_id } = key else {
            return Err(DeliveryFailure::Undecodable(format!(
                "the control-intent relay cannot deliver a {} obligation",
                key.kind()
            )));
        };
        let Some(claim) = self.ledger.take_claim(id) else {
            return Err(DeliveryFailure::Retryable(format!(
                "obligation {id} reached its delivery without a claim this relay took"
            )));
        };
        let application = match self
            .stores
            .claim_intent_application(*intent_id, self.clock.timestamp_ms())
            .await
        {
            Ok(application) => application,
            Err(error @ StoreError::ControlIntentUnknown { .. }) => {
                return Err(DeliveryFailure::Refused(error.to_string()));
            }
            Err(error) => return Err(DeliveryFailure::Retryable(error.to_string())),
        };
        let intent = match application {
            // A later intent took over the root or the session: nothing is
            // owed.
            IntentApplication::Superseded(_) => return Ok(()),
            // An earlier attempt settled the intent: only what follows it
            // may still be owed.
            IntentApplication::Done(intent) => {
                self.follow_on(&intent).await?;
                return match intent.state {
                    ControlIntentState::Failed {
                        last_error,
                        retryable: false,
                    } => Err(DeliveryFailure::Refused(last_error)),
                    _ => Ok(()),
                };
            }
            IntentApplication::Apply(intent) => intent,
        };
        let at_ms = || self.clock.timestamp_ms();
        match self.engine_half(&intent).await {
            Ok(()) => match self
                .stores
                .acknowledge_intent(intent.id, &claim.token, at_ms())
                .await
                .map_err(|error| DeliveryFailure::Retryable(error.to_string()))?
            {
                IntentSettle::ClaimLost => Err(claim_lost(id)),
                IntentSettle::Held(settled) => self.follow_on(&settled).await,
            },
            Err(failure) => {
                // At the ceiling a retryable failure closes the intent for
                // good, as a permanent one does, so the session it holds is
                // released; the relay stalls the obligation.
                let exhausted =
                    failure.retryable && claim.attempts >= self.policy.attempt_ceiling.get();
                let open = failure.retryable && !exhausted;
                let settled = match self
                    .stores
                    .record_intent_failure(intent.id, &claim.token, &failure.message, open, at_ms())
                    .await
                    .map_err(|error| DeliveryFailure::Retryable(error.to_string()))?
                {
                    IntentSettle::ClaimLost => return Err(claim_lost(id)),
                    IntentSettle::Held(settled) => settled,
                };
                let mut message = failure.message;
                if !open
                    && let Err(DeliveryFailure::Retryable(drive)) = self.follow_on(&settled).await
                {
                    message = format!("{message}; its session's drive was not accepted: {drive}");
                }
                Err(if failure.retryable {
                    DeliveryFailure::Retryable(message)
                } else {
                    DeliveryFailure::Refused(message)
                })
            }
        }
    }
}

fn claim_lost(id: &ObligationId) -> DeliveryFailure {
    DeliveryFailure::Retryable(format!(
        "obligation {id} was retaken by another claim before this delivery settled its intent"
    ))
}

/// Why an intent's engine half did not finish: retained on the intent.
struct EngineHalfFailure {
    message: String,
    retryable: bool,
}

impl From<EngineRefusal> for EngineHalfFailure {
    fn from(refusal: EngineRefusal) -> Self {
        let retryable = matches!(refusal, EngineRefusal::Retryable(_));
        Self {
            message: refusal.to_string(),
            retryable,
        }
    }
}

/// A `CloseSession`'s engine half: release every root it closed, then close
/// the session's scope. The roots' terminal evidence is already durable (the
/// intent's store half wrote it), so each release is the engine's own
/// cleanup and never decides an outcome. A closing session admits nothing,
/// so an open close wedges no work.
async fn close_session_engine_half(
    engine: &dyn SessionControlEngine,
    scopes: &dyn ScopeCloseSink,
    scope_close: &dyn ObligationRelay,
    intent: &ControlIntent,
    roots: &[crate::TurnId],
    clock: &dyn Clock,
) -> Result<(), EngineHalfFailure> {
    for root in roots {
        engine
            .release_root(
                &RootRef {
                    session: intent.session_id.clone(),
                    root: root.clone(),
                },
                None,
            )
            .await?;
    }
    // Each closed root's armed obligation gets its immediate attempt here:
    // the verdict stays the ledger's, whose retries the due pass owns, so a
    // failure is reported, never fatal (ADR 0109 §3). The session's own
    // scope close below carries no obligation of this kind.
    for root in roots {
        let id = scope_close_obligation_id(&intent.session_id, root);
        if let Err(error) = deliver_now(scope_close, &id, clock).await {
            tracing::warn!(
                session_id = intent.session_id.as_str(),
                root = root.as_str(),
                error = %error,
                "the closed root's scope-close obligation missed its immediate \
                 delivery; the due pass owns it"
            );
        }
    }
    scopes
        .close_session_scope(&intent.session_id, intent.id, roots)
        .await
        .map_err(|error| EngineHalfFailure {
            message: format!("session scope close: {error}"),
            retryable: true,
        })
}

/// A cancel's or fork's engine half: release the root's execution, then
/// attempt its scope close once. A missed close is its armed `ScopeClose`
/// obligation's to retry and never holds the intent open: a failing child
/// cancel inside the close must not wedge the session behind its cancel or
/// fork.
async fn release_root_engine_half(
    stores: &dyn DeploymentStore,
    engine: &dyn SessionControlEngine,
    scopes: &dyn ScopeCloseSink,
    scope_close: &dyn ObligationRelay,
    intent: &ControlIntent,
    root: &crate::TurnId,
    clock: &dyn Clock,
) -> Result<(), EngineHalfFailure> {
    engine
        .release_root(
            &RootRef {
                session: intent.session_id.clone(),
                root: root.clone(),
            },
            intent.engine.as_ref(),
        )
        .await?;
    let terminal = stores
        .root_terminal(&intent.session_id, root)
        .await
        .map_err(|error| EngineHalfFailure {
            message: error.to_string(),
            retryable: true,
        })?
        .ok_or_else(|| EngineHalfFailure {
            message: "root control intent has no terminal evidence".into(),
            retryable: false,
        })?;
    match deliver_scope_close(scope_close, scopes, &terminal, clock)
        .await
        .map_err(|error| EngineHalfFailure {
            message: error.to_string(),
            retryable: true,
        })? {
        ScopeCloseAttempt::Delivered => Ok(()),
        ScopeCloseAttempt::Owed { retryable } => {
            tracing::warn!(
                session_id = intent.session_id.as_str(),
                root = root.as_str(),
                intent = %intent.id,
                retryable,
                "a released root's scope close missed; its obligation owns the retry"
            );
            Ok(())
        }
    }
}

/// A ledger that remembers the claim each obligation was delivered under:
/// [`ObligationRelay::deliver`] names only the obligation, and the intent
/// writes a delivery makes compare the claim's token and read its attempt.
struct ClaimRecordingLedger {
    inner: Arc<dyn ObligationLedger>,
    claims: Mutex<BTreeMap<ObligationId, Claim>>,
}

struct Claim {
    token: ClaimToken,
    attempts: u32,
}

impl ClaimRecordingLedger {
    fn new(inner: Arc<dyn ObligationLedger>) -> Self {
        Self {
            inner,
            claims: Mutex::new(BTreeMap::new()),
        }
    }

    fn record(&self, claimed: &ClaimedObligation) {
        self.claims
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                claimed.id.clone(),
                Claim {
                    token: claimed.token.clone(),
                    attempts: claimed.attempts,
                },
            );
    }

    fn take_claim(&self, id: &ObligationId) -> Option<Claim> {
        self.claims
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
    }
}

#[async_trait::async_trait]
impl ObligationLedger for ClaimRecordingLedger {
    fn kind(&self) -> ObligationKind {
        self.inner.kind()
    }

    async fn arm(
        &self,
        key: &ObligationKey,
        now_ms: u64,
    ) -> Result<Option<ObligationId>, StoreError> {
        self.inner.arm(key, now_ms).await
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        let claimed = self.inner.claim_due(now_ms, claim_ttl_ms, limit).await?;
        for obligation in &claimed {
            self.record(obligation);
        }
        Ok(claimed)
    }

    async fn claim(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        let claimed = self.inner.claim(id, token, now_ms, claim_ttl_ms).await?;
        if let Some(obligation) = &claimed {
            self.record(obligation);
        }
        Ok(claimed)
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        self.inner.settle(id, token, settlement, now_ms).await
    }

    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError> {
        self.inner.rearm(id, now_ms).await
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        self.inner.list_stalled(after, limit).await
    }

    async fn count_stalled(&self) -> Result<u64, StoreError> {
        self.inner.count_stalled().await
    }

    async fn standing(&self, id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError> {
        self.inner.standing(id).await
    }
}
