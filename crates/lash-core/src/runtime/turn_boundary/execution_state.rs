//! How a turn's protocol-owned execution-state capture is probed, taken,
//! applied, and settled, and what a frame open carries out of the frame it
//! leaves.

use crate::SessionId;
use crate::{PluginSession, Session, SessionError, StoreError};

use super::RuntimeSessionState;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExecutionStateUpdate {
    Clean,
    Replace(crate::plugin::ExecutionStateSnapshot),
    /// The execution state is wiped. On a committed frame switch `carries`
    /// names the artifacts the switch hands to the successor frame
    /// (ADR 0113 §3.1); every other clear carries none.
    Clear {
        carries: SeedCarries,
    },
}

impl ExecutionStateUpdate {
    pub(super) fn apply(self, state: &mut RuntimeSessionState) -> Result<(), StoreError> {
        match self {
            Self::Clean => {}
            Self::Replace(snapshot) => state.set_execution_state_components(snapshot)?,
            Self::Clear { .. } => state.set_execution_state_snapshot(None),
        }
        Ok(())
    }

    /// The artifacts a clear carries into the successor frame.
    pub(super) fn carries(&self) -> SeedCarries {
        match self {
            Self::Clear { carries } => carries.clone(),
            Self::Clean | Self::Replace(_) => SeedCarries::none(),
        }
    }
}

/// The artifacts a frame open carries out of the frame it leaves into the
/// frame it opens (ADR 0113 §3.1): exactly what the code executor finds in
/// the new frame's seed. Every frame author derives them the same way,
/// through [`derive_seed_carries`], whatever wrote the seed: a
/// context-pressure hook, overflow recovery, `/compact` or `continue_as`
/// (FIG-4134). A protocol-specific seed (an RLM seed holding a module-backed
/// value) therefore keeps its artifacts alive in the successor frame, and
/// no author can hand a frame transition carries its seed did not name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::runtime) struct SeedCarries(Vec<crate::ArtifactName>);

impl SeedCarries {
    /// What a commit that opens no frame carries: nothing.
    pub(in crate::runtime) fn none() -> Self {
        Self(Vec::new())
    }

    #[cfg(test)]
    pub(in crate::runtime) fn from_names(names: Vec<crate::ArtifactName>) -> Self {
        Self(names)
    }

    fn into_names(self) -> Vec<crate::ArtifactName> {
        self.0
    }
}

/// The one carry derivation every frame open goes through (FIG-4134): the
/// artifacts the session's code executor finds in `seed`, the new frame's
/// initial nodes. A session with no code executor carries nothing.
pub(in crate::runtime) async fn derive_seed_carries(
    session: Option<&mut Session>,
    seed: &[crate::SessionAppendNode],
) -> Result<SeedCarries, SessionError> {
    let Some(session) = session else {
        return Ok(SeedCarries::none());
    };
    let Some(code_executor) = session.plugins().code_executor() else {
        return Ok(SeedCarries::none());
    };
    let session_id = session.session_id().to_string();
    let carries = code_executor
        .frame_switch_carries(
            crate::plugin::ProtocolSessionContext::new(session, &SessionId::from(session_id)),
            seed,
        )
        .await?;
    Ok(SeedCarries(carries))
}

/// The clear a committed frame switch makes: the frame's globals are wiped,
/// and only the artifacts the code executor finds in the switch's seed
/// `initial_nodes` are carried into the successor frame (ADR 0113 §3.1).
pub(super) async fn frame_switch_execution_state_update(
    session: &mut Session,
    initial_nodes: &[crate::SessionAppendNode],
) -> Result<ExecutionStateUpdate, SessionError> {
    Ok(ExecutionStateUpdate::Clear {
        carries: derive_seed_carries(Some(session), initial_nodes).await?,
    })
}

/// The artifact half of a commit that moves the session from frame `ended`
/// to the state's current frame (ADR 0113 §3.1), gated on `committing`: the
/// one execution that can still read what `ended` held.
///
/// The store ends every frame the commit leaves: the committed head's frame
/// and each frame whose open this same commit appends (`appended` names the
/// commit's nodes), except the successor (ADR 0113 §3.1, Lane G amendment).
/// A switch names the frame its turn was admitted on and carries its seed's
/// modules out of it when that frame is one the commit leaves: the frame the
/// head holds, or a frame opened in resident state since the last commit,
/// including a session's first frame. Otherwise (no switch) the transition
/// names the last committed frame and carries nothing; the store still ends
/// every other frame the commit leaves. `None` when the commit opens no
/// frame, or when there was no frame to end.
///
/// # Errors
///
/// A committing scope with no journal identity.
pub(in crate::runtime) fn committed_frame_transition(
    state: &RuntimeSessionState,
    ended: Option<crate::FrameNodeId>,
    carries: SeedCarries,
    committing: &crate::ExecutionScope,
    appended: &[crate::NodeId],
) -> Result<Option<crate::store::FrameTransition>, StoreError> {
    let Some(successor) = state.current_frame_node_id.clone() else {
        return Ok(None);
    };
    let committed = last_committed_frame(state);
    let endable = |ended: &crate::FrameNodeId| {
        is_committed(state, ended)
            || appended
                .iter()
                .any(|node_id| node_id.as_str() == ended.as_str())
    };
    let (ended, carries) = match ended.filter(endable) {
        Some(ended) => (ended, carries.into_names()),
        None => match committed {
            Some(ended) => (ended, Vec::new()),
            None => return Ok(None),
        },
    };
    if ended == successor {
        return Ok(None);
    }
    let gate = committing.journal_identity().map_err(|error| {
        StoreError::Backend(format!(
            "a frame switch commit needs its execution's journal identity: {error}"
        ))
    })?;
    Ok(Some(crate::store::FrameTransition {
        ended: crate::FrameEnvironmentId::new(state.session_id.clone(), ended),
        successor: crate::FrameEnvironmentId::new(state.session_id.clone(), successor),
        carries,
        gate,
    }))
}

/// The newest frame on the current frame's lineage that the store already
/// holds: the current frame itself unless a frame was opened in resident
/// state since the last commit.
fn last_committed_frame(state: &RuntimeSessionState) -> Option<crate::FrameNodeId> {
    let mut frame = state.current_frame_node_id.clone()?;
    loop {
        if is_committed(state, &frame) {
            return Some(frame);
        }
        frame = state
            .agent_frames
            .iter()
            .find(|record| record.frame_node_id == frame)?
            .previous_frame_node_id
            .clone()?;
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
            &SessionId::from(session_id),
        ))
        .await?;
    Ok(if snapshot.root.is_some() {
        ExecutionStateUpdate::Replace(snapshot)
    } else {
        ExecutionStateUpdate::Clear {
            carries: SeedCarries::none(),
        }
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
            &SessionId::from(session_id),
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

/// Whether the store already holds `frame`'s open.
fn is_committed(state: &RuntimeSessionState, frame: &crate::FrameNodeId) -> bool {
    state
        .persisted_node_ids
        .contains(&crate::NodeId::new(frame.as_str().to_string()))
}
