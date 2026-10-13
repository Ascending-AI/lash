//! `LashRuntime` tool-catalog operations. A session's config changes only
//! through a config transaction (`config_transaction.rs`).
//!
//! Extracted from `runtime/mod.rs`. This file re-opens `impl LashRuntime`.

use crate::SessionError;

use super::LashRuntime;

impl LashRuntime {
    /// Forget the session's built tool surface: the next sync builds it again
    /// from what its providers advertise under the run's configuration.
    pub async fn refresh_session_tool_catalog(&mut self) -> Result<(), SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        let Some(session) = self.session.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime session not available".to_string(),
            ));
        };
        session.refresh_tool_catalog();
        self.stamp_live_plugin_state()
            .map_err(|error| SessionError::Plugin(crate::PluginError::Runtime(error)))?;
        Ok(())
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
        plugin_config: crate::AdmittedPluginConfig::new(
            config.plugin_config.clone(),
            config.config_revision,
        ),
    };
    let request = match state.plugin_state() {
        Some(snapshot) => crate::plugin::PluginSessionRequest::rematerialization(
            state.session_id.clone(),
            snapshot,
            authority,
        ),
        None => crate::plugin::PluginSessionRequest::creation(state.session_id.clone(), authority),
    };
    host.validate_session_tool_catalog(
        request,
        &crate::plugin::ProtocolSessionRestoreView::new(state),
        live,
    )
    .await
}

/// Return a candidate catalog's config refusal (a namespace collision, or
/// a surface its providers cannot build) without losing its type at a
/// plugin boundary. Other failures remain infrastructure failures.
pub(super) fn catalog_config_refusal(error: &crate::PluginError) -> Option<crate::ConfigRefusal> {
    match error {
        crate::PluginError::ToolSurfaceRefused { refusal } => Some(
            crate::plugin::CoreConfigOwner::surface_refusal(refusal.as_ref().clone()),
        ),
        crate::PluginError::Runtime(error) => error
            .run_shape_refusal()
            .and_then(catalog_refusal_of)
            .cloned(),
        _ => None,
    }
}

/// The catalog refusal a run-shape refusal carries, if it is one: the core
/// owner's namespace collision or surface refusal.
pub(super) fn catalog_refusal_of(
    refusal: &crate::RunShapeRefusal,
) -> Option<&crate::ConfigRefusal> {
    match refusal {
        crate::RunShapeRefusal::Owner { refusal }
            if matches!(
                refusal.owner_refusal::<crate::CoreConfigRefusal>(),
                Some(
                    crate::CoreConfigRefusal::ToolNamespaceCollision { .. }
                        | crate::CoreConfigRefusal::ToolSurfaceRefused { .. }
                )
            ) =>
        {
            Some(refusal)
        }
        _ => None,
    }
}
