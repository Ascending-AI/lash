//! A session's durable close, the point of no return of its deletion
//! (FIG-3600 S7, FIG-3607 item 7, ADR 0104 O4).
//!
//! A session's close is the one control intent whose store half is itself a
//! recorded step: [`close_session`] asks every refusal of a deletion, records
//! `BeginSessionClose` under the session's `SessionDelete` scope, whose
//! store half arms the intent's `ControlIntent` obligation (ADR 0109), and
//! then delivers it through [`ControlIntentRelay`], the one engine-half body
//! the verbs and the reconcile tick's relay pass also run.
//!
//! It is deletion code, not the session drive: it runs under the deletion's
//! `SessionDelete` scope, never a drive's. It names no engine: the engine's
//! part is the work engine's [`SessionControlEngine`](crate::engine::SessionControlEngine),
//! and the scope owner is a [`ScopeCloseSink`].

use std::sync::Arc;

use crate::drive::ControlIntentRelay;
use crate::drive::relay::ObligationRelay;
use crate::engine::{ScopeCloseSink, begin_session_close_replay_key};
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::store::{ControlIntent, ControlIntentState, StoreError};
use crate::{
    Clock, DeploymentStore, EffectAddress, ExecutionScope, RuntimeAttribution,
    RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError,
    RuntimeErrorCode, SessionDeleteContext, SessionId, SessionWorkEngine,
};

/// What a session's close runs against besides its catalog: the engine whose
/// executions it releases, the owner of the scopes it closes, the ledger its
/// engine half is delivered through, and the clock its intent is stamped by.
#[derive(Clone)]
pub struct SessionCloseServices {
    pub work: Arc<dyn SessionWorkEngine>,
    /// The owner of lifetime scopes. [`NoScopeClose`](crate::engine::NoScopeClose)
    /// until the process registry's scope-close adapter is installed
    /// (FIG-3607 PR-2).
    pub scopes: Arc<dyn ScopeCloseSink>,
    /// The `ScopeClose` kind's relay (ADR 0109 §3): each closed root's
    /// obligation gets its immediate delivery here.
    pub scope_close_obligations: Arc<dyn ObligationRelay>,
    /// The store set's `ControlIntent` obligation ledger (ADR 0109).
    pub intents: Arc<dyn crate::store::ObligationLedger>,
    pub clock: Arc<dyn Clock>,
    /// The session-delete obligation's stores: the close's acknowledgement
    /// arms it, and the delete's relay delivers it (ADR 0109 §4).
    pub deletes: crate::session_delete::SessionDeleteStores,
}

/// Whether session `session_id` is already closing: its close committed,
/// under the `CloseSession` intent its stored drive epoch names.
async fn session_is_closing(
    stores: &dyn DeploymentStore,
    session_id: &SessionId,
) -> Result<bool, StoreError> {
    match stores.lookup_session(session_id).await? {
        crate::store::SessionLookup::Live(_) => {
            Ok(stores.drive_epoch(session_id).await?.closing.is_some())
        }
        crate::store::SessionLookup::Deleted | crate::store::SessionLookup::Absent => Ok(false),
    }
}

/// A session's close, as its deletion saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionClosed {
    /// The session's `CloseSession` intent, as its recorded step answered it.
    pub intent: ControlIntent,
    /// Where its engine half stands after this attempt: `Acknowledged`, or
    /// retained (`Pending` when the ledger did not answer, `Failed`
    /// otherwise) for its obligation's relay to finish.
    pub applied: ControlIntentState,
}

/// Why a session's close did not begin: a refusal, asked before anything
/// was closed, or a fault. Either way nothing was closed and the caller may
/// retry.
#[derive(Debug, thiserror::Error)]
pub enum SessionCloseError {
    /// The store refused, or did not answer (among them the turn-cancel
    /// closure pins, `StoreError::TurnCancelClosureLifecyclePinned`).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The runtime refused (among them the effect-group pins,
    /// `EffectGroupLifecyclePinned`), or its recorded step failed.
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}

