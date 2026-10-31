//! Actor-issued authority for one admitted execution and attempt.
use super::AdmittedExecution;
use crate::{ActorContext, EffectOpener, ToolCallId, ToolCancellation};
use lash_durable::domain::OwnerKey;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub(super) struct FinalRequest {
    pub permit: DecisionPermit,
    pub payload: serde_json::Value,
    pub reply: oneshot::Sender<bool>,
    pub protected: CancellationToken,
}

/// An opaque actor-issued permit. Serialized identities cannot construct it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionPermit(Arc<ExecutionAuthority>);

#[derive(Debug, PartialEq, Eq)]
struct ExecutionAuthority {
    execution: AdmittedExecution,
    epoch: lash_durable::Epoch,
    actor: lash_durable::ActorKey,
}

impl DecisionPermit {
    pub(super) fn execution(&self) -> &AdmittedExecution {
        &self.0.execution
    }

    pub(super) fn authorizes(&self, cx: &ActorContext, execution: &AdmittedExecution) -> bool {
        cx.lease_held()
            && !cx.cancel().is_cancelled()
            && self.0.epoch == cx.epoch()
            && &self.0.actor == cx.actor()
            && &self.0.execution == execution
    }
}

/// The runner's decision provenance, independent of its presentation material.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SettlementIntent {
    /// No final decision: a park, retry, interruption or non-tool member.
    #[default]
    Nonterminal,
    /// Commit the final decision, output and state in one fenced transaction.
    PlainFinal(DecisionPermit),
    /// Drain the final the actor already acknowledged before declarations.
    ProtectedFinal(DecisionPermit),
    /// Arbitrate a typed cancellation, even when the cooperative token is live.
    Cancelled {
        permit: DecisionPermit,
        cancellation: Box<ToolCancellation>,
    },
}

/// Task-local transport of a single-use capability. Binding consumes it.
pub(super) struct DecisionScope {
    port: Mutex<Option<BoundDecision>>,
    pub protected: CancellationToken,
    execution_limited: AtomicBool,
}

#[derive(Clone)]
pub(crate) struct BoundDecision(Arc<BoundAuthority>);

struct BoundAuthority {
    permit: DecisionPermit,
    cx: ActorContext,
    sender: mpsc::UnboundedSender<FinalRequest>,
    retained: Mutex<Option<serde_json::Value>>,
    protected: CancellationToken,
}

impl DecisionScope {
    pub(super) fn new(
        cx: ActorContext,
        execution: AdmittedExecution,
        sender: mpsc::UnboundedSender<FinalRequest>,
        retained: Option<serde_json::Value>,
    ) -> Self {
        let protected = CancellationToken::new();
        if retained.is_some() {
            protected.cancel();
        }
        Self {
            port: Mutex::new(Some(BoundDecision(Arc::new(BoundAuthority {
                permit: DecisionPermit(Arc::new(ExecutionAuthority {
                    execution,
                    epoch: cx.epoch(),
                    actor: cx.actor().clone(),
                })),
                cx,
                sender,
                retained: Mutex::new(retained),
                protected: protected.clone(),
            })))),
            protected,
            execution_limited: AtomicBool::new(false),
        }
    }
}

tokio::task_local! {
    pub(super) static DECISION_SCOPE: DecisionScope;
}

// The bounded runner signals its body at a limit so it can finish cleanup.
// That cooperative signal is not an actor-issued cancellation decision.
pub(super) fn mark_execution_limit() {
    let _ = DECISION_SCOPE.try_with(|scope| {
        scope.execution_limited.store(true, Ordering::Relaxed);
    });
}

pub(crate) fn execution_limited() -> bool {
    DECISION_SCOPE
        .try_with(|scope| scope.execution_limited.load(Ordering::Relaxed))
        .unwrap_or(false)
}

impl BoundDecision {
    pub(crate) fn bind(owner: &EffectOpener, call: &ToolCallId) -> Result<Self, String> {
        DECISION_SCOPE
            .try_with(|scope| {
                let mut port = scope.port.lock().unwrap_or_else(PoisonError::into_inner);
                let bound = port
                    .as_ref()
                    .ok_or("the admitted decision capability is already bound")?;
                bound.authenticate(owner, call)?;
                port.take()
                    .ok_or_else(|| "the admitted decision capability is already bound".to_owned())
            })
            .map_err(|_| "a call needs an actor-issued admitted decision capability".to_owned())?
    }

