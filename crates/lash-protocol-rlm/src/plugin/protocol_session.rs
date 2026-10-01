use std::sync::Arc;

use lash_core::plugin::{
    CheckpointHookContext, PluginDirective, PluginError, ProtocolSessionContext,
    ProtocolSessionPlugin, TurnPluginDirective,
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

    /// The session's system prompt (FIG-4588), rendered from recorded data:
    /// the prompt config the RLM namespace of `plugin_config` recorded, the
    /// recorded `tool_catalog`, the session's bindings and its subagent
    /// authority. `plugin_config` is the admitted root's, so a prompt
    /// command applied while a root runs reaches the next root. A session
    /// that has recorded no RLM namespace yet renders the built-in prompt,
    /// as it runs under the behaviour its plugin was built with (a reopened
    /// session with no namespace never gets here: its plugin refuses to
    /// build). The core calls it through [`ProtocolSessionPlugin::render_system_prompt`], from
    /// a turn's execution-environment sync and from a compaction's recorded
    /// render step, and journals the text it answers (FIG-4589).
    pub(crate) async fn system_prompt(
        &self,
        plugin_config: &lash_core::AdmittedPluginConfig,
        tool_catalog: &lash_core::ToolCatalog,
        subagent: Option<&lash_core::SubagentSessionContext>,
        scope: crate::system_prompt::RlmSystemPromptScope,
    ) -> Result<Arc<str>, SessionError> {
        let prompt = plugin_config
            .decode::<RlmRecordedConfig>(RLM_PROTOCOL_PLUGIN_ID)
            .map_err(|error| {
                SessionError::Protocol(format!("invalid recorded RLM session config: {error}"))
            })?
            .map(|recorded| recorded.prompt)
            .unwrap_or_default();
        Ok(self
            .runtime_state
            .system_prompt(
                &crate::system_prompt::RlmSystemPromptBehaviour {
                    channel: self.config.channel,
                    prompt_features: self.config.prompt_features,
                    discovery: self.config.discovery.as_ref(),
                },
                &prompt,
                tool_catalog,
                subagent,
                scope,
            )
            .await)
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
            effective_budget_tokens(configured, ctx.state.policy().context_window_tokens());
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

    async fn render_system_prompt(
        &self,
        ctx: lash_core::plugin::SystemPromptContext<'_>,
    ) -> Result<Arc<str>, SessionError> {
        let scope = match ctx.purpose {
            lash_core::plugin::SystemPromptPurpose::Turn => {
                crate::system_prompt::RlmSystemPromptScope::Turn
            }
            lash_core::plugin::SystemPromptPurpose::Compaction => {
                crate::system_prompt::RlmSystemPromptScope::Compaction
            }
        };
        self.system_prompt(ctx.plugin_config, ctx.tool_catalog, ctx.subagent, scope)
            .await
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
            final_answer_format: recorded.final_answer_format,
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
    use crate::system_prompt::RlmSystemPromptScope;

    const TURN: RlmSystemPromptScope = RlmSystemPromptScope::Turn;

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
    async fn session_projection_declaration_lists_names() {
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

        let declaration = session
            .runtime_state
            .read_only_variables_prompt()
            .await
            .expect("the session declares its read-only variables");
        assert!(declaration.contains("`current_query`: `string`, read-only"));
    }

    /// FIG-4588: the session's system prompt renders from the prompt config
    /// the given plugin config recorded, over the session's bindings and the
    /// given catalog and subagent authority. A root admitted before a prompt
    /// command renders the prompt it was admitted under and the next root
    /// the new one; a plugin config with no RLM namespace is refused typed.
    #[tokio::test]
    async fn the_system_prompt_renders_from_the_admitted_roots_recorded_config() {
        let deployment = RlmProtocolPluginConfig::builder()
            .channel(crate::RlmChannel::Cell)
            .instruction_limit(crate::plugin::InstructionBound::unbounded())
            .memory_limit(crate::plugin::MemoryBound::mebibytes(64))
            .build();
        let admitted = |prompt: lash_rlm_types::RlmPrompt, revision: u64| {
            let mut config = lash_core::PluginConfig::for_protocol(Some(
                crate::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            ));
            config.insert(
                crate::RLM_PROTOCOL_PLUGIN_ID,
                serde_json::to_value(crate::RlmRecordedConfig {
                    render: None,
                    termination: None,
                    final_answer_format: None,
                    channel: Some(crate::RlmChannel::Cell),
                    dialect: Some("typescript".to_string()),
                    behaviour: deployment.recorded_behaviour(false),
                    prompt,
                })
                .expect("recorded namespace"),
            );
            lash_core::AdmittedPluginConfig::new(config, revision)
        };
        let session = test_session(deployment.clone());
        session
            .apply_session_extension(crate::rlm_session_projection_extension(
                RlmProjectedBindings::new()
                    .bind_json("current_query", serde_json::json!("open issues"))
                    .expect("bind"),
            ))
            .await
            .expect("bind the session's read-only variable");
        let catalog = lash_core::ToolCatalog::from_tool_definitions(Vec::new());
        let subagent = lash_core::SubagentSessionContext {
            parent_session_id: "parent".into(),
            capability: "research".to_string(),
            depth: 1,
            max_depth: 3,
        };

        let running = admitted(lash_rlm_types::RlmPrompt::default(), 0);
        let next = admitted(
            lash_rlm_types::RlmPrompt {
                intro: lash_rlm_types::RlmPromptIntro::Host {
                    text: "You are the release assistant.".to_string(),
                },
                context: vec!["Release 4.2 freezes on Friday.".to_string()],
                ..lash_rlm_types::RlmPrompt::default()
            },
            1,
        );
        let running_prompt = session
            .system_prompt(&running, &catalog, Some(&subagent), TURN)
            .await
            .expect("the running root's prompt");
        assert!(running_prompt.starts_with(crate::RLM_BUILTIN_INTRO));
        assert!(!running_prompt.contains("## Context"));
        let next_prompt = session
            .system_prompt(&next, &catalog, Some(&subagent), TURN)
            .await
            .expect("the next root's prompt");
        assert!(next_prompt.starts_with("You are the release assistant.\n\n"));
        assert!(next_prompt.ends_with("## Context\n\nRelease 4.2 freezes on Friday."));
        for prompt in [&running_prompt, &next_prompt] {
            assert!(prompt.contains("### Read-Only Variables"));
            assert!(prompt.contains("- `current_query`: `string`, read-only"));
            assert!(prompt.contains("Subagent capability: research. Depth: 1/3."));
        }
        assert_eq!(
            session
                .system_prompt(&running, &catalog, Some(&subagent), TURN)
                .await
                .expect("a second render"),
            running_prompt,
            "a render is a function of its recorded inputs"
        );

        let unrecorded = lash_core::AdmittedPluginConfig::new(
            lash_core::PluginConfig::for_protocol(Some(crate::RLM_PROTOCOL_PLUGIN_ID.to_string())),
            0,
        );
        assert_eq!(
            session
                .system_prompt(&unrecorded, &catalog, None, TURN)
                .await
                .expect("a session with no recorded RLM namespace renders"),
            session
                .system_prompt(
                    &admitted(lash_rlm_types::RlmPrompt::default(), 0),
                    &catalog,
                    None,
                    TURN
                )
                .await
                .expect("the built-in prompt renders"),
            "a session that recorded no RLM namespace yet renders the built-in prompt"
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
            model: Some(lash_core::ModelConfig::new(lash_core::RecordedModel::mint(
                lash_core::ModelKey::from("budget-unit-model"),
                lash_core::ModelMetadata::builder("budget-unit-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model limits"),
            ))),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            )
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
            model: Some(lash_core::ModelConfig::new(lash_core::RecordedModel::mint(
                lash_core::ModelKey::from("budget-unit-model"),
                lash_core::ModelMetadata::builder("budget-unit-model")
                    .context_window_tokens(41_000)
                    .build()
                    .expect("model limits"),
            ))),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
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
            model: Some(lash_core::ModelConfig::new(lash_core::RecordedModel::mint(
                lash_core::ModelKey::from("realistic-41k-model"),
                lash_core::ModelMetadata::builder("realistic-41k-model")
                    .context_window_tokens(41_000)
                    .build()
                    .expect("model limits"),
            ))),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            )
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
            model: Some(lash_core::ModelConfig::new(lash_core::RecordedModel::mint(
                lash_core::ModelKey::from("budget-unit-model"),
                lash_core::ModelMetadata::builder("budget-unit-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model limits"),
            ))),
            ..lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            )
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
                prompt: Default::default(),
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
