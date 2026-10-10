//! `LashRuntime` tool-catalog and tool-state operations. A session's config
//! changes only through a config transaction (`config_transaction.rs`).
//!
//! Extracted from `runtime/mod.rs`. This file re-opens `impl LashRuntime`.

use crate::SessionError;

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
        let registry = session.plugins().tool_registry();
        let (revision, preview) = registry.preview_reconfiguration();
        let generation = preview
            .apply_state(snapshot)
            .map_err(|err| SessionError::Protocol(format!("tool reconfigure failed: {err}")))?;
        session
            .validate_tool_registry(std::sync::Arc::new(preview.clone()))
            .await?;
        registry
            .publish_reconfiguration(revision, &preview)
            .map_err(|err| SessionError::Protocol(format!("tool reconfigure failed: {err}")))?;
        session.refresh_tool_catalog().await?;
        self.stamp_live_plugin_state()
            .map_err(|error| SessionError::Plugin(crate::PluginError::Runtime(error)))?;
        Ok(generation)
    }

    /// The report of the latest persisted tool-state install on this runtime
    /// that no turn has reported yet.
    ///
    /// A run's plugin transition, a host restore command, a persisted-state
    /// install and a resident re-sync each leave their report here; the next
    /// turn this runtime starts takes it and reports it as
    /// `TurnEvent::ToolRestoreReported` when it is not clean (FIG-5134).
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
    /// Unresolved ids never refuse: the host's
    /// [`ToolSourcePolicy`](crate::ToolSourcePolicy) applies to a turn run,
    /// not to a restore the host asked for. A lost member comes
    /// back in the report, which is also kept for
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
        let (revision, preview) = registry.preview_reconfiguration();
        let report = preview
            .restore_state(snapshot)
            .map_err(|err| SessionError::Protocol(format!("tool restore failed: {err}")))?;
        session
            .validate_tool_registry(std::sync::Arc::new(preview.clone()))
            .await?;
        registry
            .publish_reconfiguration(revision, &preview)
            .map_err(|err| SessionError::Protocol(format!("tool restore failed: {err}")))?;
        crate::runtime::tool_restore::deliver(
            &report,
            &crate::runtime::tool_restore::ToolRestoreContext::new(
                &session_id,
                crate::runtime::ToolRestoreSite::HostRestore,
                &tracing,
            ),
        );
        session.refresh_tool_catalog().await?;
        self.tool_restore_report = Some(report.clone());
        self.stamp_live_plugin_state()
            .map_err(|error| SessionError::Plugin(crate::PluginError::Runtime(error)))?;
        Ok(report)
    }
}

/// Judge a proposed catalog over the boundary's committed bindings without
/// materializing or registering a live session. Config commands can still
/// repair a session whose old plugin configuration cannot build.
pub(super) async fn validate_config_tool_catalog(
    host: &crate::PluginHost,
    state: &crate::RuntimeSessionState,
    config: &crate::PersistedSessionConfig,
    live: Option<&std::sync::Arc<dyn crate::plugin::ProtocolSessionPlugin>>,
) -> Result<(), crate::PluginError> {
    let authority = crate::plugin::SessionAuthorityContext {
        tool_access: config.tool_access.clone(),
        plugin_config: crate::AdmittedPluginConfig::new(
            config.plugin_config.clone(),
            config.config_revision,
        ),
    };
    let mut request = match state.plugin_state() {
        Some(snapshot) => crate::plugin::PluginSessionRequest::rematerialization(
            state.session_id.clone(),
            snapshot,
            authority,
        ),
        None => crate::plugin::PluginSessionRequest::creation(state.session_id.clone(), authority),
    };
    request.tool_snapshot = state.tool_state_snapshot().cloned();
    host.validate_session_tool_catalog(
        request,
        &crate::plugin::ProtocolSessionRestoreView::new(state),
        live,
    )
    .await
}

/// Return a namespace collision's config refusal without losing its type
/// at a plugin boundary. Other failures remain infrastructure failures.
pub(super) fn catalog_config_refusal(error: &crate::PluginError) -> Option<crate::ConfigRefusal> {
    if let crate::PluginError::Runtime(error) = error
        && let Some(crate::RunShapeRefusal::Owner { refusal }) = error.run_shape_refusal()
        && matches!(
            refusal.owner_refusal::<crate::CoreConfigRefusal>(),
            Some(crate::CoreConfigRefusal::ToolNamespaceCollision { .. })
        )
    {
        Some(refusal.clone())
    } else {
        None
    }
}
