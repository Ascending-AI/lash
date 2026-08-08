use std::sync::{Arc, Mutex};

use lash_core::plugin::{
    CheckpointHookContext, PluginDirective, PluginError, ProtocolRuntimeContext,
    ProtocolSessionContext, ProtocolSessionMaterialization, ProtocolSessionPlugin,
};
use lash_core::{CheckpointKind, PluginOptions, ProtocolTurnOptions, SessionError};
use lash_rlm_types::{RlmCreateExtras, RlmFinalAnswerFormat};

use super::budget_warning::BUDGET_WARNING_STATUS;
use super::runtime_state::RlmRuntimeState;
use super::{RLM_PROTOCOL_PLUGIN_ID, RlmProtocolPluginConfig};

pub(super) struct RlmProtocolSession {
    config: RlmProtocolPluginConfig,
    runtime_state: Arc<RlmRuntimeState>,
    warned_at_threshold: Mutex<bool>,
}

impl RlmProtocolSession {
    pub(super) fn new(
        config: RlmProtocolPluginConfig,
        runtime_state: Arc<RlmRuntimeState>,
    ) -> Self {
        Self {
            runtime_state,
            config,
            warned_at_threshold: Mutex::new(false),
        }
    }

    pub(super) async fn projected_binding_prompt_contributions(
        &self,
    ) -> Vec<lash_core::PromptContribution> {
        self.runtime_state
            .projected_binding_prompt_contributions()
            .await
    }

    pub(super) fn soft_warn_directives(
        &self,
        ctx: CheckpointHookContext,
    ) -> Result<Vec<PluginDirective>, PluginError> {
        if ctx.checkpoint != CheckpointKind::AfterWork {
            return Ok(Vec::new());
        }
        let Some(threshold) = self.config.continue_as_soft_warn_tokens else {
            return Ok(Vec::new());
        };
        let used = ctx.state.token_usage().total().max(0) as usize;
        if used == 0 || used < threshold {
            return Ok(Vec::new());
        }
        let mut warned = self
            .warned_at_threshold
            .lock()
            .map_err(|_| PluginError::Session("rlm soft-warning state poisoned".to_string()))?;
        if *warned {
            return Ok(Vec::new());
        }
        *warned = true;
        Ok(vec![PluginDirective::emit_runtime_events(vec![
            lash_core::PluginRuntimeEvent::Status {
                key: BUDGET_WARNING_STATUS.to_string(),
                label: "context budget".to_string(),
                detail: Some(format!(
                    "{used} tokens used; warn at {threshold}; choose frame switch path"
                )),
            },
        ])])
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
        _ctx: ProtocolSessionContext<'_>,
        state: &lash_core::runtime::RuntimeSessionState,
    ) -> Result<(), SessionError> {
        self.runtime_state
            .restore_runtime_session_state(state)
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

    async fn validate_turn_extension(
        &self,
        extension: &lash_core::ProtocolTurnExtensionHandle,
    ) -> Result<(), SessionError> {
        self.runtime_state.validate_turn_extension(extension).await
    }

    fn configure_runtime_on_materialize(
        &self,
        mut ctx: ProtocolRuntimeContext<'_>,
        materialization: ProtocolSessionMaterialization<'_>,
    ) -> Result<(), SessionError> {
        let options = resolve_rlm_session_options(
            ctx.protocol_turn_options(),
            materialization.plugin_options,
            materialization.is_root_session,
        )?;
        ctx.set_protocol_turn_options_all_frames(options);
        Ok(())
    }
}

/// Apply-and-default RLM session options with apply-at-open semantics.
///
/// Starts from the existing durable options (preserving fields such as
/// termination that a prior open applied), overlays any explicit extras from the
/// materialization's plugin options, and defaults `final_answer_format` —
/// `Markdown` for root sessions, `RawFinalValue` for children — when none was
/// supplied explicitly.
pub(crate) fn resolve_rlm_session_options(
    existing: &ProtocolTurnOptions,
    plugin_options: &PluginOptions,
    is_root_session: bool,
) -> Result<ProtocolTurnOptions, SessionError> {
    let explicit = plugin_options
        .decode::<RlmCreateExtras>(RLM_PROTOCOL_PLUGIN_ID)
        .map_err(|err| SessionError::Protocol(format!("invalid RLM create options: {err}")))?;

    let mut extras = if existing.is_empty() {
        RlmCreateExtras::default()
    } else {
        existing
            .decode()
            .map_err(|err| SessionError::Protocol(err.to_string()))?
    };

    let explicit_format = match explicit {
        Some(explicit) => {
            extras.termination = explicit.termination;
            explicit.final_answer_format
        }
        None => None,
    };

    let default_format = if is_root_session {
        RlmFinalAnswerFormat::Markdown
    } else {
        RlmFinalAnswerFormat::RawFinalValue
    };
    extras.final_answer_format = Some(explicit_format.unwrap_or(default_format));

    Ok(ProtocolTurnOptions::typed(extras)?)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::plugin::budget_warning::BUDGET_WARNING_STATUS;
    use crate::projection::{ProjectionRegistry, RlmProjectedBindings};

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
            _session_id: &str,
        ) -> Result<lash_core::SessionSnapshot, lash_core::plugin::PluginError> {
            Err(lash_core::plugin::PluginError::Session(
                "not used".to_string(),
            ))
        }