/// Close the session `context` deletes (FIG-3600 S7, FIG-3607 item 7).
///
/// 1. Every refusal first, with nothing closed yet: a pending turn-cancel
///    closure pins the session, and so does an effect group that is live or
///    closing (ADR 0099 §7 / W16). A deletion retried after its close
///    committed asks none of them: it only replays the step below.
/// 2. The recorded `BeginSessionClose` step runs the close's store half
///    ([`ControlIntentStore::begin_session_close`](crate::store::ControlIntentStore::begin_session_close)):
///    every root ends, the session stops accepting and admitting, and the
///    `CloseSession` intent is recorded. This is the point of no return:
///    after it the deletion only retries, and a retried deletion replays the
///    recorded step.
/// 3. [`ControlIntentRelay`] delivers its engine half now: every closed
///    root's execution is released and the session's scope is closed. A
///    failure is retained on the intent and its obligation and never fails
///    the close: the obligation's relay finishes it. The acknowledgement
///    arms the session's physical delete
///    ([`session_delete`](crate::session_delete), ADR 0109 §4), so the
///    delete waits for it.
///
/// `None` when the session had no durable record: nothing was closed.
pub async fn close_session(
    context: &SessionDeleteContext<'_>,
) -> Result<Option<SessionClosed>, SessionCloseError> {
    let session_id = context.session_id();
    let administration = context.administration();
    let controller = context.controller();
    let stores = administration.store_factory();
    let services = administration.session_close();
    match controller.execution_scope() {
        ExecutionScope::SessionDelete { session_id: scoped } if scoped == session_id => {}
        _ => {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SessionDeleteScopeMismatch,
                "session deletion requires a matching SessionDelete scope",
            )
            .into());
        }
    }
    // The refusals below are asked before anything is closed. A deletion
    // retried after its close committed is past its point of no return: it
    // replays the recorded step, and a pin found now is one its close
    // superseded (a turn whose final commit the close cut short), which the
    // physical delete retires with the session's storage. Refusing it here
    // would wedge the deletion for good, and answer a replay differently
    // from the run that recorded the step.
    let closed = session_is_closing(stores.as_ref(), session_id).await?;
    let pins = if closed {
        Vec::new()
    } else {
        stores.pending_turn_cancel_closure_pins(session_id).await?
    };
    if !pins.is_empty() {
        return Err(StoreError::TurnCancelClosureLifecyclePinned {
            session_id: session_id.clone(),
            pending_count: pins.len(),
        }
        .into());
    }
    let invocation = RuntimeEffectInvocation::new(
        EffectAddress::new(
            controller.execution_scope().clone(),
            begin_session_close_replay_key(session_id),
        )
        .map_err(RuntimeError::from)?,
        RuntimeAttribution::for_session(session_id.clone()),
        format!("session-close:{session_id}"),
    );
    let intent = controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::BeginSessionClose {
                    session: session_id.clone(),
                },
            ),
            RuntimeEffectLocalExecutor::owned_runner(
                Box::new(BeginSessionCloseRunner {
                    stores: Arc::clone(stores),
                    session_id: session_id.clone(),
                    clock: Arc::clone(&services.clock),
                }),
                None,
            ),
        )
        .await
        .and_then(RuntimeEffectOutcome::into_begin_session_close)
        .map_err(RuntimeEffectControllerError::into_runtime_error)?;
    let Some(intent) = intent else {
        return Ok(None);
    };
    let relay = ControlIntentRelay::new(
        Arc::clone(&services.intents),
        Arc::clone(stores),
        Arc::clone(&services.work),
        Arc::clone(&services.scopes),
        Arc::clone(&services.scope_close_obligations),
        Arc::clone(&services.clock),
    );
    let applied = match relay.deliver_intent(&intent).await {
        Ok(state) => state,
        Err(error) => {
            // The ledger did not answer: the intent stays open, and its
            // obligation's relay delivers it.
            tracing::warn!(
                session_id = session_id.as_str(),
                intent = %intent.id,
                error = %error,
                "the session close's engine half is left to its obligation's relay"
            );
            intent.state.clone()
        }
    };
    Ok(Some(SessionClosed { intent, applied }))
}

/// The first execution of one `BeginSessionClose` step. A replay decodes the
/// recorded intent and never runs it.
struct BeginSessionCloseRunner {
    stores: Arc<dyn DeploymentStore>,
    session_id: SessionId,
    clock: Arc<dyn Clock>,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for BeginSessionCloseRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::BeginSessionClose { session } = &envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "session close executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if *session != self.session_id {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "session close executor was bound to another session",
            ));
        }
        // A store that did not answer is this attempt's fault, never the
        // step's outcome: the engine runs the step again.
        let intent = self
            .stores
            .begin_session_close(&self.session_id, self.clock.timestamp_ms())
            .await
            .map_err(|error| {
                let mut fault = RuntimeEffectControllerError::from(
                    crate::runtime::runtime_error_from_store_commit(error),
                );
                fault.message = format!("session close: {}", fault.message);
                fault.retryable_uncommitted_derivation()
            })?;
        Ok(RuntimeEffectOutcome::BeginSessionClose {
            intent: intent.map(Box::new),
        })
    }
}
