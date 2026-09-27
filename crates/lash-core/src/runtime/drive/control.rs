//! The engine half of a control intent (FIG-3600 S7, ADR 0104 O4, B6).
//!
//! An intent's store half commits in the transaction that records it: a
//! parked root's cancel or fork, or a session's close. What remains is the
//! engine's part — release the roots' executions, close their scopes — which
//! runs after that transaction, idempotently, and is acknowledged. A failure
//! is retained on the intent, and reconciliation re-applies it: the verb that
//! recorded the intent and the reconciler run this one body.
//!
//! It names no engine: the engine's part is a [`SessionControlEngine`], and
//! the scope owner is a [`ScopeCloseSink`].
//!
//! **A failed engine half.** A retryable failure keeps the intent open:
//! reconciliation re-applies it, and while a cancel or fork is open its
//! session admits nothing, so the next root never runs beside the old
//! execution it has yet to release. A permanent failure — the engine refused
//! for good, or the root's evidence is missing — closes the intent
//! `Failed { retryable: false }`: it is surfaced typed on the intent ledger
//! for an operator, and it never wedges the session. Its store half already
//! ended the root and raised the drive epoch past the old execution's fence,
//! so that execution can neither commit nor park; the session drives its
//! next root, and reconciliation closes the ended root's scope as it would
//! an acknowledged verb's.
//!
//! **A redrive** applies only while the root's park still names it; the
//! claim settles one the root ran past (see
//! [`decide_intent_application`](crate::store::decide_intent_application)),
//! and a cancel or fork supersedes every redrive of the root still open, so
//! a redrive never resumes a root after it re-parked or ended.

use super::relay::{ObligationRelay, deliver_now};
use super::scope_close::{ScopeCloseAttempt, deliver_scope_close};
use crate::engine::{EngineRefusal, RootRef, ScopeCloseSink, SessionControlEngine};
use crate::store::{
    ControlIntent, ControlIntentKind, ControlIntentState, IntentApplication, StoreError,
    scope_close_obligation_id,
};
use crate::{Clock, SessionStoreFactory, SessionWorkEngine};

/// Apply the engine half of `intent` (ADR 0104 O4).
///
/// The intent's state is re-read in a store transaction first
/// ([`claim_intent_application`](crate::store::ControlIntentStore::claim_intent_application)),
/// so an intent a later one superseded, or one already acknowledged, runs
/// nothing and answers its state. Otherwise the engine half runs and is
/// acknowledged:
///
/// - `CloseSession { roots }`: release each root's execution, then close the
///   session's scope and the roots'.
///
/// `scope_close` is the `ScopeClose` kind's relay when the host wires its
/// ledger (ADR 0109 §3): a released root's close is then its obligation's
/// immediate delivery, which the ledger owns from there. `None` on a host
/// without an obligation substrate; its closes reach `scopes` directly.
///
/// An engine refusal or a scope-close failure is retained on the intent
/// (retryable unless the engine refused permanently) and answered as its
/// state: the store half stands, and reconciliation finishes the rest. Only
/// a store that did not answer is an `Err`.
pub async fn apply_control_intent(
    stores: &dyn SessionStoreFactory,
    engine: &dyn SessionControlEngine,
    work: &dyn SessionWorkEngine,
    scopes: &dyn ScopeCloseSink,
    scope_close: Option<&dyn ObligationRelay>,
    intent: &ControlIntent,
    clock: &dyn Clock,
) -> Result<ControlIntentState, StoreError> {
    let intent = match stores
        .claim_intent_application(intent.id, clock.timestamp_ms())
        .await?
    {
        IntentApplication::Apply(intent) => intent,
        IntentApplication::Superseded(intent) | IntentApplication::Done(intent) => {
            return Ok(intent.state);
        }
    };
    let applied = match &intent.kind {
        ControlIntentKind::CloseSession { roots } => {
            close_session_engine_half(engine, scopes, scope_close, &intent, roots, clock).await
        }
        ControlIntentKind::Redrive { root, .. } => {
            let target = RootRef {
                session: intent.session_id.clone(),
                root: root.clone(),
            };
            match engine.resume_root(&target, intent.engine.as_ref()).await {
                Ok(crate::engine::EngineAck::NothingHeld) => {
                    work.schedule_drive(
                        &intent.session_id,
                        crate::engine::DriveRequestId::new(format!("intent:{}", intent.id)),
                    );
                    Ok(())
                }
                Ok(_) => Ok(()),
                Err(error) => Err(error.into()),
            }
        }
        ControlIntentKind::Cancel { root, .. } | ControlIntentKind::Fork { root, .. } => {
            release_root_engine_half(stores, engine, scopes, scope_close, &intent, root, clock)
                .await
        }
    };
    match applied {
        Ok(()) => {
            let at_ms = clock.timestamp_ms();
            stores.acknowledge_intent(intent.id, at_ms).await?;
            let state = stores
                .load_intent(intent.id)
                .await?
                .ok_or(StoreError::ControlIntentUnknown { intent: intent.id })?
                .state;
            if matches!(state, ControlIntentState::Acknowledged { .. })
                && matches!(
                    intent.kind,
                    ControlIntentKind::Cancel { .. } | ControlIntentKind::Fork { .. }
                )
            {
                work.schedule_drive(
                    &intent.session_id,
                    crate::engine::DriveRequestId::new(format!("intent:{}", intent.id)),
                );
            }
            Ok(state)
        }
        Err(failure) => {
            let failed = stores
                .record_intent_failure(
                    intent.id,
                    &failure.message,
                    failure.retryable,
                    clock.timestamp_ms(),
                )
                .await?;
            Ok(failed.state)
        }
    }
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
/// cleanup and never decides an outcome.
async fn close_session_engine_half(
    engine: &dyn SessionControlEngine,
    scopes: &dyn ScopeCloseSink,
    scope_close: Option<&dyn ObligationRelay>,
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
    if let Some(relay) = scope_close {
        for root in roots {
            let id = scope_close_obligation_id(&intent.session_id, root);
            if let Err(error) = deliver_now(relay, &id, clock).await {
                tracing::warn!(
                    session_id = intent.session_id.as_str(),
                    root = root.as_str(),
                    error = %error,
                    "the closed root's scope-close obligation missed its immediate \
                     delivery; the due pass owns it"
                );
            }
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

async fn release_root_engine_half(
    stores: &dyn SessionStoreFactory,
    engine: &dyn SessionControlEngine,
    scopes: &dyn ScopeCloseSink,
    scope_close: Option<&dyn ObligationRelay>,
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
        // The armed obligation owns the retry, but the intent's engine half
        // still records that the close did not land when it ran — its
        // failure is retained and retried like any engine-half miss.
        ScopeCloseAttempt::Owed { retryable } => Err(EngineHalfFailure {
            message: format!(
                "root scope close left to its obligation ({})",
                if retryable { "retryable" } else { "stalled" }
            ),
            retryable,
        }),
    }
}