        async fn tool_catalog(
            &self,
            _session_id: &str,
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

        async fn close_session(
            &self,
            _session_id: &str,
        ) -> Result<(), lash_core::plugin::PluginError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl lash_core::plugin::runtime_host::SessionGraphService for NoopPromptManager {}

    #[test]
    fn resolve_rlm_session_options_preserves_existing_termination() {
        let existing = ProtocolTurnOptions::typed(RlmCreateExtras {
            termination: lash_rlm_types::RlmTermination::Natural,
            final_answer_format: None,
        })
        .expect("existing options");

        let options = resolve_rlm_session_options(&existing, &PluginOptions::default(), true)
            .expect("resolve options");
        let extras: RlmCreateExtras = options.decode().expect("decode options");
        assert_eq!(extras.termination, lash_rlm_types::RlmTermination::Natural);
        assert_eq!(
            extras.final_answer_format,
            Some(RlmFinalAnswerFormat::Markdown)
        );
    }

    #[test]
    fn resolve_rlm_session_options_defaults_child_to_raw_final_value() {
        let options = resolve_rlm_session_options(
            &ProtocolTurnOptions::empty(),
            &PluginOptions::default(),
            false,
        )
        .expect("resolve options");
        let extras: RlmCreateExtras = options.decode().expect("decode options");
        assert_eq!(
            extras.final_answer_format,
            Some(RlmFinalAnswerFormat::RawFinalValue)
        );
    }

    #[test]
    fn resolve_rlm_session_options_applies_explicit_extras() {
        let plugin_options = PluginOptions::typed(
            RLM_PROTOCOL_PLUGIN_ID,
            RlmCreateExtras {
                termination: lash_rlm_types::RlmTermination::FinishRequired { schema: None },
                final_answer_format: Some(RlmFinalAnswerFormat::RawFinalValue),
            },
        )
        .expect("plugin options");

        let options =
            resolve_rlm_session_options(&ProtocolTurnOptions::empty(), &plugin_options, false)
                .expect("resolve options");
        let extras: RlmCreateExtras = options.decode().expect("decode options");
        assert_eq!(
            extras.termination,
            lash_rlm_types::RlmTermination::FinishRequired { schema: None }
        );
        assert_eq!(
            extras.final_answer_format,
            Some(RlmFinalAnswerFormat::RawFinalValue)
        );
    }

    #[test]
    fn resolve_rlm_session_options_rejects_malformed_extras() {
        let mut plugin_options = PluginOptions::default();
        plugin_options.plugins.insert(
            RLM_PROTOCOL_PLUGIN_ID.to_string(),
            serde_json::json!({ "termination": { "kind": "unknown" } }),
        );
        let err =
            resolve_rlm_session_options(&ProtocolTurnOptions::empty(), &plugin_options, false)
                .expect_err("malformed extras should error");
        assert!(err.to_string().contains("invalid RLM create options"));
    }

    fn test_session(config: RlmProtocolPluginConfig) -> RlmProtocolSession {
        let runtime_state = Arc::new(
            RlmRuntimeState::new(
                Arc::new(ProjectionRegistry::default()),
                lashlang::global_in_memory_lashlang_artifact_store(),
                lash_lashlang_runtime::LashlangSurface::default(),
                None,
                crate::executor::RlmLashlangExecutionTraceConfig::default(),
            )
            .expect("runtime state"),
        );
        RlmProtocolSession::new(config, runtime_state)
    }

    #[tokio::test]
    async fn session_projection_extension_rejects_duplicate_names() {
        let session = test_session(RlmProtocolPluginConfig::default());
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
        let session = test_session(RlmProtocolPluginConfig::default());
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
            ..Default::default()
        });
        let state = lash_core::SessionSnapshot {
            token_usage: lash_core::TokenUsage {
                input_tokens: 120_292,
                ..Default::default()
            },
            ..Default::default()
        };
        let directives = session
            .soft_warn_directives(lash_core::plugin::CheckpointHookContext {
                session_id: "root".to_string(),
                checkpoint: lash_core::CheckpointKind::AfterWork,
                state: lash_core::SessionReadView::from_snapshot(&state),
                sessions: Arc::new(NoopPromptManager),
                session_lifecycle: Arc::new(NoopPromptManager),
                session_graph: Arc::new(NoopPromptManager),
            })
            .expect("warning directives");

        assert_eq!(directives.len(), 1);
        let lash_core::plugin::PluginDirective::EmitRuntimeEvents { events } = &directives[0]
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
            text.contains("120292 tokens used") && text.contains("choose frame switch path")
        }));
    }
}
