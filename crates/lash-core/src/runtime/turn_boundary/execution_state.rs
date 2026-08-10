//! How a turn's protocol-owned execution-state capture is probed, taken,
//! applied, and settled.

use crate::{PluginSession, Session, SessionError, StoreError};

use super::RuntimeSessionState;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExecutionStateUpdate {
    Clean,
    Replace(crate::plugin::ExecutionStateSnapshot),
    Clear,
}

impl ExecutionStateUpdate {
    pub(super) fn apply(self, state: &mut RuntimeSessionState) -> Result<(), StoreError> {
        match self {
            Self::Clean => {}
            Self::Replace(snapshot) => state.set_execution_state_components(snapshot)?,
            Self::Clear => state.set_execution_state_snapshot(None),
        }
        Ok(())
    }
}

/// Take the turn's one execution-state capture. Called only from the final
/// commit: a capture staged anywhere else would be speculative, because no
/// earlier boundary writes to the store.
pub(super) async fn capture_execution_state_update(
    session: &mut Session,
) -> Result<ExecutionStateUpdate, SessionError> {
    let Some(code_executor) = session.plugins().code_executor() else {
        return Ok(ExecutionStateUpdate::Clean);
    };
    if !code_executor.execution_state_dirty() {
        return Ok(ExecutionStateUpdate::Clean);
    }
    let session_id = session.session_id().to_string();
    let snapshot = code_executor
        .snapshot_execution_state(crate::plugin::ProtocolSessionContext::new(
            session,
            &session_id,
        ))
        .await?;
    Ok(if snapshot.root.is_some() {
        ExecutionStateUpdate::Replace(snapshot)
    } else {
        ExecutionStateUpdate::Clear
    })
}

/// Ask whether the turn's eventual capture would fail, *before* the turn spends
/// a provider round trip.
///
/// Because the final commit is the only capture boundary, a dirty-capture
/// failure discovered there has already burned the model call and the turn's
/// tool work, and the successful response it aborts is thrown away — the retry
/// then asks the provider for the next response instead of reusing it. Every
/// prompt-resume-safe boundary that precedes a provider call therefore asks the
/// executor whether the capture is possible and fails the turn there instead.
/// The probe stages no checkpoint state, so this keeps the "only the final
/// commit captures" rule intact.
pub(super) async fn probe_execution_state_capture(
    session: &mut Session,
) -> Result<(), SessionError> {
    let Some(code_executor) = session.plugins().code_executor() else {
        return Ok(());
    };
    if !code_executor.execution_state_dirty() {
        return Ok(());
    }
    let session_id = session.session_id().to_string();
    code_executor
        .probe_execution_state_capture(crate::plugin::ProtocolSessionContext::new(
            session,
            &session_id,
        ))
        .await
}

pub(super) async fn settle_execution_state_capture(
    plugins: Option<&PluginSession>,
    captured: bool,
    committed: bool,
) {
    let Some(code_executor) = captured
        .then_some(plugins)
        .flatten()
        .and_then(PluginSession::code_executor)
    else {
        return;
    };
    if committed {
        code_executor.acknowledge_execution_state_capture().await;
    } else {
        code_executor.abort_execution_state_capture().await;
    }
}
