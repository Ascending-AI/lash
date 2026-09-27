//! A session's two-phase delete (ADR 0109 §4, the finalizer rule).
//!
//! **Phase one, the close.** [`delete_session`] closes the session
//! ([`close_session`]): its `CloseSession` intent commits, the session is
//! marked closing and refuses new sends typed
//! ([`StoreError::SessionClosing`]), and the intent's engine half releases
//! the session's roots and closes its scopes. The intent's acknowledgement
//! arms the session's `SessionDelete` obligation on its `session_meta` row,
//! in the same transaction.
//!
//! **Phase two, the physical delete.** The `SessionDelete` obligation's
//! delivery ([`SessionDeleteRelay`]) refuses, retryably, while any cleanup
//! obligation the close left behind — a root's scope close, a parent-end plan
//! of a scope the session owns — is undelivered, and while the engine still
//! runs work of the session (`SessionWorkEngine::session_work_in_flight`):
//! a drive replayed after a crash must find the session its first run saw.
//! Then it deletes the
//! session's process state, trigger subscriptions and durable waits, retires
//! its effect journal and the artifact owners that retirement queued, and
//! deletes its storage. The storage delete removes the `session_meta` row
//! the obligation lives on, so it is the last step: every step before it is
//! idempotent, and a failure anywhere leaves the obligation owed for the
//! relay's next attempt. The `CloseSession` intent stays as the session's
//! tombstone (ADR 0108 §5a).
//!
//! The verb attempts the physical delete before it returns
//! ([`deliver_now`]); what the attempt could not finish, the reconcile
//! tick's relay retries with backoff until the kind's attempt ceiling, and
//! then the obligation stalls, surfaced like any other stall (ADR 0109
//! §1.5). The caller never retries a deletion to finish it.
//!
//! [`StoreError::SessionClosing`]: crate::store::StoreError::SessionClosing

use std::sync::{Arc, Mutex};

use crate::drive::relay::{
    DeliveryFailure, ObligationRelay, RelayPolicy, RelayVerdict, deliver_now,
};
use crate::session_close::{SessionCloseError, close_session};
use crate::store::session_delete::{SessionCleanup, SessionDeleteLedger, SessionDeleteObligation};
use crate::store::{
    ControlIntentState, MaintenanceFailure, ObligationId, ObligationKey, ObligationKind,
    ObligationLedger, ObligationState, SessionBlobReclaimReport, StoreError,
};
use crate::{
    EffectJournalRetirement, ProcessSessionDeleteReport, RuntimeError, SessionAdministration,
    SessionDeleteContext, SessionId,
};

/// The store set a session's delete runs its obligation through: its
/// `SessionDelete` obligation ledger and the reads that ledger's generic
/// surface cannot make. Resolved when a delete asks, so a store set that
/// serves no ledger fails the delete, never the administration's assembly.
#[derive(Clone)]
pub struct SessionDeleteStores {
    stores: Arc<dyn crate::StoreSet>,
}

impl SessionDeleteStores {
    /// The session-delete stores of `backend`'s store set.
    #[must_use]
    pub fn of(backend: &crate::Backend) -> Self {
        Self {
            stores: backend.stores(),
        }
    }

    /// The session-delete stores of `stores`.
    #[must_use]
    pub fn of_store_set(stores: Arc<dyn crate::StoreSet>) -> Self {
        Self { stores }
    }

    /// The `SessionDelete` kind's obligation ledger.
    #[must_use]
    pub fn obligations(&self) -> Arc<dyn ObligationLedger> {
        self.stores.obligation_ledger(ObligationKind::SessionDelete)
    }

    /// Which obligation a session's delete is, and what cleanup it waits on.
    #[must_use]
    pub fn ledger(&self) -> Arc<dyn SessionDeleteLedger> {
        self.stores.session_delete_ledger()
    }
}

/// What a session's physical delete removed.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionDeleteReport {
    /// Identifier of the deleted session.
    pub session_id: SessionId,
    /// Storage reclaimed while deleting the session.
    pub storage: SessionBlobReclaimReport,
    /// Process-state deletion report.
    pub process: Option<ProcessSessionDeleteReport>,
}

