//! `LashRuntime` session configuration patches and prompt helpers, plus
//! tool-catalog and tool-state operations.
//!
//! Extracted from `runtime/mod.rs`. This file re-opens `impl LashRuntime`.

use crate::SessionError;
use crate::provider::ProviderHandle;

use super::LashRuntime;

/// A mid-run configuration change: what to make true of the session from here
/// on, leaving everything else alone.
///
/// Every field is an overlay — `None` leaves the current value in place. The
/// vocabulary matches [`crate::SessionSpec`] field for field, deliberately:
/// `model` and `prompt` replace, and `generation` takes a
/// [`crate::GenerationOverlay`], so it merges per option unless it says
/// otherwise. A patch naming only an output-token cap would otherwise drop a
/// temperature and seed the session spec pinned — the same silent loss the
/// spec's overlay exists to prevent, one surface over.
#[derive(Clone, Debug, Default)]
pub struct SessionConfigPatch {
    pub provider: Option<ProviderHandle>,
    pub model: Option<crate::ModelSpec>,
    pub prompt: Option<crate::PromptLayer>,
    pub generation: Option<crate::GenerationOverlay>,
}

impl SessionConfigPatch {
    /// A patch that changes only the prompt layer.
    pub fn with_prompt(prompt: crate::PromptLayer) -> Self {
        Self {
            prompt: Some(prompt),
            ..Self::default()
        }
    }
}

impl LashRuntime {
    /// Apply a mid-run configuration change; see [`SessionConfigPatch`] for
    /// what each field leaves alone and what it replaces.
    pub async fn update_session_config(
        &mut self,
        patch: SessionConfigPatch,
    ) -> Result<(), SessionError> {
        self.apply_session_config(patch, |_| {}).await
    }

    async fn apply_session_config(
        &mut self,
        patch: SessionConfigPatch,
        mutate_prompt: impl FnOnce(&mut crate::PromptLayer),
    ) -> Result<(), SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        let previous = self.session_policy();
        if let Some(provider) = patch.provider {
            self.host.core.providers.provider_resolver =
                std::sync::Arc::new(crate::SingleProviderResolver::new(provider.clone()));
            self.policy.provider_id = provider.kind().to_string();
        }
        if let Some(model) = patch.model {
            self.policy.model = model;
        }
        if let Some(prompt) = patch.prompt {
            self.policy.prompt = prompt;
        }
        mutate_prompt(&mut self.policy.prompt);
        if let Some(generation) = patch.generation {
            self.policy.generation = generation.resolve(&self.policy.generation);
        }
        self.state.policy = self.policy.clone();
        self.apply_session_config_mutations(previous.clone()).await;
        self.notify_session_config_changed(previous)
            .await
            .map_err(|error| SessionError::Protocol(error.to_string()))
    }

    pub async fn set_prompt_template(
        &mut self,
        template: crate::PromptTemplate,
    ) -> Result<(), SessionError> {
        self.apply_session_config(SessionConfigPatch::default(), move |prompt| {
            prompt.template = Some(template);
        })
        .await
    }

    pub async fn clear_prompt_template(&mut self) -> Result<(), SessionError> {
        self.apply_session_config(SessionConfigPatch::default(), |prompt| {
            prompt.template = None;
        })
        .await
    }

    pub async fn add_prompt_contribution(
        &mut self,
        contribution: crate::PromptContribution,
    ) -> Result<(), SessionError> {
        self.apply_session_config(SessionConfigPatch::default(), move |prompt| {
            prompt.add_contribution(contribution);
        })
        .await
    }

    pub async fn replace_prompt_slot(
        &mut self,
        slot: crate::PromptSlot,
        contributions: impl IntoIterator<Item = crate::PromptContribution>,
    ) -> Result<(), SessionError> {
        self.apply_session_config(SessionConfigPatch::default(), move |prompt| {
            prompt.replace_slot(slot, contributions);
        })
        .await
    }

    pub async fn clear_prompt_slot(&mut self, slot: crate::PromptSlot) -> Result<(), SessionError> {
        self.apply_session_config(SessionConfigPatch::default(), move |prompt| {
            prompt.clear_slot(slot);
        })
        .await
    }

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
        self.stamp_live_plugin_state();
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
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
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
