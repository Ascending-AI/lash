use std::sync::Arc;

use lash_core::plugin::{
    CheckpointHookContext, PluginDirective, PluginError, ProtocolSessionContext,
    ProtocolSessionPlugin, TurnPluginDirective,
};
use lash_core::{CheckpointKind, ProtocolTurnOptions, SessionError};
use lash_rlm_types::{RlmCreateExtras, RlmSessionConfig};

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

    pub(crate) async fn projected_binding_prompt_contributions(
        &self,
    ) -> Vec<lash_core::PromptContribution> {
        self.runtime_state
            .projected_binding_prompt_contributions()
            .await
    }

    /// The soft context-budget warning (FIG-4398): a pure function of
    /// recorded state, holding nothing between calls. Every `AfterWork`
    /// checkpoint of a turn that began at or over the recorded threshold
    /// emits the same keyed status — the usage it reads is the session's
    /// committed prompt usage, which is constant within a turn — so a
    /// reopened or resumed session warns exactly as the one it replaces.
    pub(crate) fn soft_warn_directives(
        &self,
        ctx: CheckpointHookContext,
    ) -> Result<Vec<TurnPluginDirective>, PluginError> {
        if ctx.checkpoint != CheckpointKind::AfterWork {
            return Ok(Vec::new());
        }
        // The threshold the running root was admitted under; a session that
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
            effective_budget_tokens(configured, Some(ctx.state.policy().context_window_tokens()));
        let Some(threshold) = threshold else {
            return Ok(Vec::new());
        };
        // The model's budget suffix uses the prior completed prompt. The
        // checkpoint view's token_usage is already the current turn's usage.
        let used = ctx
            .state
            .last_prompt_usage()
            .map(|usage| usage.total().max(0) as usize)
            .unwrap_or(0);
        if used == 0 || used < threshold {
            return Ok(Vec::new());
        }
        Ok(vec![
            PluginDirective::emit_runtime_events(vec![lash_core::PluginRuntimeEvent::Status {
                key: BUDGET_WARNING_STATUS.to_string(),
                label: "context budget".to_string(),
                detail: Some(format!(
                    "{used} tokens used; warn at {threshold}; choose frame switch path"
                )),
            }])
            .into(),
        ])
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

    async fn apply_session_extension(
        &self,
        extension: lash_core::ProtocolSessionExtensionHandle,
    ) -> Result<(), SessionError> {
        self.runtime_state.apply_session_extension(extension).await
    }

    async fn bound_variables_prompt(
        &self,
        ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<std::sync::Arc<str>>, SessionError> {
        self.runtime_state
            .bound_variables_prompt(ctx.recorded_render())
            .await
            .map(Some)
    }
}

/// The durable RLM facts recorded on a set of protocol turn options.
///
/// Absence stays absence: a session that has stated nothing reads as an empty
/// [`RlmSessionConfig`] rather than as the values the defaults would resolve to.
pub fn rlm_session_config(
    options: &ProtocolTurnOptions,
) -> Result<RlmSessionConfig, RlmSessionConfigDecodeError> {
    if options.is_empty() {
        return Ok(RlmSessionConfig::default());
    }
    let extras = super::channel::without_session_pins(options)
        .decode::<RlmCreateExtras>()
        .map_err(|err| RlmSessionConfigDecodeError::Invalid(err.to_string()))?;
    Ok(RlmSessionConfig::from(&extras))
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
    impl lash_core::plugin::runtime_host::SessionStateService for NoopPromptManager {
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

    #[async_trait::async_trait]
    impl lash_core::plugin::runtime_host::SessionLifecycleService for NoopPromptManager {
        async fn create_session(
            &self,
            _request: lash_core::SessionCreateRequest,
        ) -> Result<lash_core::facade_support::SessionHandle, lash_core::plugin::PluginError>
        {
            Err(lash_core::plugin::PluginError::Session(
                "not used".to_string(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl lash_core::plugin::runtime_host::SessionGraphService for NoopPromptManager {}

    fn test_session(config: RlmProtocolPluginConfig) -> RlmProtocolSession {
        let runtime_state = Arc::new(RlmRuntimeState::new_for_tests().expect("runtime state"));
        RlmProtocolSession::new(config, runtime_state)
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
        session
            .apply_session_extension(crate::rlm_session_projection_extension(
                RlmProjectedBindings::new()
                    .bind_json("current_query", serde_json::json!("first"))
                    .expect("first bind"),
            ))
            .await
            .expect("first projection");

        let duplicate = session
            .apply_session_extension(crate::rlm_session_projection_extension(
                RlmProjectedBindings::new()
                    .bind_json("current_query", serde_json::json!("second"))
                    .expect("second bind"),
            ))
            .await;
        let Err(err) = duplicate else {
            panic!("duplicate session projection should fail");
        };
        assert!(err.to_string().contains("current_query"));
    }

    #[tokio::test]
    async fn session_projection_prompt_contribution_lists_names() {
        let session = test_session(
            RlmProtocolPluginConfig::builder()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(crate::plugin::InstructionBound::unbounded())
                .memory_limit(crate::plugin::MemoryBound::mebibytes(64))
                .build(),
        );
        session
            .apply_session_extension(crate::rlm_session_projection_extension(
                RlmProjectedBindings::new()
                    .bind_json("current_query", serde_json::json!("first"))
                    .expect("bind"),
            ))
            .await
            .expect("projection");

        let contributions = session.projected_binding_prompt_contributions().await;
        assert_eq!(contributions.len(), 1);
        assert!(contributions[0].content.contains("`current_query`"));
        assert!(
            contributions[0]
                .content
                .contains("`current_query`: `str`, read-only")
        );
    }

    #[test]
    fn soft_budget_warning_emits_plugin_event_not_user_message() {
        let session = test_session(RlmProtocolPluginConfig {
            continue_as_soft_warn_tokens: Some(100_000),
            ..RlmProtocolPluginConfig::builder()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(crate::plugin::InstructionBound::unbounded())
                .memory_limit(crate::plugin::MemoryBound::mebibytes(64))
                .build()
        });
        let policy = lash_core::SessionPolicy {
            model: lash_core::ModelSpec::builder("budget-unit-model")
                .context_window_tokens(200_000)
                .build()
                .expect("model limits"),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        };
        let state = lash_core::SessionSnapshot {
            last_prompt_usage: Some(lash_core::TokenUsage {
                input_tokens: 120_292,
                ..Default::default()
            }),
            ..lash_core::SessionSnapshot::new(policy)
        };
        let directives = session
            .soft_warn_directives(lash_core::plugin::CheckpointHookContext {
                session_id: SessionId::from("root"),
                checkpoint: lash_core::CheckpointKind::AfterWork,
                state: lash_core::SessionReadView::from_snapshot(&state),
                sessions: Arc::new(NoopPromptManager),
                session_lifecycle: Arc::new(NoopPromptManager),
                session_graph: Arc::new(NoopPromptManager),
                plugin_config: Default::default(),
            })
            .expect("warning directives");

        assert_eq!(directives.len(), 1);
        let lash_core::plugin::TurnPluginDirective::Ambient(
            lash_core::plugin::PluginDirective::EmitRuntimeEvents { events },
        ) = &directives[0]
        else {
            panic!("budget warning must be a runtime event, not an injected message");
        };
        assert_eq!(events.len(), 1);
        let lash_core::PluginRuntimeEvent::Status { key, label, detail } = &events[0] else {
            panic!("budget warning should use a typed status runtime event");
        };
        assert_eq!(key, BUDGET_WARNING_STATUS);
        assert_eq!(label, "context budget");
        assert!(detail.as_deref().is_some_and(|text| {
            text.contains("120292 tokens used")
                && text.contains("warn at 100000")
                && text.contains("choose frame switch path")
        }));
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
            model: lash_core::ModelSpec::builder("budget-unit-model")
                .context_window_tokens(41_000)
                .build()
                .expect("model limits"),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
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
            ..lash_core::SessionSnapshot::new(policy)
        };
        let directives = session
            .soft_warn_directives(lash_core::plugin::CheckpointHookContext {
                session_id: SessionId::from("root"),
                checkpoint: lash_core::CheckpointKind::AfterWork,
                state: lash_core::SessionReadView::from_snapshot(&state),
                sessions: Arc::new(NoopPromptManager),
                session_lifecycle: Arc::new(NoopPromptManager),
                session_graph: Arc::new(NoopPromptManager),
                plugin_config: Default::default(),
            })
            .expect("warning directives");
        assert!(directives.is_empty());
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
            model: lash_core::ModelSpec::builder("realistic-41k-model")
                .context_window_tokens(41_000)
                .build()
                .expect("model limits"),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        };
        let state = lash_core::SessionSnapshot {
            last_prompt_usage: Some(lash_core::TokenUsage {
                input_tokens: 40_999,
                ..Default::default()
            }),
            ..lash_core::SessionSnapshot::new(policy)
        };

        let directives = session
            .soft_warn_directives(lash_core::plugin::CheckpointHookContext {
                session_id: SessionId::from("root"),
                checkpoint: lash_core::CheckpointKind::AfterWork,
                state: lash_core::SessionReadView::from_snapshot(&state),
                sessions: Arc::new(NoopPromptManager),
                session_lifecycle: Arc::new(NoopPromptManager),
                session_graph: Arc::new(NoopPromptManager),
                plugin_config: Default::default(),
            })
            .expect("warning directives");

        assert_eq!(directives.len(), 1);
        let lash_core::plugin::TurnPluginDirective::Ambient(
            lash_core::plugin::PluginDirective::EmitRuntimeEvents { events },
        ) = &directives[0]
        else {
            panic!("budget warning must be a runtime event");
        };
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
            model: lash_core::ModelSpec::builder("budget-unit-model")
                .context_window_tokens(200_000)
                .build()
                .expect("model limits"),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        };
        let state = lash_core::SessionSnapshot {
            last_prompt_usage: Some(lash_core::TokenUsage {
                input_tokens: used,
                ..Default::default()
            }),
            ..lash_core::SessionSnapshot::new(policy)
        };
        lash_core::plugin::CheckpointHookContext {
            session_id: SessionId::from("root"),
            checkpoint: lash_core::CheckpointKind::AfterWork,
            state: lash_core::SessionReadView::from_snapshot(&state),
            sessions: Arc::new(NoopPromptManager),
            session_lifecycle: Arc::new(NoopPromptManager),
            session_graph: Arc::new(NoopPromptManager),
            plugin_config,
        }
    }

    fn statuses(directives: &[TurnPluginDirective]) -> Vec<lash_core::PluginRuntimeEvent> {
        directives
            .iter()
            .flat_map(|directive| match directive {
                TurnPluginDirective::Ambient(PluginDirective::EmitRuntimeEvents { events }) => {
                    events.clone()
                }
                other => panic!("the budget warning emits runtime events only: {other:?}"),
            })
            .collect()
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
                final_answer_format: None,
                channel: Some(crate::RlmChannel::Cell),
                dialect: Some("typescript".to_string()),
                behaviour: creating.recorded_behaviour(false),
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
                    .soft_warn_directives(after_work(used, recorded.clone()))
                    .expect("warning directives"),
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