/// Why one attempt at a session's physical delete stopped. Every step is
/// idempotent and the next attempt starts over.
#[derive(Debug, thiserror::Error)]
pub enum SessionDeleteFailure {
    /// The process registry did not delete the session's process state.
    #[error("process state: {message}")]
    Process { message: String },
    /// The trigger store did not delete the session's subscriptions.
    #[error("trigger subscriptions: {message}")]
    Triggers { message: String },
    /// The effect host did not revoke the session's durable waits.
    #[error("durable waits: {message}")]
    Waits { message: String },
    /// The effect host did not retire the session's effect journal.
    #[error("effect journal: {message}")]
    Journal { message: String },
    /// An artifact owner the journal retirement queued was not retired.
    #[error("artifact owner retirement: {message}")]
    Artifacts { message: String },
    /// The session's storage delete stopped, with the reclaim counters it
    /// witnessed before it did (ADR 0067).
    #[error("storage: {0}")]
    Storage(Box<MaintenanceFailure<SessionBlobReclaimReport>>),
}

/// What [`delete_session`] did.
#[derive(Debug)]
pub enum SessionDeletion {
    /// The physical delete ran in this call.
    Deleted(SessionDeleteReport),
    /// The session is closed and its physical delete had already run: a
    /// repeated deletion.
    AlreadyDeleted { session_id: SessionId },
    /// The session is closing: it refuses new work, and its physical delete
    /// is still owed.
    Closing(SessionClosing),
}

impl SessionDeletion {
    /// The physical delete's report, when this call ran it.
    #[must_use]
    pub fn deleted(&self) -> Option<&SessionDeleteReport> {
        match self {
            Self::Deleted(report) => Some(report),
            Self::AlreadyDeleted { .. } | Self::Closing(_) => None,
        }
    }
}

/// A closing session, as its deletion left it.
#[derive(Debug)]
pub struct SessionClosing {
    pub session_id: SessionId,
    /// The session's `SessionDelete` obligation; `None` while its close's
    /// engine half is unacknowledged, which is what arms it.
    pub obligation: Option<ObligationId>,
    /// What the physical delete waits on.
    pub waiting: SessionDeleteWait,
}

/// What a closing session's physical delete waits on.
#[derive(Debug)]
pub enum SessionDeleteWait {
    /// The `CloseSession` intent's engine half is not acknowledged: it is
    /// retained on the intent, and its recovery finishes it and then arms the
    /// delete.
    CloseIntent(ControlIntentState),
    /// Cleanup obligations of the session are undelivered; the relay
    /// attempts the delete again once they are.
    Cleanup(SessionCleanup),
    /// The engine still runs work of the session (a drive or a turn that has
    /// not finished, or a replay of one); the relay attempts the delete again
    /// once it ends, so no replay reads a session its first run saw live.
    EngineWork,
    /// This call's attempt failed; the relay attempts it again.
    Failed(SessionDeleteFailure),
    /// The obligation is not due: another relay holds it (`Claimed`), or it
    /// stalled and waits for an operator's re-arm (`Stalled`).
    Obligation(ObligationState),
}

