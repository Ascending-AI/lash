use std::sync::Arc;

use lash_core::plugin::{
    CheckpointHookContext, PluginError, ProtocolSessionContext, ProtocolSessionPlugin,
    TurnContributions,
};
use lash_core::{CheckpointKind, ProtocolTurnOptions, SessionError};
use lash_rlm_types::RlmSessionConfig;

use super::budget_warning::BUDGET_WARNING_STATUS;
use super::runtime_state::RlmRuntimeState;
use super::{RLM_PROTOCOL_PLUGIN_ID, RlmProtocolPluginConfig, RlmRecordedConfig};
use crate::rlm_support::effective_budget_tokens;

pub(crate) struct RlmProtocolSession {
    config: RlmProtocolPluginConfig,
    runtime_state: Arc<RlmRuntimeState>,
}

impl RlmProtocolSession {
    pub(crate) fn new(
        config: RlmProtocolPluginConfig,
        runtime_state: Arc<RlmRuntimeState>,
    ) -> Self {
        Self {
            runtime_state,
            config,
        }
    }

    /// The soft context-budget warning (FIG-4398): a pure function of
    /// recorded state, holding nothing between calls. Every `AfterWork`
    /// checkpoint of a turn that began at or over the recorded threshold
    /// emits the same keyed status — the usage it reads is the session's
    /// committed prompt usage, which is constant within a turn — so a
    /// reopened or resumed session warns exactly as the one it replaces.
    pub(crate) fn soft_warn_contributions(
        &self,
        ctx: CheckpointHookContext,
    ) -> Result<TurnContributions, PluginError> {
        if ctx.checkpoint != CheckpointKind::AfterWork {
            return Ok(TurnContributions::default());
        }
        // The threshold the running run was admitted under; a session that
        // has recorded no RLM namespace yet runs under the behaviour its
        // plugin was built with.
        let configured = match ctx
            .plugin_config
            .decode::<RlmRecordedConfig>(RLM_PROTOCOL_PLUGIN_ID)
            .map_err(|error| {
                PluginError::Session(format!("invalid recorded RLM session config: {error}"))
            })? {
            Some(recorded) => recorded.behaviour.continue_as_soft_warn_tokens,
            None => self.config.continue_as_soft_warn_tokens,
        };
        let threshold =
            effective_budget_tokens(configured, ctx.state.policy().context_window_tokens());
        let Some(threshold) = threshold else {
            return Ok(TurnContributions::default());
        };
        // The model's context-budget section reads the prior completed
        // prompt. The checkpoint view's token_usage is already the current
        // turn's usage.
        let used = ctx
            .state
            .last_prompt_usage()
            .map(|usage| usage.total().max(0) as usize)
            .unwrap_or(0);
        if used == 0 || used < threshold {
            return Ok(TurnContributions::default());
        }
        Ok(TurnContributions {
            events: vec![lash_core::PluginRuntimeEvent::Status {
                key: BUDGET_WARNING_STATUS.to_string(),
                label: "context budget".to_string(),
                detail: Some(format!(
                    "{used} tokens used; warn at {threshold}; choose frame switch path"
                )),
            }],
            state: Default::default(),
            session: Default::default(),
        })
    }
}

#[async_trait::async_trait]
impl ProtocolSessionPlugin for RlmProtocolSession {
    async fn initialize_session(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), SessionError> {
        Ok(())
    }

    async fn restore_session(
        &self,
        ctx: ProtocolSessionContext<'_>,
        state: lash_core::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), SessionError> {
        self.runtime_state
            .restore_runtime_session_state(state, ctx.fleet_format())
            .await
    }

    async fn append_session_nodes(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        nodes: &[lash_core::SessionAppendNode],
    ) -> Result<(), SessionError> {
        self.runtime_state.append_session_nodes(nodes).await
    }

    /// The bound variables and read-only variables the session's sections
    /// render, under the run's recorded render.
    async fn prompt_facts(
        &self,
        ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<lash_core::plugin::prompt::ProtocolPromptFacts>, SessionError> {
        let facts = self
            .runtime_state
            .prompt_facts(
                ctx.recorded_render(),
                ctx.prompt_history(),
                self.config.prompt_features.images,
            )
            .await?;
        Ok(Some(Arc::new(facts)))
    }
}

/// The durable RLM facts recorded on a set of protocol turn options.
///
/// Absence stays absence: a session that has stated nothing reads as an empty
/// [`RlmSessionConfig`] rather than as the values the defaults would resolve to.
pub fn rlm_session_config(
    options: &ProtocolTurnOptions,
) -> Result<RlmSessionConfig, RlmSessionConfigDecodeError> {
    let recorded = RlmRecordedConfig::read(options)
        .map_err(|err| RlmSessionConfigDecodeError::Invalid(err.to_string()))?;
    Ok(recorded
        .map(|recorded| RlmSessionConfig {
            termination: recorded.termination,
        })
        .unwrap_or_default())
}

/// A recorded RLM options bag that could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RlmSessionConfigDecodeError {
    /// The bag is not a valid RLM session config.
    Invalid(String),
}

