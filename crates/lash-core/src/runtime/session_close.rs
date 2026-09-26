//! A session's durable close, the point of no return of its deletion
//! (FIG-3600 S7, FIG-3607 item 7, ADR 0104 O4).
//!
//! A session's close is the one control intent whose store half is itself a
//! recorded step: [`close_session`] asks every refusal of a deletion, records
//! `BeginSessionClose` under the session's `SessionDelete` scope, and then
//! applies the intent it answered through
//! [`apply_control_intent`](crate::drive::apply_control_intent), the one
//! engine-half body the verbs and reconciliation also run.
//!
//! It is deletion code, not the session drive: it runs under the deletion's
//! `SessionDelete` scope, never a drive's. It names no engine: the engine's
//! part is the work engine's [`SessionControlEngine`](crate::engine::SessionControlEngine),
//! and the scope owner is a [`ScopeCloseSink`].

use std::sync::Arc;

use crate::drive::apply_control_intent;
use crate::engine::{ScopeCloseSink, begin_session_close_replay_key};
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::store::{ControlIntent, ControlIntentState, StoreError};
use crate::{
    Clock, EffectAddress, ExecutionScope, RuntimeAttribution, RuntimeEffectCommand,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, RuntimeErrorCode,
    SessionDeleteContext, SessionId, SessionStoreFactory, SessionWorkEngine,
};

/// What a session's close runs against besides its catalog: the engine whose
/// executions it releases, the owner of the scopes it closes, and the clock
/// its intent is stamped by.
#[derive(Clone)]
pub struct SessionCloseServices {
    pub work: Arc<dyn SessionWorkEngine>,
    /// The owner of lifetime scopes. [`NoScopeClose`](crate::engine::NoScopeClose)
    /// until the process registry's scope-close adapter is installed
    /// (FIG-3607 PR-2).
    pub scopes: Arc<dyn ScopeCloseSink>,
    pub clock: Arc<dyn Clock>,
}

/// A session's close, as its deletion saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionClosed {
    /// The session's `CloseSession` intent, as its recorded step answered it.
    pub intent: ControlIntent,
    /// Where its engine half stands after this attempt: `Acknowledged`, or
    /// retained (`Pending` when the ledger did not answer, `Failed`
    /// otherwise) for reconciliation to finish.
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
///    closing (ADR 0099 §7 / W16).
/// 2. The recorded `BeginSessionClose` step runs the close's store half
///    ([`ControlIntentStore::begin_session_close`](crate::store::ControlIntentStore::begin_session_close)):
///    every root ends, the session stops accepting and admitting, and the
///    `CloseSession` intent is recorded. This is the point of no return:
///    after it the deletion only retries, and a retried deletion replays the
///    recorded step.
/// 3. [`apply_control_intent`] runs its engine half: every closed root's
///    execution is released and the session's scope is closed. A failure is
///    retained on the intent and never fails the close: reconciliation
///    finishes it, so the deletion continues.
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
    let pins = stores.pending_turn_cancel_closure_pins(session_id).await?;
    if !pins.is_empty() {
        return Err(StoreError::TurnCancelClosureLifecyclePinned {
            session_id: session_id.clone(),
            pending_count: pins.len(),
        }
        .into());
    }
    // ADR 0099 §7 / W16: an accepted or closing effect group keeps its
    // session until it settles. The journal retirement after the close
    // refuses the same pins, but only after the session is closed.
    if let Some(closing) = administration.effect_host().effect_group_closing() {
        let group_pins = closing
            .read_session_pins(session_id)
            .await
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        if let Some(first) = group_pins.first() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::EffectGroupLifecyclePinned,
                format!(
                    "session `{session_id}` still owns {} effect group(s) that are live or \
                     closing (first: `{first}`); session deletion is refused until they settle",
                    group_pins.len()
                ),
            )
            .into());
        }
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
    let engine = services.work.control();
    let applied = match apply_control_intent(
        stores.as_ref(),
        engine.as_ref(),
        services.work.as_ref(),
        services.scopes.as_ref(),
        &intent,
        services.clock.as_ref(),
    )
    .await
    {
        Ok(state) => state,
        Err(error) => {
            // The ledger did not answer: the intent stays open, and
            // reconciliation re-applies it.
            tracing::warn!(
                session_id = session_id.as_str(),
                intent = %intent.id,
                error = %error,
                "the session close's engine half is left to reconciliation"
            );
            intent.state.clone()
        }
    };
    Ok(Some(SessionClosed { intent, applied }))
}

/// The first execution of one `BeginSessionClose` step. A replay decodes the
/// recorded intent and never runs it.
struct BeginSessionCloseRunner {
    stores: Arc<dyn SessionStoreFactory>,
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