/// Why a deletion did not close its session, or a session with no durable
/// record did not delete. Nothing was closed and the caller may retry.
#[derive(Debug, thiserror::Error)]
pub enum SessionDeleteError {
    #[error(transparent)]
    Close(#[from] SessionCloseError),
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A session with no durable record owes no obligation, so its cleanup
    /// runs in the call, and it failed.
    #[error("session `{session_id}` delete: {failure}")]
    Unrecorded {
        session_id: SessionId,
        failure: SessionDeleteFailure,
    },
}

/// Delete the session `context` deletes (ADR 0109 §4): close it, then
/// attempt the physical delete its close armed.
///
/// A session with no durable record has nothing to close and owes no
/// obligation; what it may still have left behind — process state,
/// subscriptions, waits — is deleted in the call.
///
/// # Errors
///
/// A refusal or fault of the close (nothing was closed), a store that did
/// not answer, or an unrecorded session's failed cleanup. A failed physical
/// delete is not an error: the session is closing and its obligation is
/// retried.
pub async fn delete_session(
    context: &SessionDeleteContext<'_>,
) -> Result<SessionDeletion, SessionDeleteError> {
    let session_id = context.session_id().clone();
    let administration = context.administration();
    let Some(closed) = close_session(context).await? else {
        return physically_delete(administration, &session_id)
            .await
            .map(SessionDeletion::Deleted)
            .map_err(|failure| SessionDeleteError::Unrecorded {
                session_id: session_id.clone(),
                failure,
            });
    };
    if !matches!(closed.applied, ControlIntentState::Acknowledged { .. }) {
        return Ok(SessionDeletion::Closing(SessionClosing {
            session_id,
            obligation: None,
            waiting: SessionDeleteWait::CloseIntent(closed.applied),
        }));
    }
    let services = administration.session_close();
    let clock = services.clock.as_ref();
    let obligations = services.deletes.obligations();
    let obligation = match services
        .deletes
        .ledger()
        .delete_obligation(&session_id)
        .await?
    {
        Some(obligation) => obligation,
        // The acknowledgement arms the delete in its own transaction, so an
        // acknowledged close whose row owes nothing has no row: its physical
        // delete ran. A row that is there and owes nothing is armed here.
        None => match obligations
            .arm(
                &ObligationKey::SessionDelete {
                    session_id: session_id.clone(),
                },
                clock.timestamp_ms(),
            )
            .await?
        {
            Some(id) => SessionDeleteObligation {
                id,
                state: ObligationState::Due,
            },
            None => return Ok(SessionDeletion::AlreadyDeleted { session_id }),
        },
    };
    let closing = |waiting| {
        Ok(SessionDeletion::Closing(SessionClosing {
            session_id: session_id.clone(),
            obligation: Some(obligation.id.clone()),
            waiting,
        }))
    };
    if obligation.state != ObligationState::Due {
        return closing(SessionDeleteWait::Obligation(obligation.state));
    }
    let relay = SessionDeleteRelay::new(administration.clone());
    let verdict = deliver_now(&relay, &obligation.id, clock).await?;
    match relay.take_attempt(&obligation.id) {
        Some(DeleteAttempt::Deleted(report)) => Ok(SessionDeletion::Deleted(report)),
        Some(DeleteAttempt::Waiting(cleanup)) => closing(SessionDeleteWait::Cleanup(cleanup)),
        Some(DeleteAttempt::EngineWork) => closing(SessionDeleteWait::EngineWork),
        Some(DeleteAttempt::Failed(failure)) => closing(SessionDeleteWait::Failed(failure)),
        None => {
            // Not attempted: another relay claimed it between the read and
            // the claim.
            debug_assert!(matches!(verdict, RelayVerdict::NotDue));
            let state = obligations
                .state(&obligation.id)
                .await?
                .unwrap_or(ObligationState::Claimed);
            closing(SessionDeleteWait::Obligation(state))
        }
    }
}

/// Physically delete session `session_id`, the last step of its deletion.
/// Every step is idempotent; the storage delete, which removes the row the
/// session's delete obligation lives on, is last.
///
/// # Errors
///
/// The first step that failed; the steps before it stand.
pub async fn physically_delete(
    administration: &SessionAdministration,
    session_id: &SessionId,
) -> Result<SessionDeleteReport, SessionDeleteFailure> {
    let process = match administration.process() {
        Some(process) => Some(
            process
                .registry()
                .delete_session_process_state(session_id)
                .await
                .map_err(|error| SessionDeleteFailure::Process {
                    message: error.to_string(),
                })?,
        ),
        None => None,
    };
    if let Some(triggers) = administration.trigger_store() {
        triggers
            .delete_session_subscriptions(session_id)
            .await
            .map_err(|error| SessionDeleteFailure::Triggers {
                message: error.to_string(),
            })?;
    }
    let host = administration.effect_host();
    host.revoke_await_events_for_session(session_id)
        .await
        .map_err(|error| SessionDeleteFailure::Waits {
            message: error.to_string(),
        })?;
    host.retire_effect_journal(EffectJournalRetirement::session(session_id))
        .await
        .map_err(|error| SessionDeleteFailure::Journal {
            message: error.to_string(),
        })?;
    retire_artifact_owners(administration)
        .await
        .map_err(|error| SessionDeleteFailure::Artifacts {
            message: error.to_string(),
        })?;
    let storage = administration
        .store_factory()
        .delete_session(session_id)
        .await
        .map_err(|failure| SessionDeleteFailure::Storage(Box::new(failure)))?;
    Ok(SessionDeleteReport {
        session_id: session_id.clone(),
        storage,
        process,
    })
}

/// Retire every artifact owner a journal retirement queued.
async fn retire_artifact_owners(
    administration: &SessionAdministration,
) -> Result<(), RuntimeError> {
    let host = administration.effect_host();
    for scope in host.pending_artifact_owner_retirements().await? {
        let owner = crate::ArtifactOwner::execution(scope.clone());
        administration
            .process_env_store()
            .retire_process_execution_env_owner(&owner)
            .await
            .map_err(|error| {
                RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, error.to_string())
            })?;
        administration
            .process_engines()
            .retire_artifact_owner(&owner)
            .await
            .map_err(|error| {
                RuntimeError::new(crate::RuntimeErrorCode::RuntimeStore, error.to_string())
            })?;
        host.complete_artifact_owner_retirement(&scope).await?;
    }
    Ok(())
}