impl std::fmt::Display for RlmSessionConfigDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(detail) => write!(f, "invalid RLM session config: {detail}"),
        }
    }
}

impl std::error::Error for RlmSessionConfigDecodeError {}

#[cfg(test)]
mod tests {
    use lash_sansio::SessionId;
    use std::sync::Arc;

    use super::*;
    use crate::plugin::budget_warning::BUDGET_WARNING_STATUS;
    use crate::projection::RlmProjectedBindings;

    struct NoopPromptManager;

    #[async_trait::async_trait]
    impl lash_core::plugin::SessionReadService for NoopPromptManager {
        async fn snapshot_current(
            &self,
        ) -> Result<lash_core::SessionSnapshot, lash_core::plugin::PluginError> {
            Err(lash_core::plugin::PluginError::Session(
                "not used".to_string(),
            ))
        }

        async fn snapshot_session(
            &self,
            _session_id: &SessionId,
        ) -> Result<lash_core::SessionSnapshot, lash_core::plugin::PluginError> {
            Err(lash_core::plugin::PluginError::Session(
                "not used".to_string(),
            ))
        }

        async fn tool_catalog(
            &self,
            _session_id: &SessionId,
        ) -> Result<Vec<serde_json::Value>, lash_core::plugin::PluginError> {
            Ok(Vec::new())
        }
    }

    fn test_session(config: RlmProtocolPluginConfig) -> RlmProtocolSession {
        let runtime_state = Arc::new(RlmRuntimeState::new_for_tests().expect("runtime state"));
        RlmProtocolSession::new(config, runtime_state)
    }

    /// Land `bindings`' session extension on `session` the way the host's
    /// append does: its durable nodes, through `append_session_nodes`.
    async fn extend(
        session: &RlmProtocolSession,
        bindings: RlmProjectedBindings,
    ) -> Result<(), SessionError> {
        let fleet = lash_core::FleetFormat::current();
        let session_id = SessionId::from("rlm-session-extension");
        session
            .append_session_nodes(
                ProtocolSessionContext::new(&session_id, fleet),
                &crate::rlm_session_projection_extension(bindings).session_nodes(fleet),
            )
            .await
    }

    #[tokio::test]
    async fn session_projection_extension_rejects_duplicate_names() {
        let session = test_session(
            RlmProtocolPluginConfig::builder()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(crate::plugin::InstructionBound::unbounded())
                .memory_limit(crate::plugin::MemoryBound::mebibytes(64))
                .build(),
        );
        extend(
            &session,
            RlmProjectedBindings::new()
                .bind_json("current_query", serde_json::json!("first"))
                .expect("first bind"),
        )
        .await
        .expect("first projection");

        let duplicate = extend(
            &session,
            RlmProjectedBindings::new()
                .bind_json("current_query", serde_json::json!("second"))
                .expect("second bind"),
        )
        .await;
        let Err(err) = duplicate else {
            panic!("duplicate session projection should fail");
        };
        assert!(err.to_string().contains("current_query"));
    }