    pub(crate) fn authenticate(
        &self,
        owner: &EffectOpener,
        call: &ToolCallId,
    ) -> Result<(), String> {
        let execution = &self.0.permit.0.execution;
        let owns = match (&execution.id().owner, owner) {
            (
                OwnerKey::Turn(session, turn) | OwnerKey::Cell(session, turn, _),
                EffectOpener::Turn {
                    session_id,
                    turn_id,
                },
            ) => session == session_id && turn == turn_id,
            (OwnerKey::Process(process), EffectOpener::Process { process_id }) => {
                process == process_id
            }
            _ => false,
        };
        if !owns || execution.call() != call || !self.0.permit.authorizes(&self.0.cx, execution) {
            return Err(
                "the decision capability does not authorize this owner, call and live attempt"
                    .to_owned(),
            );
        }
        Ok(())
    }

    pub(crate) fn authenticate_attempt(
        &self,
        owner: &EffectOpener,
        call: &ToolCallId,
        attempt: crate::tool_run::AttemptOrdinal,
    ) -> Result<(), String> {
        self.authenticate(owner, call)?;
        if self.0.permit.0.execution.attempt() != attempt.get() {
            return Err("the decision capability belongs to another admitted attempt".to_owned());
        }
        Ok(())
    }

    pub(crate) fn retained_final(&self) -> Option<serde_json::Value> {
        self.0
            .retained
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn plain_final(&self) -> SettlementIntent {
        SettlementIntent::PlainFinal(self.0.permit.clone())
    }

    pub(crate) fn protected_final(&self) -> SettlementIntent {
        SettlementIntent::ProtectedFinal(self.0.permit.clone())
    }

    pub(crate) fn cancelled(&self, cancellation: ToolCancellation) -> SettlementIntent {
        SettlementIntent::Cancelled {
            permit: self.0.permit.clone(),
            cancellation: Box::new(cancellation),
        }
    }

    /// No declaration is realized until the owning actor acknowledges its fence.
    pub(crate) async fn commit_final(&self, payload: serde_json::Value) -> Result<bool, String> {
        if !self
            .0
            .permit
            .authorizes(&self.0.cx, &self.0.permit.0.execution)
        {
            return Err("the call's actor no longer owns this attempt".to_owned());
        }
        if self.retained_final().is_some() {
            return Ok(true);
        }
        let (reply, receive) = oneshot::channel();
        self.0
            .sender
            .send(FinalRequest {
                permit: self.0.permit.clone(),
                payload: payload.clone(),
                reply,
                protected: self.0.protected.clone(),
            })
            .map_err(|_| "the call's owner stopped before deciding its final")?;
        let won = receive
            .await
            .map_err(|_| "the call's owner stopped before acknowledging its final".to_owned())?;
        if won
            && !self
                .0
                .permit
                .authorizes(&self.0.cx, &self.0.permit.0.execution)
        {
            return Err(
                "the call's actor lost ownership before acknowledging its final".to_owned(),
            );
        }
        if won {
            *self
                .0
                .retained
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(payload);
        }
        Ok(won)
    }
}

pub(super) fn protection() -> Option<CancellationToken> {
    DECISION_SCOPE
        .try_with(|scope| scope.protected.clone())
        .ok()
}

#[cfg(test)]
pub(crate) async fn with_admitted_fixture<F: std::future::Future>(
    call: &crate::tool_dispatch::SingletonToolCall,
    body: F,
) -> F::Output {
    use lash_core_store::tool_run::{Material, MaterialOwner, MaterialRole};
    use lash_durable::domain::{OwnerKey, RunSeq};
    use lash_durable::{
        ActorKey, ActorTx, DurableInstant, Epoch, MailSeq, OpenedActor, StateRevision,
    };
    let EffectOpener::Turn {
        session_id,
        turn_id,
    } = &call.owner
    else {
        panic!("a turn fixture")
    };
    let mut tx = ActorTx::opened(OpenedActor {
        actor: ActorKey::session(session_id.as_str()).unwrap(),
        epoch: Epoch(0),
        revision: StateRevision(0),
        acked: MailSeq(0),
        seen: MailSeq(0),
        mail: Vec::new(),
        at: DurableInstant(0),
        turn_cancel: None,
    });
    let request = Material::journal_local(
        MaterialOwner::Run {
            opener: call.owner.clone(),
        },
        MaterialRole::PreparedRequest,
        "request".to_owned(),
    );
    let execution = super::admit(
        &mut tx,
        &OwnerKey::Turn(session_id.clone(), turn_id.clone()),
        RunSeq(1),
        vec![super::ExecutionDraft::new(
            DurableInstant(0),
            call.call_id.clone(),
            lash_vocabulary::tool_contract::ToolId::new(&call.tool_name),
            request.reference().clone(),
            lash_kernel_value::ExecutionPolicy::Once,
            crate::ExecutionLimit {
                expires_at: 1_000_000,
                max_slice: std::time::Duration::from_secs(60),
            },
            None,
        )],
    )
    .unwrap()
    .remove(0);
    let (sender, _owner) = mpsc::unbounded_channel();
    DECISION_SCOPE
        .scope(
            DecisionScope::new(ActorContext::unavailable(), execution, sender, None),
            body,
        )
        .await
}
