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
    /// Protocol-owned turn options. Unlike the other fields this durable fact
    /// lives on the runtime session state rather than inside
    /// [`crate::SessionPolicy`], but it settles through the same commanded
    /// path: the drain commit publishes it to the session head (v6) and to
    /// resident state in one step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_turn_options: Option<crate::ProtocolTurnOptions>,
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
            protocol_turn_options: None,
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
            policy.replace_model_retaining_attachment_acceptance(model.clone());
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
            && self.protocol_turn_options.is_none()
    }

    /// Publish every settled field to resident session state.
    ///
    /// Policy-homed fields land through [`Self::apply_to`]; the protocol turn
    /// options land on their runtime-state home. Both publications happen only
    /// after the durable head accepted the same values.
    pub(super) fn apply_to_state(&self, state: &mut crate::RuntimeSessionState) {
        self.apply_to(&mut state.policy);
        if let Some(options) = self.protocol_turn_options.as_ref() {
            state.protocol_turn_options = options.clone();
        }
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

/// Content-addressed identity for the reopen seed commit (FIG-1875).
///
/// The identity pairs the base head revision with the hash of the commit
/// intent — the reconciled seed config and graph — following the
/// `initial-park` precedent in [`super::state::boundary_operation`]'s audit
/// table: an exact retry of the same seed against the same head replays under
/// the journaled-determinism guard, while different content (a later reopen
/// with a different seed, or the same seed against an advanced head) mints a
/// different operation and commits fresh. A per-session constant identity
/// would instead make the guard refuse every second differing reopen.
fn reopen_seed_operation(
    state: &crate::RuntimeSessionState,
    commit_budget: crate::CommitBudget,
) -> Result<crate::OperationId, crate::StoreError> {
    let preview_operation = super::state::boundary_operation(
        &state.session_id,
        "session-open-preview",
        "record-seeded-config",
    );
    let mut graph = state.pending_graph_commit();
    graph.derive_node_ids(&state.session_id, &preview_operation)?;
    let preview =
        crate::store::RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            &[],
            preview_operation,
            commit_budget,
        )?;
    let content_hash = preview.turn_commit_hash()?;
    Ok(super::state::boundary_operation(
        &state.session_id,
        &format!("base:{}:content:{content_hash}", state.head_revision),
        "record-seeded-config",
    ))
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
            candidate.replace_model_retaining_attachment_acceptance(model);
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
            self.settle_config_patch(durable_patch).await?;
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

    /// Submit a durable config patch and map its settlement to the session
    /// setter contract: enqueue-failure = rejected error, drain-commit =
    /// durable success, pending/cancelled = their typed errors.
    async fn settle_config_patch(&mut self, patch: ApplyConfigPatch) -> Result<(), SessionError> {
        match self
            .submit_apply_config_patch(patch)
            .await
            .map_err(|error| SessionError::Protocol(error.to_string()))?
        {
            super::SessionCommandSettlement::Durable(receipt) => {
                drop(receipt);
                Ok(())
            }
            super::SessionCommandSettlement::Rejected(error) => {
                Err(SessionError::Protocol(format!(
                    "session config command rejected before acceptance: {}",
                    error.message
                )))
            }
            super::SessionCommandSettlement::Pending(receipt) => {
                Err(SessionError::SessionCommandPending(receipt))
            }
            super::SessionCommandSettlement::Cancelled(receipt) => {
                Err(SessionError::SessionCommandCancelled(receipt))
            }
        }
    }

    /// Guard-write the facade's open-time seed to the durable head
    /// (seed-then-write, FIG-1875).
    ///
    /// ADR 0030's reopen reconciliation is an explicit host-seed precedence
    /// applied exactly once, before the runtime starts. Adoption is
    /// head-authoritative with no preservation lists, so the seed must not
    /// stay resident-only: this settles the difference between the persisted
    /// head config and the freshly reconciled policy through the commanded
    /// durable write, making the durable head true again by the end of open.
    /// A reopen whose seed matches the head settles nothing. No turn can be
    /// active this early, so the seed publishes as one direct fenced head
    /// commit (the same guard-write shape as protocol materialization)
    /// rather than a mid-run session command.
    ///
    /// The commit identity is content-addressed (the `initial-park` pattern):
    /// a retry of the same reconciled seed against the same head replays the
    /// original commit through the determinism guard, while a later reopen
    /// with a different seed — or against an advanced head — hashes to a new
    /// operation and is admitted as a new commit.
    ///
    /// Facade-only: reached through [`crate::facade_support`].
    pub(crate) async fn settle_reopen_seeded_config(
        &mut self,
        persisted: &crate::PersistedSessionConfig,
    ) -> Result<(), SessionError> {
        let policy = &self.state.policy;
        // A legacy promptless head (`prompt: None`) matches a default-empty
        // resident prompt layer: neither carries prompt content, so treating
        // them as differing would mint a spurious open-time commit.
        let prompt_differs = match persisted.prompt.as_ref() {
            Some(prompt) => prompt != &policy.prompt,
            None => policy.prompt != crate::PromptLayer::default(),
        };
        let seed_differs = persisted.provider_id != policy.provider_id
            || persisted.model != policy.model
            || prompt_differs
            || persisted.generation != policy.generation;
        if !seed_differs {
            return Ok(());
        }
        let Some(store) = self.services.store.clone() else {
            return Ok(());
        };
        let operation = reopen_seed_operation(&self.state, self.host.core.durability.commit_budget)
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        let (commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_budget(
                &mut self.state,
                &[],
                operation,
                self.host.core.durability.commit_budget,
            )
            .map_err(|error| SessionError::Protocol(error.to_string()))?;
        let result = super::commit_runtime_state_with_fresh_session_execution_lease(
            store,
            commit,
            &self.runtime_lease_owner,
            &self.runtime_lease_executor_id,
            self.host.core.control.lease_timings,
            std::sync::Arc::clone(&self.host.core.clock),
        )
        .await
        .map_err(|source| {
            super::session_commit_error("failed to record the reopen-seeded session config", source)
        })?;
        if result.receipt_replayed {
            // A receipt proves this seed settled once, not that its config is
            // still current. A delayed retry may race a later config command;
            // discard the local seed and adopt the durable head in full.
            self.invalidate_resident_session_state();
            self.reload_invalidated_resident_session_state_for_session()
                .await?;
        } else {
            self.state.apply_persisted_commit_result(result);
            self.state.mark_node_ids_persisted(persisted_node_ids);
        }
        Ok(())
    }

    /// Override protocol-owned turn options for this session through the
    /// commanded durable write (FIG-2479).
    ///
    /// The patch settles like every other session-config command: the durable
    /// head accepts the value before resident state publishes it, so a
    /// successful return means the options are durable. Restating the current
    /// value is a no-op.
    pub async fn set_protocol_turn_options(
        &mut self,
        options: crate::ProtocolTurnOptions,
    ) -> Result<(), SessionError> {
        self.apply_protocol_turn_options_patch(options).await
    }

    /// Override protocol-owned turn options through the commanded durable
    /// write (FIG-2479).
    ///
    /// Existing `FrameOpen` nodes are immutable historical snapshots; the next
    /// opened frame captures the settled value.
    pub async fn set_protocol_turn_options_all_frames(
        &mut self,
        options: crate::ProtocolTurnOptions,
    ) -> Result<(), SessionError> {
        self.apply_protocol_turn_options_patch(options).await
    }

    async fn apply_protocol_turn_options_patch(
        &mut self,
        options: crate::ProtocolTurnOptions,
    ) -> Result<(), SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        if self.state.protocol_turn_options == options {
            return Ok(());
        }
        self.settle_config_patch(ApplyConfigPatch {
            protocol_turn_options: Some(options),
            ..ApplyConfigPatch::default()
        })
        .await
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

#[cfg(test)]
mod reopen_seed_identity_tests {
    use super::reopen_seed_operation;

    #[test]
    fn reopen_seed_identity_is_stable_for_replay_and_distinguishes_seeds() {
        let mut state = crate::RuntimeSessionState {
            session_id: "reopen-seed-identity".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        state.ensure_agent_frame_initialized();
        let budget = crate::CommitBudget::bounded(1024 * 1024, 512);
        let first = reopen_seed_operation(&state, budget).expect("first seed identity");

        let mut retry_state = state.clone();
        let retry = reopen_seed_operation(&retry_state, budget).expect("retry seed identity");
        retry_state.head_revision = 41;
        let advanced = reopen_seed_operation(&retry_state, budget).expect("advanced seed identity");

        let mut changed_state = retry_state;
        changed_state.policy.prompt = crate::PromptLayer::new().with_contribution(
            crate::PromptContribution::guidance("Host", "A DIFFERENT RECONCILED SEED"),
        );
        let changed = reopen_seed_operation(&changed_state, budget).expect("changed seed identity");

        assert_eq!(
            first, retry,
            "the same seed against the same base must retain replay identity"
        );
        assert_ne!(
            first, advanced,
            "a new base head must mint a fresh identity"
        );
        assert_ne!(
            advanced, changed,
            "a different reconciled seed must not reuse the first receipt"
        );
    }
}
