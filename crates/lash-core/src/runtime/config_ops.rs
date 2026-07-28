//! `LashRuntime` configuration mutators: provider, model spec, session id,
//! and tool-catalog refresh.
//!
//! Extracted from `runtime/mod.rs`. This file re-opens `impl LashRuntime`;
//! no types live here and no public API is changed.

use crate::SessionError;
use crate::provider::ProviderHandle;

use super::LashRuntime;

impl LashRuntime {
    /// Update model spec on the runtime config.
    pub fn set_model(&mut self, model: crate::ModelSpec) {
        self.policy.model = model;
        self.state.policy.model = self.policy.model.clone();
    }

    /// Update provider on the runtime config.
    pub fn set_provider(&mut self, provider: ProviderHandle) {
        self.host.core.providers.provider_resolver =
            std::sync::Arc::new(crate::SingleProviderResolver::new(provider.clone()));
        self.policy.provider_id = provider.kind().to_string();
        self.state.policy.provider_id = self.policy.provider_id.clone();
    }

    /// Update session ID metadata on the runtime config.
    pub fn set_session_id(&mut self, session_id: Option<String>) {
        self.policy.session_id = session_id;
        self.state.policy.session_id = self.policy.session_id.clone();
    }

    /// Apply a mid-run configuration change. Each argument is an overlay:
    /// `None` leaves the current value in place, `Some` replaces it. A
    /// `Some(GenerationOptions::default())` therefore clears the session's
    /// sampling intent rather than leaving it alone.
    pub async fn update_session_config(
        &mut self,
        provider: Option<ProviderHandle>,
        model: Option<crate::ModelSpec>,
        prompt: Option<crate::PromptLayer>,
        generation: Option<crate::GenerationOptions>,
    ) {
        let previous = self.session_policy();
        if let Some(provider) = provider {
            self.set_provider(provider);
        }
        if let Some(model) = model {
            self.policy.model = model;
        }
        if let Some(prompt) = prompt {
            self.policy.prompt = prompt;
        }
        if let Some(generation) = generation {
            self.policy.generation = generation;
        }
        self.state.policy = self.policy.clone();
        self.apply_session_config_mutations(previous.clone()).await;
        self.notify_session_config_changed(previous).await;
    }

    pub async fn set_prompt_template(&mut self, template: crate::PromptTemplate) {
        let mut prompt = self.policy.prompt.clone();
        prompt.template = Some(template);
        self.update_session_config(None, None, Some(prompt), None)
            .await;
    }

    pub async fn clear_prompt_template(&mut self) {
        let mut prompt = self.policy.prompt.clone();
        prompt.template = None;
        self.update_session_config(None, None, Some(prompt), None)
            .await;
    }

    pub async fn add_prompt_contribution(&mut self, contribution: crate::PromptContribution) {
        let mut prompt = self.policy.prompt.clone();
        prompt.add_contribution(contribution);
        self.update_session_config(None, None, Some(prompt), None)
            .await;
    }

    pub async fn replace_prompt_slot(
        &mut self,
        slot: crate::PromptSlot,
        contributions: impl IntoIterator<Item = crate::PromptContribution>,
    ) {
        let mut prompt = self.policy.prompt.clone();
        prompt.replace_slot(slot, contributions);
        self.update_session_config(None, None, Some(prompt), None)
            .await;
    }

    pub async fn clear_prompt_slot(&mut self, slot: crate::PromptSlot) {
        let mut prompt = self.policy.prompt.clone();
        prompt.clear_slot(slot);
        self.update_session_config(None, None, Some(prompt), None)
            .await;
    }

    /// Re-register the current tool catalog in the live protocol session.
    pub async fn refresh_session_tool_catalog(&mut self) -> Result<(), SessionError> {
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
        self.stamp_live_plugin_state();
        Ok(())
    }

    pub async fn apply_tool_state(
        &mut self,
        snapshot: crate::ToolState,
    ) -> Result<u64, SessionError> {
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
        self.stamp_live_plugin_state();
        Ok(generation)
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
    pub async fn restore_tool_state(
        &mut self,
        snapshot: crate::ToolState,
    ) -> Result<crate::ToolRestoreReport, SessionError> {
        let Some(session) = self.session.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime session not available".to_string(),
            ));
        };
        let report = session
            .plugins()
            .tool_registry()
            .restore_state(snapshot)
            .map_err(|err| SessionError::Protocol(format!("tool restore failed: {err}")))?;
        if !report.orphaned.is_empty() {
            tracing::warn!(
                orphaned = ?report.orphaned,
                "tool state restored with orphaned tools: no registered source \
                 resolves them; they remain non-members until their source returns"
            );
        }
        session.refresh_tool_catalog().await?;
        self.stamp_live_plugin_state();
        Ok(report)
    }
}