    /// FIG-5257: the session's prompt facts carry its read-only variables
    /// and its bound values under the run's recorded render, for the RLM
    /// sections of the call they are asked for.
    #[tokio::test]
    async fn the_prompt_facts_carry_the_sessions_variables() {
        let session = test_session(
            RlmProtocolPluginConfig::builder()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(crate::plugin::InstructionBound::unbounded())
                .memory_limit(crate::plugin::MemoryBound::mebibytes(64))
                .build(),
        );
        extend(
            &session,
            RlmProjectedBindings::new()
                .bind_json("current_query", serde_json::json!("open issues"))
                .expect("bind"),
        )
        .await
        .expect("bind the session's read-only variable");
        let session_id = SessionId::from("rlm-prompt-facts");
        let render = crate::testing::recorded_test_render();
        let mut snapshot = lash_core::SessionSnapshot::new(
            session_id.clone(),
            lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(16),
                lash_core::NoProgressBudget::bounded(12),
            ),
        );
        snapshot.session_graph.append_node_drafts_at(
            "prompt-facts",
            crate::driver::tests::prompt_history_fixture(true)
                .into_iter()
                .map(lash_core::session_graph::SessionNodeDraft::event),
            "2026-01-01T00:00:00.000000000Z"
                .parse()
                .expect("canonical timestamp"),
        );
        let history = lash_core::SessionReadView::from_snapshot(&snapshot);
        let facts = session
            .prompt_facts(
                ProtocolSessionContext::new(&session_id, lash_core::FleetFormat::current())
                    .with_recorded_render(&render)
                    .with_prompt_history(&history),
            )
            .await
            .expect("the facts derive")
            .expect("the protocol derives facts");
        let facts = facts
            .downcast_ref::<crate::prompt_sections::RlmPromptFacts>()
            .expect("the RLM facts");
        let variables = facts
            .read_only_variables
            .as_deref()
            .expect("the read-only variables");
        assert!(variables.contains("- `current_query`: `string`, read-only"));
        assert!(
            facts
                .history_binding
                .starts_with("- `history`: `HistoryItem[]`, read-only, 2 entries\n\nSchema:\n")
        );
        assert!(facts.history_binding.contains("HistoryItem"));
    }

    #[test]
    fn soft_budget_warning_uses_prior_prompt_usage_instead_of_current_turn_usage() {
        let session = test_session(RlmProtocolPluginConfig {
            continue_as_soft_warn_tokens: Some(100),
            ..RlmProtocolPluginConfig::builder()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(crate::plugin::InstructionBound::unbounded())
                .memory_limit(crate::plugin::MemoryBound::mebibytes(64))
                .build()
        });
        let policy = lash_core::SessionPolicy {
            model: Some(lash_core::LlmProfileConfig::new(
                lash_core::RecordedLlmProfile::mint(
                    lash_core::LlmProfileKey::from("budget-unit-model"),
                    lash_core::LlmProfileMetadata::builder("budget-unit-model")
                        .context_window_tokens(41_000)
                        .build()
                        .expect("model limits"),
                ),
            )),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            )
        };
        let state = lash_core::SessionSnapshot {
            token_usage: lash_core::TokenUsage {
                input_tokens: 120,
                ..Default::default()
            },
            last_prompt_usage: Some(lash_core::TokenUsage {
                input_tokens: 8,
                ..Default::default()
            }),
            ..lash_core::SessionSnapshot::new(lash_core::SessionId::from("session"), policy)
        };
        let directives = session
            .soft_warn_contributions(lash_core::plugin::CheckpointHookContext {
                session_id: SessionId::from("root"),
                checkpoint: lash_core::CheckpointKind::AfterWork,
                state: lash_core::SessionReadView::from_snapshot(&state),
                sessions: Arc::new(NoopPromptManager),
                plugin_config: Default::default(),
            })
            .expect("warning contributions");
        assert!(directives.events.is_empty());
    }

    #[test]
    fn soft_budget_warning_clamps_to_the_session_context_window() {
        let session = test_session(RlmProtocolPluginConfig {
            continue_as_soft_warn_tokens: Some(100_000),
            ..RlmProtocolPluginConfig::builder()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(crate::plugin::InstructionBound::unbounded())
                .memory_limit(crate::plugin::MemoryBound::mebibytes(64))
                .build()
        });
        let policy = lash_core::SessionPolicy {
            model: Some(lash_core::LlmProfileConfig::new(
                lash_core::RecordedLlmProfile::mint(
                    lash_core::LlmProfileKey::from("realistic-41k-model"),
                    lash_core::LlmProfileMetadata::builder("realistic-41k-model")
                        .context_window_tokens(41_000)
                        .build()
                        .expect("model limits"),
                ),
            )),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            )
        };
        let state = lash_core::SessionSnapshot {
            last_prompt_usage: Some(lash_core::TokenUsage {
                input_tokens: 40_999,
                ..Default::default()
            }),
            ..lash_core::SessionSnapshot::new(lash_core::SessionId::from("session"), policy)
        };

        let directives = session
            .soft_warn_contributions(lash_core::plugin::CheckpointHookContext {
                session_id: SessionId::from("root"),
                checkpoint: lash_core::CheckpointKind::AfterWork,
                state: lash_core::SessionReadView::from_snapshot(&state),
                sessions: Arc::new(NoopPromptManager),
                plugin_config: Default::default(),
            })
            .expect("warning contributions");

        let events = &directives.events;
        let lash_core::PluginRuntimeEvent::Status { detail, .. } = &events[0] else {
            panic!("budget warning should use a typed status runtime event");
        };
        assert_eq!(
            detail.as_deref(),
            Some("40999 tokens used; warn at 40999; choose frame switch path")
        );
    }

    /// A checkpoint context at `AfterWork` whose session's committed prompt
    /// usage is `used` tokens, under `plugin_config`.
    fn after_work(
        used: i64,
        plugin_config: lash_core::AdmittedPluginConfig,
    ) -> lash_core::plugin::CheckpointHookContext {
        let policy = lash_core::SessionPolicy {
            model: Some(lash_core::LlmProfileConfig::new(
                lash_core::RecordedLlmProfile::mint(
                    lash_core::LlmProfileKey::from("budget-unit-model"),
                    lash_core::LlmProfileMetadata::builder("budget-unit-model")
                        .context_window_tokens(200_000)
                        .build()
                        .expect("model limits"),
                ),
            )),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            )
        };
        let state = lash_core::SessionSnapshot {
            last_prompt_usage: Some(lash_core::TokenUsage {
                input_tokens: used,
                ..Default::default()
            }),
            ..lash_core::SessionSnapshot::new(lash_core::SessionId::from("session"), policy)
        };
        lash_core::plugin::CheckpointHookContext {
            session_id: SessionId::from("root"),
            checkpoint: lash_core::CheckpointKind::AfterWork,
            state: lash_core::SessionReadView::from_snapshot(&state),
            sessions: Arc::new(NoopPromptManager),
            plugin_config,
        }
    }

    fn statuses(contributions: &TurnContributions) -> Vec<lash_core::PluginRuntimeEvent> {
        contributions.events.clone()
    }

    /// The soft warning is a pure function of recorded state (FIG-4398):
    /// every `AfterWork` checkpoint over the recorded threshold emits the
    /// same keyed status, a session reopened or resumed by a worker whose
    /// factory states another threshold — or none — emits exactly what the
    /// original emits, and nothing the session holds suppresses or changes
    /// it.
    #[test]
    fn the_soft_warning_is_the_same_keyed_status_on_every_worker() {
        let creating = RlmProtocolPluginConfig {
            continue_as_soft_warn_tokens: Some(100_000),
            ..RlmProtocolPluginConfig::builder()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(crate::plugin::InstructionBound::unbounded())
                .memory_limit(crate::plugin::MemoryBound::mebibytes(64))
                .build()
        };
        let mut config =
            lash_core::PluginConfig::for_protocol(Some(crate::RLM_PROTOCOL_PLUGIN_ID.to_string()));
        config.insert(
            crate::RLM_PROTOCOL_PLUGIN_ID,
            serde_json::to_value(crate::RlmRecordedConfig {
                render: None,
                termination: None,
                channel: Some(crate::RlmChannel::Cell),
                dialect: Some("typescript".to_string()),
                behaviour: creating.recorded_behaviour(),
            })
            .expect("recorded namespace"),
        );
        let recorded = lash_core::AdmittedPluginConfig::new(config, 1);
        let original = test_session(creating);
        let resumed = test_session(RlmProtocolPluginConfig {
            continue_as_soft_warn_tokens: None,
            ..RlmProtocolPluginConfig::builder()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(crate::plugin::InstructionBound::unbounded())
                .memory_limit(crate::plugin::MemoryBound::mebibytes(64))
                .build()
        });

        let warned = |session: &RlmProtocolSession, used: i64| {
            statuses(
                &session
                    .soft_warn_contributions(after_work(used, recorded.clone()))
                    .expect("warning contributions"),
            )
        };
        let expected = vec![lash_core::PluginRuntimeEvent::Status {
            key: BUDGET_WARNING_STATUS.to_string(),
            label: "context budget".to_string(),
            detail: Some(
                "120000 tokens used; warn at 100000; choose frame switch path".to_string(),
            ),
        }];
        for checkpoint in 0..3 {
            assert_eq!(
                warned(&original, 120_000),
                expected,
                "checkpoint {checkpoint} of the original session"
            );
            assert_eq!(
                warned(&resumed, 120_000),
                expected,
                "checkpoint {checkpoint} on the worker that resumed it"
            );
        }
        assert!(warned(&original, 99_999).is_empty());
        assert!(warned(&resumed, 99_999).is_empty());
    }

    /// A malformed recorded bag is an error, never a silent default.
    #[test]
    fn an_unreadable_session_config_is_an_error() {
        for (label, payload) in [
            (
                "an unknown termination kind",
                serde_json::json!({"termination": {"kind": "python"}}),
            ),
            ("an unknown key", serde_json::json!({"lashlang": true})),
            (
                "a non-object termination",
                serde_json::json!({"termination": 7}),
            ),
        ] {
            let tampered = ProtocolTurnOptions::from_payload(payload);
            match rlm_session_config(&tampered) {
                Ok(config) => panic!("{label} resolved to {config:?} instead of refusing"),
                Err(error) => assert!(
                    error.to_string().starts_with("invalid RLM session config"),
                    "{label} refused with an unexpected message: {error}"
                ),
            }
        }
    }
}
