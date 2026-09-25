//! The engine half of a control intent (FIG-3600 S7, ADR 0104 O4, astra B6).
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

use crate::engine::{EngineRefusal, RootRef, ScopeCloseSink, SessionControlEngine};
use crate::store::{
    ControlIntent, ControlIntentKind, ControlIntentState, IntentApplication, StoreError,
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
/// An engine refusal or a scope-close failure is retained on the intent
/// (retryable unless the engine refused permanently) and answered as its
/// state: the store half stands, and reconciliation finishes the rest. Only
/// a store that did not answer is an `Err`.
pub async fn apply_control_intent(
    stores: &dyn SessionStoreFactory,
    engine: &dyn SessionControlEngine,
    work: &dyn SessionWorkEngine,
    scopes: &dyn ScopeCloseSink,
    intent: &ControlIntent,
    clock: &dyn Clock,
) -> Result<ControlIntentState, StoreError> {
    let intent = match stores.claim_intent_application(intent.id).await? {
        IntentApplication::Apply(intent) => intent,
        IntentApplication::Superseded(intent) | IntentApplication::Done(intent) => {
            return Ok(intent.state);
        }
    };
    let applied = match &intent.kind {
        ControlIntentKind::CloseSession { roots } => {
            close_session_engine_half(engine, scopes, &intent, roots).await
        }
        ControlIntentKind::Redrive { .. }
        | ControlIntentKind::Cancel { .. }
        | ControlIntentKind::Fork { .. } => {
            let _ = work;
            return Err(StoreError::UnsupportedStoreOperation {
                operation: "apply_control_intent: the parked-root verbs",
            });
        }
    };
    match applied {
        Ok(()) => {
            let at_ms = clock.timestamp_ms();
            stores.acknowledge_intent(intent.id, at_ms).await?;
            Ok(ControlIntentState::Acknowledged { at_ms })
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
    intent: &ControlIntent,
    roots: &[crate::TurnId],
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
    scopes
        .close_session_scope(&intent.session_id, intent.id, roots)
        .await
        .map_err(|error| EngineHalfFailure {
            message: format!("session scope close: {error}"),
            retryable: true,
        })
}