/// What one delivery attempt of a session's delete did.
#[derive(Debug)]
enum DeleteAttempt {
    Deleted(SessionDeleteReport),
    Waiting(SessionCleanup),
    EngineWork,
    Failed(SessionDeleteFailure),
}

/// The `SessionDelete` kind's relay (ADR 0109 §4): its delivery is the
/// session's physical delete, once the session's cleanup has settled.
pub struct SessionDeleteRelay {
    administration: SessionAdministration,
    obligations: Arc<dyn ObligationLedger>,
    ledger: Arc<dyn SessionDeleteLedger>,
    policy: RelayPolicy,
    last: Mutex<Option<(ObligationId, DeleteAttempt)>>,
}

impl SessionDeleteRelay {
    /// The relay over `administration`'s deployment, at the default policy.
    #[must_use]
    pub fn new(administration: SessionAdministration) -> Self {
        Self::with_policy(administration, RelayPolicy::default())
    }

    /// The relay over `administration`'s deployment at `policy`.
    #[must_use]
    pub fn with_policy(administration: SessionAdministration, policy: RelayPolicy) -> Self {
        let deletes = &administration.session_close().deletes;
        let (obligations, ledger) = (deletes.obligations(), deletes.ledger());
        Self {
            administration,
            obligations,
            ledger,
            policy,
            last: Mutex::new(None),
        }
    }

    fn record(&self, id: &ObligationId, attempt: DeleteAttempt) {
        *self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((id.clone(), attempt));
    }

    fn take_attempt(&self, id: &ObligationId) -> Option<DeleteAttempt> {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match last.take() {
            Some((attempted, attempt)) if attempted == *id => Some(attempt),
            other => {
                *last = other;
                None
            }
        }
    }
}

#[async_trait::async_trait]
impl ObligationRelay for SessionDeleteRelay {
    fn ledger(&self) -> &dyn ObligationLedger {
        self.obligations.as_ref()
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
        let ObligationKey::SessionDelete { session_id } = key else {
            return Err(DeliveryFailure::Undecodable(format!(
                "a {} key on the session_delete ledger",
                key.kind()
            )));
        };
        let cleanup = self
            .ledger
            .undelivered_cleanup(session_id)
            .await
            .map_err(|error| DeliveryFailure::Retryable(error.to_string()))?;
        if !cleanup.is_settled() {
            self.record(id, DeleteAttempt::Waiting(cleanup));
            return Err(DeliveryFailure::Retryable(format!(
                "session `{session_id}` waits on its cleanup: {cleanup}"
            )));
        }
        if self
            .administration
            .session_close()
            .work
            .session_work_in_flight(session_id)
            .await
        {
            self.record(id, DeleteAttempt::EngineWork);
            return Err(DeliveryFailure::Retryable(format!(
                "session `{session_id}` waits on its engine work"
            )));
        }
        match physically_delete(&self.administration, session_id).await {
            Ok(report) => {
                self.record(id, DeleteAttempt::Deleted(report));
                Ok(())
            }
            Err(failure) => {
                let message = format!("session `{session_id}` physical delete: {failure}");
                self.record(id, DeleteAttempt::Failed(failure));
                Err(DeliveryFailure::Retryable(message))
            }
        }
    }
}
