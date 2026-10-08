//! The one way a runtime opens a frame (FIG-4110 F1, FIG-4134).
//!
//! Every frame author opens through [`LashRuntime::open_frame`] or, for a
//! turn's own `continue_as`, through the turn's final fold, and both reach
//! the store primitive `open_agent_frame_in_state_with_clock`. An open:
//!
//! - derives what its seed carries out of the frame it leaves through the one
//!   carry derivation ([`derive_seed_carries`]), so a protocol-specific seed
//!   keeps its artifacts alive in the successor frame, whoever wrote it;
//! - resets the stored execution state and the prompt usage (the store
//!   primitive);
//! - resets the live interpreter from the new frame's seed through
//!   [`LashRuntime::restore_protocol_session_after_frame_open`], once the open
//!   is accepted: after its commit when it commits, at once on a storeless
//!   runtime, which has nothing to commit.
//!
//! A store-backed session's host opens are session commands its shift
//! applies at a turn boundary and commits with their settlement (FIG-4202).
//!
//! Initial-frame construction (`ensure_agent_frame_initialized_with_clock`,
//! `reset_initial_agent_frame_with_clock`) is the bootstrap exception: it
//! opens a session's first frame before any execution state exists, so there
//! is nothing to carry and nothing live to reset.

use super::*;
use crate::runtime::turn_boundary::{SeedCarries, derive_seed_carries};

/// A frame an author opened in resident state, with what its commit needs.
#[must_use = "an opened frame is committed with its carries, then the live state is reset"]
pub(in crate::runtime) struct OpenedFrame {
    pub(in crate::runtime) result: crate::OpenAgentFrameOutcome,
    /// The frame current when it opened: the one it leaves.
    pub(in crate::runtime) ended: Option<crate::FrameNodeId>,
    /// What its seed carries out of `ended` (ADR 0113 §3.1).
    pub(in crate::runtime) carries: SeedCarries,
}

impl LashRuntime {
    /// Opens `request`'s frame in resident state: the carries its seed names
    /// are derived first, then the store primitive opens the frame, resets
    /// the stored execution state and the prompt usage, and seeds it. The
    /// caller commits the open with [`OpenedFrame::carries`] and then resets
    /// the live state.
    pub(in crate::runtime) async fn open_frame(
        &mut self,
        request: crate::OpenAgentFrameRequest,
    ) -> Result<OpenedFrame, RuntimeError> {
        let successor =
            crate::session_graph::frame_node_id(&self.state.session_id, request.frame_key.as_str());
        let carries =
            derive_seed_carries(self.session.as_mut(), &successor, &request.initial_nodes)
                .await
                .map_err(|error| {
                    RuntimeError::new(
                        RuntimeErrorCode::ExecutionStateCaptureFailed,
                        format!("failed to derive what a frame's seed carries: {error}"),
                    )
                })?;
        let ended = self.state.current_frame_node_id.clone();
        let result = open_agent_frame_in_state_with_clock(
            &mut self.state,
            request,
            self.host.core.clock.as_ref(),
        )?;
        if result.opened {
            self.stamp_live_plugin_state()?;
        }
        Ok(OpenedFrame {
            result,
            ended,
            carries,
        })
    }

    /// Open a new Agent Frame on a storeless runtime, or replay the current
    /// one idempotently.
    ///
    /// A storeless runtime has no durable head: the frame opens in resident
    /// state and the live interpreter restarts from its seed at once, as
    /// after every accepted open (FIG-4134, F5). A store-backed session's
    /// head is owned by its bound turn, so its host opens are
    /// [`SessionCommand::OpenAgentFrame`](crate::SessionCommand::OpenAgentFrame)
    /// commands its shift applies and commits at a turn boundary (FIG-4202);
    /// calling this on one is refused with
    /// [`RuntimeErrorCode::SessionCommandRequired`].
    ///
    /// Refuses with
    /// [`RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported`] when the
    /// key names a frame of the session that is not current. Refuses with
    /// [`RuntimeErrorCode::ExecutionStateCaptureFailed`], before anything is
    /// opened, a seed that carries artifacts: only an open that commits its
    /// own frame with a carry (a context-pressure hook, an administrative
    /// compaction, `continue_as`) can hand them to the new frame.
    pub async fn open_storeless_agent_frame(
        &mut self,
        request: crate::OpenAgentFrameRequest,
    ) -> Result<crate::OpenAgentFrameOutcome, RuntimeError> {
        self.refuse_store_backed_host_write("open_agent_frame")?;
        let opened = self.stage_agent_frame(request, StagedOpen::Caller).await?;
        if opened.result.opened {
            self.restore_protocol_session_after_frame_open().await?;
        }
        Ok(opened.result)
    }

