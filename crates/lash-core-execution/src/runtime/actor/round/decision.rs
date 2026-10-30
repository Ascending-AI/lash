//! The actor commits a final before its body may realize declarations.
use super::AdmittedExecution;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub(super) struct FinalRequest {
    pub execution: AdmittedExecution,
    pub payload: serde_json::Value,
    pub reply: oneshot::Sender<bool>,
    pub protected: CancellationToken,
}

pub(super) struct FinalScope {
    pub execution: AdmittedExecution,
    pub sender: mpsc::UnboundedSender<FinalRequest>,
    pub state: std::sync::Mutex<FinalState>,
    pub protected: CancellationToken,
}

pub(super) enum FinalState {
    Open,
    AtOutcome,
    Protected(serde_json::Value),
}

impl FinalScope {
    pub(super) fn at_outcome(&self) -> bool {
        matches!(
            *self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            FinalState::AtOutcome
        )
    }
}

tokio::task_local! {
    pub(super) static FINAL_SCOPE: Arc<FinalScope>;
}

pub(crate) fn retained_final(call: &crate::ToolCallId) -> Option<serde_json::Value> {
    FINAL_SCOPE
        .try_with(|scope| {
            if scope.execution.call() != call {
                return None;
            }
            match &*scope
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                FinalState::Protected(payload) => Some(payload.clone()),
                FinalState::Open | FinalState::AtOutcome => None,
            }
        })
        .ok()
        .flatten()
}

pub(crate) fn final_at_outcome(call: &crate::ToolCallId) -> Result<(), String> {
    let Ok(scope) = FINAL_SCOPE.try_with(Arc::clone) else {
        return Ok(());
    };
    if scope.execution.call() != call {
        return Err("a nested call needs its own admitted decision owner".to_owned());
    }
    let mut state = scope
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if matches!(*state, FinalState::Open) {
        *state = FinalState::AtOutcome;
    }
    Ok(())
}

pub(super) fn protection() -> Option<CancellationToken> {
    FINAL_SCOPE.try_with(|scope| scope.protected.clone()).ok()
}

/// Wait for the owner's acknowledged decision. A dropped owner grants nothing.
pub(crate) async fn commit_final(
    call: &crate::ToolCallId,
    payload: serde_json::Value,
) -> Result<bool, String> {
    let Ok(scope) = FINAL_SCOPE.try_with(Arc::clone) else {
        return Ok(true);
    };
    if scope.execution.call() != call {
        return Err("a nested call needs its own admitted decision owner".to_owned());
    }
    if matches!(
        *scope
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        FinalState::Protected(_)
    ) {
        return Ok(true);
    }
    let (reply, receive) = oneshot::channel();
    scope
        .sender
        .send(FinalRequest {
            execution: scope.execution.clone(),
            payload: payload.clone(),
            reply,
            protected: scope.protected.clone(),
        })
        .map_err(|_| "the call's owner stopped before deciding its final")?;
    let won = receive
        .await
        .map_err(|_| "the call's owner stopped before acknowledging its final".to_owned())?;
    if won {
        *scope
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = FinalState::Protected(payload);
    }
    Ok(won)
}
