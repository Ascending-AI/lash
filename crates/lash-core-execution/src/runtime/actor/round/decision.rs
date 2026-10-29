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
    pub retained: std::sync::Mutex<Option<serde_json::Value>>,
    pub protected: CancellationToken,
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
            scope
                .retained
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
        .ok()
        .flatten()
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
    if scope
        .retained
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
    {
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
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(payload);
    }
    Ok(won)
}
