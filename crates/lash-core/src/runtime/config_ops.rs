//! `LashRuntime` tool-catalog operations. A session's config changes only
//! through a config transaction (`config_transaction.rs`).
//!
//! Extracted from `runtime/mod.rs`. This file re-opens `impl LashRuntime`.

use crate::SessionError;

use super::LashRuntime;

impl LashRuntime {
    /// Enqueue explicit source refresh for the next model boundary.
    pub async fn refresh_session_tool_catalog(&mut self) -> Result<(), SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        if self.session.is_none() {
            return Err(SessionError::Protocol(
                "runtime session not available".to_string(),
            ));
        }
        if let Some(environment) = &self.environment {
            environment.refresh();
        }
        self.stamp_live_plugin_state().map_err(|error| {
            SessionError::Plugin(
                lash_execution_contracts::plugin_error::PluginError::Runtime(error),
            )
        })?;
        Ok(())
    }
}

/// Judge that a proposed catalog builds, without materializing or
/// registering a live session. Config commands can still repair a session
/// whose old plugin configuration cannot build.
pub(super) async fn validate_config_tool_catalog(
    host: &crate::PluginHost,
    state: &crate::RuntimeSessionState,
    config: &crate::PersistedSessionConfig,
) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
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
    host.validate_session_tool_catalog(request)
}

/// Return a candidate catalog's config refusal (a surface its providers
/// cannot build) without losing its type at a
/// plugin boundary. Other failures remain infrastructure failures.
pub(super) fn catalog_config_refusal(
    error: &lash_execution_contracts::plugin_error::PluginError,
) -> Option<crate::ConfigRefusal> {
    match error {
        lash_execution_contracts::plugin_error::PluginError::ToolSurfaceRefused { refusal } => {
            Some(
                lash_execution_contracts::config_error::CoreConfigRefusal::surface_refusal(
                    refusal.as_ref().clone(),
                ),
            )
        }
        lash_execution_contracts::plugin_error::PluginError::Runtime(error) => error
            .run_shape_refusal()
            .and_then(catalog_refusal_of)
            .cloned(),
        _ => None,
    }
}

/// The catalog refusal a run-shape refusal carries, if it is one: the core
/// owner's surface refusal.
pub(super) fn catalog_refusal_of(
    refusal: &crate::RunShapeRefusal,
) -> Option<&crate::ConfigRefusal> {
    match refusal {
        crate::RunShapeRefusal::Owner { refusal }
            if matches!(
                refusal
                    .owner_refusal::<lash_execution_contracts::config_error::CoreConfigRefusal>(),
                Some(
                    lash_execution_contracts::config_error::CoreConfigRefusal::ToolSurfaceRefused { .. }
                    | lash_execution_contracts::config_error::CoreConfigRefusal::EnvironmentAssemblyRefused { .. }
                )
            ) =>
        {
            Some(refusal)
        }
        _ => None,
    }
}
