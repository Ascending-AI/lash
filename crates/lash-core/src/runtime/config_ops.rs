//! `LashRuntime` session configuration patches and prompt helpers, plus
//! tool-catalog and tool-state operations.
//!
//! Extracted from `runtime/mod.rs`. This file re-opens `impl LashRuntime`.

use crate::SessionError;
pub use lash_core_store::session_policy::*;
use std::sync::Arc;

use super::LashRuntime;

/// A mid-run configuration change: what to make true of the session from here
/// on, leaving everything else alone.
///
/// The route is a provider id, never a live handle (FIG-3600 ruling 7): the
/// host's provider resolver must already serve it, or the change is refused
/// typed when it is sent.
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
    pub provider_id: Option<String>,
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
    pub async fn update_session_config(
        &mut self,
        patch: SessionConfigPatch,
    ) -> Result<(), SessionError> {
        Box::pin(self.apply_session_config(patch, |_| {})).await
    }

    async fn apply_session_config(
        &mut self,
        patch: SessionConfigPatch,
        mutate_prompt: impl FnOnce(&mut crate::PromptLayer),
    ) -> Result<(), SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        let previous = self.session_policy();
        let mut candidate = previous.clone();
        if let Some(provider_id) = patch.provider_id {
            candidate.provider_id = provider_id;
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
        let durable_patch =
            ApplyConfigPatch::between(&previous, &candidate, self.state.config_revision);
        if !durable_patch.is_empty() {
            self.settle_config_patch(durable_patch).await?;
        }
        // These fields are explicitly live policy, not part of the durable
        // session-head config. Publish them only after the durable-classified
        // portion above has settled so a whole-policy plugin mutator cannot
        // smuggle a resident-first durable assignment back into this path.
        self.state.policy.autonomous = candidate.autonomous;
        self.state.policy.no_progress_budget = candidate.no_progress_budget;
        self.notify_session_config_changed(previous)
            .await
            .map_err(|error| SessionError::Protocol(error.to_string()))
    }

    /// Submit a durable config patch and map its settlement to the session
    /// setter contract: enqueue-failure = rejected error, drain-commit =
    /// durable success, pending/cancelled = their typed errors.
    async fn settle_config_patch(&mut self, patch: ApplyConfigPatch) -> Result<(), SessionError> {
        // Boxed at this seam: the config-patch commit future carries a whole
        // runtime commit, and inlining it pushes both callers past the
        // large-future bound.
        match Box::pin(self.submit_apply_config_patch(patch))
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
            super::SessionCommandSettlement::Stale { base, head } => {
                Err(SessionError::Protocol(format!(
                    "session config command was written against config revision {base}, but the running revision is {head}: the command settled without applying"
                )))
            }
            super::SessionCommandSettlement::Refused { code } => Err(SessionError::Protocol(
                format!("session config command refused at the drain: {code:?}"),
            )),
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
    pub async fn settle_reopen_seeded_config(
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
        // A differing seed is a config write, so it advances the config
        // revision by one (ADR 0101 §12): a patch written against the pre-seed
        // revision must not compare-and-set onto the seeded config.
        self.state.config_revision = self.state.config_revision.saturating_add(1);
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
        match Box::pin(self.update_protocol_turn_options(|_| {
            Ok::<crate::ProtocolTurnOptions, std::convert::Infallible>(options)
        }))
        .await?
        {
            Ok(_) => Ok(()),
            Err(never) => match never {},
        }
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
        Box::pin(self.set_protocol_turn_options(options)).await
    }

    /// Reload the durable session state, derive protocol options from that
    /// exact value, and settle the derived value before returning.
    ///
    /// The nested result keeps a caller's typed decision error separate from
    /// session reload or settlement failures. The boolean reports whether a
    /// durable change was required.
    pub async fn update_protocol_turn_options<E>(
        &mut self,
        update: impl FnOnce(&crate::ProtocolTurnOptions) -> Result<crate::ProtocolTurnOptions, E>,
    ) -> Result<Result<bool, E>, SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        let options = match update(self.state.effective_protocol_turn_options()) {
            Ok(options) => options,
            Err(error) => return Ok(Err(error)),
        };
        if self.state.protocol_turn_options == options {
            return Ok(Ok(false));
        }
        self.settle_config_patch(ApplyConfigPatch {
            base_config_revision: self.state.config_revision,
            protocol_turn_options: Some(options),
            ..ApplyConfigPatch::default()
        })
        .await?;
        Ok(Ok(true))
    }

    /// Replace this session's persisted tool authority through the commanded
    /// durable write. A successful return makes the new surface authoritative
    /// for the next model request and across park/resume.
    pub async fn set_tool_access(
        &mut self,
        access: crate::SessionToolAccess,
    ) -> Result<(), SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        if self.state.authority.tool_access == access {
            return Ok(());
        }
        self.settle_config_patch(ApplyConfigPatch {
            base_config_revision: self.state.config_revision,
            tool_access: Some(access),
            ..ApplyConfigPatch::default()
        })
        .await
    }

    pub async fn set_prompt_template(
        &mut self,
        template: crate::PromptTemplate,
    ) -> Result<(), SessionError> {
        Box::pin(
            self.apply_session_config(SessionConfigPatch::default(), move |prompt| {
                prompt.template = Some(template);
            }),
        )
        .await
    }

    pub async fn clear_prompt_template(&mut self) -> Result<(), SessionError> {
        Box::pin(
            self.apply_session_config(SessionConfigPatch::default(), |prompt| {
                prompt.template = None;
            }),
        )
        .await
    }

    pub async fn add_prompt_contribution(
        &mut self,
        contribution: crate::PromptContribution,
    ) -> Result<(), SessionError> {
        Box::pin(
            self.apply_session_config(SessionConfigPatch::default(), move |prompt| {
                prompt.add_contribution(contribution);
            }),
        )
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
        Box::pin(
            self.apply_session_config(SessionConfigPatch::default(), move |prompt| {
                prompt.clear_slot(slot);
            }),
        )
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
        let clock = Arc::clone(&self.host.core.clock);
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
                clock.as_ref(),
            ),
        )?;
        session.refresh_tool_catalog().await?;
        self.tool_restore_report = Some(report.clone());
        self.stamp_live_plugin_state();
        Ok(report)
    }
}

#[cfg(test)]
mod reopen_seed_identity_tests {
    use super::reopen_seed_operation;
    use crate::SessionId;

    #[test]
    fn reopen_seed_identity_is_stable_for_replay_and_distinguishes_seeds() {
        let mut state = crate::RuntimeSessionState {
            session_id: SessionId::from("reopen-seed-identity"),
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