    /// Opens `request`'s frame in resident state through the one frame-open
    /// primitive, after the checks its opener needs. The caller resets the
    /// live state once the open is accepted.
    pub(in crate::runtime) async fn stage_agent_frame(
        &mut self,
        request: crate::OpenAgentFrameRequest,
        opener: StagedOpen,
    ) -> Result<OpenedFrame, RuntimeError> {
        self.reload_invalidated_resident_session_state().await?;
        if opener == StagedOpen::Compaction {
            return self.open_frame(request).await;
        }
        // The runtime's own store: a runtime whose session is not
        // materialized holds it too, and without it the historical check
        // reads nothing and the staged frame collides at the commit.
        let store = self.services.store.clone();
        crate::runtime::state::refuse_historical_frame_switch(
            store.as_ref(),
            &self.state.session_id,
            self.state.current_frame_node_id.as_deref(),
            &self.state.session_graph,
            &request.frame_key,
        )
        .await?;
        let seed_carries = crate::runtime::turn_boundary::derive_seed_carries(
            self.session.as_mut(),
            &crate::session_graph::frame_node_id(
                &self.state.session_id,
                request.frame_key.as_str(),
            ),
            &request.initial_nodes,
        )
        .await
        .map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::ExecutionStateCaptureFailed,
                format!("failed to derive what a frame's seed carries: {error}"),
            )
        })?;
        if seed_carries != crate::runtime::turn_boundary::SeedCarries::none() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ExecutionStateCaptureFailed,
                "a staged frame open cannot carry its seed's artifacts into the new frame; \
                 open the frame through a commit that carries them (a context-pressure \
                 hook, an administrative compaction or `continue_as`)",
            ));
        }
        self.open_frame(request).await
    }

    /// Restores the live protocol session from the resident state after a
    /// frame opened, so the live execution state matches the durable one:
    /// the open cleared the stored snapshot, and the protocol restarts from
    /// the new frame's seed nodes (ADR 0113 §3.1). Every accepted open goes
    /// through here.
    pub(in crate::runtime) async fn restore_protocol_session_after_frame_open(
        &mut self,
    ) -> Result<(), RuntimeError> {
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        let protocol_session = Arc::clone(session.plugins().protocol_session());
        let session_id = self.state.session_id.clone();
        let restored = protocol_session
            .restore_session(
                crate::plugin::ProtocolSessionContext::new(&session_id, session.fleet_format()),
                crate::plugin::ProtocolSessionRestoreView::new(&self.state),
            )
            .await;
        restored.map_err(|err| {
            self.invalidate_resident_session_state();
            RuntimeError::new(
                RuntimeErrorCode::ContextPrepareTurn,
                format!("failed to restore the protocol session after a frame opened: {err}"),
            )
        })
    }
}

/// Who stages an open, and so which checks it needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime) enum StagedOpen {
    /// A caller naming its own frame key, a storeless runtime's host or a
    /// host's `OpenAgentFrame` command (FIG-4202): a key can name a persisted
    /// historical frame, which is refused, and the open carries nothing out
    /// of the frame it leaves, so a seed that carries artifacts is refused
    /// too.
    Caller,
    /// An administrative compaction, whose key core derives from the
    /// compaction's scope and the frame current at its recorded base, as a
    /// pressure frame's is: it names a new frame (FIG-4133, FIG-4201). It
    /// commits its frame with the artifacts its seed carries.
    Compaction,
}
