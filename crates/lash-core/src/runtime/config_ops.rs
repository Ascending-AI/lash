//! `LashRuntime` tool-catalog and tool-state operations. A session's config
//! changes only through a config transaction (`config_transaction.rs`).
//!
//! Extracted from `runtime/mod.rs`. This file re-opens `impl LashRuntime`.

use crate::SessionError;
use std::sync::Arc;

use super::LashRuntime;

impl LashRuntime {
    /// Re-register the current tool catalog in the live protocol session.
    pub async fn refresh_session_tool_catalog(&mut self) -> Result<(), SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        let Some(session) = self.session.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime session not available".to_string(),
            ));
        };
        session
            .plugins()
            .tool_registry()
            .refresh_sources()
            .map_err(|err| SessionError::Protocol(format!("tool refresh failed: {err}")))?;
        session.refresh_tool_catalog().await?;
        self.stamp_live_plugin_state()
            .map_err(|error| SessionError::Plugin(crate::PluginError::Runtime(error)))?;
        Ok(())
    }

    pub async fn apply_tool_state(
        &mut self,
        snapshot: crate::ToolState,
    ) -> Result<u64, SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        let Some(session) = self.session.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime session not available".to_string(),
            ));
        };
        let generation = session
            .plugins()
            .tool_registry()
            .apply_state(snapshot)
            .map_err(|err| SessionError::Protocol(format!("tool reconfigure failed: {err}")))?;
        session.refresh_tool_catalog().await?;
        self.stamp_live_plugin_state()
            .map_err(|error| SessionError::Plugin(crate::PluginError::Runtime(error)))?;
        Ok(generation)
    }

    /// The report from the most recent persisted tool-state install on this
    /// runtime.
    ///
    /// Present after any open that restored tool state, and replaced by every
    /// later host restore, persisted-state install or resident re-sync. It is
    /// how those internal reloads deliver their answer: the paths that have no
    /// return value leave the typed report here (and on the trace) instead of
    /// dropping it (FIG-3367).
    pub fn tool_restore_report(&self) -> Option<&crate::ToolRestoreReport> {
        self.tool_restore_report.as_ref()
    }

    /// Restore a persisted tool-state snapshot over the live source surface.
    ///
    /// Unlike [`apply_tool_state`](Self::apply_tool_state) — a generation-checked
    /// delta that requires the snapshot to match the current generation and
    /// bumps it — this adopts the persisted generation when the reconciled
    /// surface is unchanged. A live-surface change bumps once, marking the
    /// snapshot dirty for the next commit. A cold resume whose surface reached
    /// generation ≥ 2 still succeeds because this is not a delta apply onto the
    /// fresh base-1 registry.
    ///
    /// Persisted tools that no registered source resolves become orphans
    /// (kept as non-members, rebound when their source returns) and are listed
    /// in the returned [`crate::ToolRestoreReport`].
    ///
    /// This is an install onto a *live* runtime, so it never refuses: the
    /// host's [`ToolSourcePolicy`](crate::ToolSourcePolicy) applies to opening
    /// a session, not to a restore the host asked for on one it already holds.
    /// A lost member comes back in the report, which is also retained for
    /// [`tool_restore_report`](Self::tool_restore_report).
    pub async fn restore_tool_state(
        &mut self,
        snapshot: crate::ToolState,
    ) -> Result<crate::ToolRestoreReport, SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        let tracing = self.host.core.tracing.clone();
        let session_id = self.state.session_id.clone();
        let Some(session) = self.session.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime session not available".to_string(),
            ));
        };
        let registry = session.plugins().tool_registry();
        let report = crate::runtime::tool_restore::install_persisted_tool_state(
            registry.as_ref(),
            snapshot,
            // A live runtime: this returns the report to its caller and
            // never refuses, whatever the host's open policy is (FIG-3367).
            crate::runtime::tool_restore::ToolRestoreContext::for_live_install(
                &session_id,
                crate::runtime::ToolRestoreSite::HostRestore,
                &tracing,
            ),
        )?;
        session.refresh_tool_catalog().await?;
        self.tool_restore_report = Some(report.clone());
        self.stamp_live_plugin_state()
            .map_err(|error| SessionError::Plugin(crate::PluginError::Runtime(error)))?;
        Ok(report)
    }
}
