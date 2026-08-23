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

/// Durable session-policy mutation carried by
/// [`crate::SessionCommand::ApplyConfigPatch`].
///
/// Every field is applied at the session-command drain. The command commit is
/// therefore the publication boundary: resident policy is never changed by a
/// setter before the durable head accepts the same values.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApplyConfigPatch {
    /// Exact session-config wire generation. The patch and the head row share
    /// one schema because they carry the same durable policy facts.
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<crate::ModelSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<crate::PromptLayer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<crate::GenerationOverlay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_budget: Option<crate::TurnBudget>,
}

impl Default for ApplyConfigPatch {
    fn default() -> Self {
        Self {
            schema_version: crate::store::SESSION_HEAD_META_SCHEMA_VERSION,
            provider_id: None,
            model: None,
            prompt: None,
            generation: None,
            turn_budget: None,
        }
    }
}

impl ApplyConfigPatch {
    pub(super) fn between(previous: &crate::SessionPolicy, next: &crate::SessionPolicy) -> Self {
        Self {
            provider_id: (previous.provider_id != next.provider_id)
                .then(|| next.provider_id.clone()),
            model: (previous.model != next.model).then(|| next.model.clone()),
            prompt: (previous.prompt != next.prompt).then(|| next.prompt.clone()),
            generation: (previous.generation != next.generation)
                .then(|| crate::GenerationOverlay::Replace(next.generation.clone())),
            turn_budget: (previous.turn_budget != next.turn_budget).then_some(next.turn_budget),
            ..Self::default()
        }
    }

    pub(super) fn validate(&self) -> Result<(), crate::RuntimeError> {
        if self.schema_version != crate::store::SESSION_HEAD_META_SCHEMA_VERSION {
            return Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::SessionCommandClaim,
                format!(
                    "unsupported config patch schema version {}; expected {}",
                    self.schema_version,
                    crate::store::SESSION_HEAD_META_SCHEMA_VERSION
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn apply_to(&self, policy: &mut crate::SessionPolicy) {
        if let Some(provider_id) = self.provider_id.as_ref() {
            policy.provider_id = provider_id.clone();
        }
        if let Some(model) = self.model.as_ref() {
            policy.model = model.clone();
        }
        if let Some(prompt) = self.prompt.as_ref() {
            policy.prompt = prompt.clone();
        }
        if let Some(generation) = self.generation.as_ref() {
            policy.generation = generation.resolve(&policy.generation);
        }
        if let Some(turn_budget) = self.turn_budget {
            policy.turn_budget = turn_budget;
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.provider_id.is_none()
            && self.model.is_none()
            && self.prompt.is_none()
            && self.generation.is_none()
            && self.turn_budget.is_none()
    }
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
        let provider = patch.provider;
        let mut candidate = previous.clone();
        if let Some(provider) = provider.as_ref() {
            candidate.provider_id = provider.kind().to_string();
        }
        if let Some(model) = patch.model {
            candidate.model = model;
        }
        if let Some(prompt) = patch.prompt {
            candidate.prompt = prompt;
        }
        mutate_prompt(&mut candidate.prompt);
        if let Some(generation) = patch.generation {
            candidate.generation = generation.resolve(&candidate.generation);
        }
        candidate = self
            .resolve_session_config_mutations(previous.clone(), candidate)
            .await;
        let durable_patch = ApplyConfigPatch::between(&previous, &candidate);
        if !durable_patch.is_empty() {
            match self
                .submit_apply_config_patch(durable_patch)
                .await
                .map_err(|error| SessionError::Protocol(error.to_string()))?
            {
                super::SessionCommandSettlement::Durable(receipt) => drop(receipt),
                super::SessionCommandSettlement::Rejected(error) => {
                    return Err(SessionError::Protocol(format!(
                        "session config command rejected before acceptance: {}",
                        error.message
                    )));
                }
                super::SessionCommandSettlement::Pending(receipt) => {
                    return Err(SessionError::SessionCommandPending(receipt));
                }
                super::SessionCommandSettlement::Cancelled(receipt) => {
                    return Err(SessionError::SessionCommandCancelled(receipt));
                }
            }
        }
        // These fields are explicitly live policy, not part of the durable
        // session-head config. Publish them only after the durable-classified
        // portion above has settled so a whole-policy plugin mutator cannot
        // smuggle a resident-first durable assignment back into this path.
        self.state.policy.autonomous = candidate.autonomous;
        self.state.policy.no_progress_budget = candidate.no_progress_budget;
        if let Some(provider) = provider {
            self.host.core.providers.provider_resolver =
                std::sync::Arc::new(crate::SingleProviderResolver::new(provider));
        }
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
