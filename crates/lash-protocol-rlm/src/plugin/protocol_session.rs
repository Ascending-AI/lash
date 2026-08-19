use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use lash_core::plugin::{
    CheckpointHookContext, PluginDirective, PluginError, ProtocolRuntimeContext,
    ProtocolSessionContext, ProtocolSessionMaterialization, ProtocolSessionPlugin,
};
use lash_core::{CheckpointKind, PluginOptions, ProtocolTurnOptions, SessionError};
use lash_rlm_types::{
    RlmCreateExtras, RlmFinalAnswerFormat, RlmSessionConfig, RlmSessionConfigConflict,
};

use super::budget_warning::BUDGET_WARNING_STATUS;
use super::runtime_state::RlmRuntimeState;
use super::{RLM_PROTOCOL_PLUGIN_ID, RlmProtocolPluginConfig};

pub(super) struct RlmProtocolSession {
    config: RlmProtocolPluginConfig,
    runtime_state: Arc<RlmRuntimeState>,
    warned_at_threshold: Mutex<bool>,
}

impl RlmProtocolSession {
    pub(crate) fn dialect_prompt_vocabulary(&self) -> crate::dialect::DialectPromptVocabulary {
        self.runtime_state.dialect_prompt_vocabulary()
    }

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
        let mut warned = self.warned_at_threshold.lock_recover();
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
    let extras = options
        .decode::<RlmCreateExtras>()
        .map_err(|err| RlmSessionConfigDecodeError(err.to_string()))?;
    Ok(RlmSessionConfig::from(&extras))
}

/// A recorded RLM options bag that could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RlmSessionConfigDecodeError(pub String);

impl std::fmt::Display for RlmSessionConfigDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid RLM session config: {}", self.0)
    }
}

impl std::error::Error for RlmSessionConfigDecodeError {}

/// The guarded set-if-unset write for the durable RLM bag (ADR 0066).
///
/// Every field the request states is written only while the session has
/// recorded nothing for it. Restating the value already recorded is a no-op
/// rather than an error — set-if-unset is idempotent, which is what lets a host
/// call it on every open. Stating a *different* value is refused with a typed
/// [`RlmSessionConfigConflict`]; there is no smoothing path, by design.
///
/// Fields the request leaves unstated are carried through untouched. That is
/// the whole of the two clobber fixes: options that name a dialect no longer
/// restate the termination, and a reopen that names no format no longer resets
/// one the session recorded.
pub fn apply_rlm_session_config_if_unset(
    existing: &RlmSessionConfig,
    requested: &RlmSessionConfig,
) -> Result<RlmSessionConfig, RlmSessionConfigConflict> {
    let mut next = existing.clone();
    if let Some(requested) = requested.dialect {
        match existing.dialect {
            Some(recorded) if recorded != requested => {
                return Err(RlmSessionConfigConflict::Dialect {
                    recorded,
                    requested,
                });
            }
            Some(_) => {}
            None => next.dialect = Some(requested),
        }
    }
    if let Some(requested) = requested.final_answer_format.clone() {
        match existing.final_answer_format.clone() {
            Some(recorded) if recorded != requested => {
                return Err(RlmSessionConfigConflict::FinalAnswerFormat {
                    recorded,
                    requested,
                });
            }
            Some(_) => {}
            None => next.final_answer_format = Some(requested),
        }
    }
    if let Some(requested) = requested.termination.clone() {
        match existing.termination.clone() {
            Some(recorded) if recorded != requested => {
                return Err(RlmSessionConfigConflict::Termination {
                    recorded: Box::new(recorded),
                    requested: Box::new(requested),
                });
            }
            Some(_) => {}
            None => next.termination = Some(requested),
        }
    }
    Ok(next)
}

/// The guarded set-if-unset write as an *open session* may perform it.
///
/// Same engine as [`apply_rlm_session_config_if_unset`], with one field held
/// back: the dialect is compared and never written. An open session already
/// picked its dialect implementation when its plugins were built, and a session
/// whose bag records no dialect resolved the default one, so the comparison is
/// against `existing.dialect.unwrap_or_default()` — the dialect the session is
/// *running*. Writing it here would leave the recorded fact disagreeing with
/// the plugin that is executing, which is exactly the divergence the typed pin
/// exists to prevent.
pub fn apply_rlm_session_config_post_open(
    existing: &RlmSessionConfig,
    requested: &RlmSessionConfig,
) -> Result<RlmSessionConfig, RlmSessionConfigConflict> {
    if let Some(requested) = requested.dialect {
        let running = existing.dialect.unwrap_or_default();
        if running != requested {
            return Err(RlmSessionConfigConflict::Dialect {
                recorded: running,
                requested,
            });
        }
    }
    apply_rlm_session_config_if_unset(
        existing,
        &RlmSessionConfig {
            dialect: None,
            ..requested.clone()
        },
    )
}

/// Encode a config back into the durable options bag.
pub fn rlm_session_config_options(
    config: &RlmSessionConfig,
) -> Result<ProtocolTurnOptions, SessionError> {
    Ok(ProtocolTurnOptions::typed(RlmCreateExtras::from(config))?)
}

/// Materialize the durable RLM options for a session that is opening.
///
/// Applies the materialization's plugin options as a guarded set-if-unset write
/// over whatever the session already recorded. On a session that has recorded
/// nothing yet — and only then — the two facts the runtime cannot start without
/// are filled from their defaults: the language, and the presentation format
/// the prompt is written against (`Markdown` for root sessions,
/// `RawFinalValue` for children). That is what pins a session's dialect at its
/// first open, as it always has.
///
/// A session that has recorded something is never *re*-defaulted. A reopen that
/// states nothing therefore carries every recorded fact through untouched,
/// which is the whole of the two clobber fixes: a reopen no longer resets the
/// recorded final-answer format, and options that state only a dialect no
/// longer restate the termination.
pub(crate) fn resolve_rlm_session_options(
    existing: &ProtocolTurnOptions,
    plugin_options: &PluginOptions,
    is_root_session: bool,
) -> Result<ProtocolTurnOptions, SessionError> {
    let mut resolved = guarded_session_config(existing, plugin_options)?;

    if existing.is_empty() {
        resolved.dialect = Some(resolved.dialect.unwrap_or_default());
        resolved.final_answer_format = Some(resolved.final_answer_format.unwrap_or({
            if is_root_session {
                RlmFinalAnswerFormat::Markdown
            } else {
                RlmFinalAnswerFormat::RawFinalValue
            }
        }));
    }

    rlm_session_config_options(&resolved)
}

/// The config a session materializes with: what it recorded, with the
/// materialization's plugin options applied as a guarded set-if-unset write.
fn guarded_session_config(
    existing: &ProtocolTurnOptions,
    plugin_options: &PluginOptions,
) -> Result<RlmSessionConfig, SessionError> {
    let recorded =
        rlm_session_config(existing).map_err(|err| SessionError::Protocol(err.to_string()))?;
    let requested = plugin_options
        .decode::<RlmCreateExtras>(RLM_PROTOCOL_PLUGIN_ID)
        .map_err(|err| SessionError::Protocol(format!("invalid RLM create options: {err}")))?
        .map(|extras| RlmSessionConfig::from(&extras))
        .unwrap_or_default();
    apply_rlm_session_config_if_unset(&recorded, &requested)
        .map_err(|conflict| SessionError::Protocol(conflict.to_string()))
}

/// The one language this session is allowed to use, for the plugin build that
/// has to pick a dialect implementation before the session materializes.
pub(super) fn resolve_rlm_session_dialect(
    existing: &ProtocolTurnOptions,
    plugin_options: &PluginOptions,
) -> Result<lash_rlm_types::RlmDialect, SessionError> {
    Ok(guarded_session_config(existing, plugin_options)?
        .dialect
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::plugin::budget_warning::BUDGET_WARNING_STATUS;
    use crate::projection::RlmProjectedBindings;
    use lash_rlm_types::RlmDialect;

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

    /// A session that recorded something is never re-defaulted: the reopen
    /// carries every recorded fact through and adds nothing it did not state.
    #[test]
    fn resolve_rlm_session_options_preserves_existing_termination() {
        let existing = ProtocolTurnOptions::typed(RlmCreateExtras {
            dialect: None,
            termination: Some(lash_rlm_types::RlmTermination::Natural),
            final_answer_format: None,
        })
        .expect("existing options");

        let options = resolve_rlm_session_options(&existing, &PluginOptions::default(), true)
            .expect("resolve options");
        let extras: RlmCreateExtras = options.decode().expect("decode options");
        assert_eq!(
            extras.termination,
            Some(lash_rlm_types::RlmTermination::Natural)
        );
        assert_eq!(
            extras.dialect, None,
            "a reopen must not invent a pin the session never recorded"
        );
        assert_eq!(
            extras.final_answer_format, None,
            "a reopen must not invent a format the session never recorded"
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
        assert_eq!(extras.dialect, Some(RlmDialect::Lashlang));
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
                dialect: None,
                termination: Some(lash_rlm_types::RlmTermination::FinishRequired { schema: None }),
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
            Some(lash_rlm_types::RlmTermination::FinishRequired { schema: None })
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

    #[test]
    fn create_time_typescript_choice_is_recorded_and_cannot_change_on_reopen() {
        let requested = PluginOptions::typed(
            RLM_PROTOCOL_PLUGIN_ID,
            RlmCreateExtras {
                dialect: Some(RlmDialect::Typescript),
                ..RlmCreateExtras::default()
            },
        )
        .expect("typescript options");
        let recorded = resolve_rlm_session_options(&ProtocolTurnOptions::empty(), &requested, true)
            .expect("record TypeScript");
        let extras: RlmCreateExtras = recorded.decode().expect("decode recorded options");
        assert_eq!(extras.dialect, Some(RlmDialect::Typescript));

        let mismatch = PluginOptions::typed(
            RLM_PROTOCOL_PLUGIN_ID,
            RlmCreateExtras {
                dialect: Some(RlmDialect::Lashlang),
                ..RlmCreateExtras::default()
            },
        )
        .expect("lashlang options");
        let error = resolve_rlm_session_options(&recorded, &mismatch, true)
            .expect_err("a durable dialect cannot change on reopen");
        assert!(
            error.to_string().contains(
                "RLM session dialect is durably pinned to `typescript` and cannot be set to `lashlang`"
            ),
            "the refusal must render the one typed message: {error}"
        );
    }

    /// FIG-1555 clobber 1: a reopen that names no format must keep the one the
    /// session recorded, instead of resetting it to the root default.
    #[test]
    fn reopen_without_an_explicit_format_keeps_the_recorded_final_answer_format() {
        let existing = ProtocolTurnOptions::typed(RlmCreateExtras {
            dialect: Some(RlmDialect::Lashlang),
            termination: Some(lash_rlm_types::RlmTermination::Natural),
            final_answer_format: Some(RlmFinalAnswerFormat::RawFinalValue),
        })
        .expect("existing options");

        let options = resolve_rlm_session_options(&existing, &PluginOptions::default(), true)
            .expect("resolve options");
        let extras: RlmCreateExtras = options.decode().expect("decode options");
        assert_eq!(
            extras.final_answer_format,
            Some(RlmFinalAnswerFormat::RawFinalValue),
            "a reopen that states no format must not reset the recorded one"
        );
    }

    /// FIG-1555 clobber 2: options that state a dialect and nothing else must
    /// not reset a recorded `FinishRequired` termination to the default.
    #[test]
    fn stating_only_a_dialect_keeps_the_recorded_termination() {
        let existing = ProtocolTurnOptions::typed(RlmCreateExtras {
            dialect: None,
            termination: Some(lash_rlm_types::RlmTermination::FinishRequired { schema: None }),
            final_answer_format: None,
        })
        .expect("existing options");
        let requested = PluginOptions::typed(
            RLM_PROTOCOL_PLUGIN_ID,
            RlmCreateExtras {
                dialect: Some(RlmDialect::Lashlang),
                ..RlmCreateExtras::default()
            },
        )
        .expect("dialect-only options");

        let options =
            resolve_rlm_session_options(&existing, &requested, true).expect("resolve options");
        let extras: RlmCreateExtras = options.decode().expect("decode options");
        assert_eq!(
            extras.termination,
            Some(lash_rlm_types::RlmTermination::FinishRequired { schema: None }),
            "stating a dialect must not silently restate the termination"
        );
    }

    fn test_session(config: RlmProtocolPluginConfig) -> RlmProtocolSession {
        let runtime_state =
            Arc::new(RlmRuntimeState::new_lashlang_for_tests().expect("runtime state"));
        RlmProtocolSession::new(config, runtime_state)
    }

    #[tokio::test]
    async fn session_projection_extension_rejects_duplicate_names() {
        let session = test_session(RlmProtocolPluginConfig::new(
            lashlang::ExecutionBound::Unbounded,
            lashlang::ExecutionBound::Unbounded,
            lashlang::ExecutionBound::instructions(64 * 1024 * 1024),
        ));
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
        let session = test_session(RlmProtocolPluginConfig::new(
            lashlang::ExecutionBound::Unbounded,
            lashlang::ExecutionBound::Unbounded,
            lashlang::ExecutionBound::instructions(64 * 1024 * 1024),
        ));
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
            ..RlmProtocolPluginConfig::new(
                lashlang::ExecutionBound::Unbounded,
                lashlang::ExecutionBound::Unbounded,
                lashlang::ExecutionBound::instructions(64 * 1024 * 1024),
            )
        });
        let state = lash_core::SessionSnapshot {
            token_usage: lash_core::TokenUsage {
                input_tokens: 120_292,
                ..Default::default()
            },
            ..lash_core::SessionSnapshot::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
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

    /// A bag that records no dialect still belongs to a session running the
    /// default one, so a post-open statement is compared against that default
    /// and refused when it disagrees -- never written.
    #[test]
    fn a_post_open_dialect_is_compared_against_the_running_default() {
        let bag_without_a_dialect = RlmSessionConfig::new()
            .termination(lash_rlm_types::RlmTermination::FinishRequired { schema: None });
        assert_eq!(bag_without_a_dialect.dialect, None);

        let conflict = apply_rlm_session_config_post_open(
            &bag_without_a_dialect,
            &RlmSessionConfig::new().dialect(RlmDialect::Typescript),
        )
        .expect_err("a session running the default dialect cannot be moved onto another");
        assert_eq!(
            conflict,
            RlmSessionConfigConflict::Dialect {
                recorded: RlmDialect::Lashlang,
                requested: RlmDialect::Typescript,
            },
            "the conflict names the dialect the session is running, not an absent one"
        );
    }

    /// Agreeing with the running default is a no-op: it records nothing, so the
    /// read half keeps reporting that the session stated no dialect.
    #[test]
    fn agreeing_with_the_running_default_dialect_writes_nothing() {
        let bag_without_a_dialect = RlmSessionConfig::new();

        let resolved = apply_rlm_session_config_post_open(
            &bag_without_a_dialect,
            &RlmSessionConfig::new().dialect(RlmDialect::Lashlang),
        )
        .expect("stating the dialect the session is running is a no-op");
        assert_eq!(resolved, bag_without_a_dialect);
        assert_eq!(resolved.dialect, None);
    }

    /// The held-back dialect does not hold back the rest: a statement that
    /// agrees on the dialect still writes the facts the bag has not recorded.
    #[test]
    fn a_post_open_write_still_lands_on_the_facts_that_are_unset() {
        let recorded = RlmSessionConfig::new().dialect(RlmDialect::Typescript);

        let resolved = apply_rlm_session_config_post_open(
            &recorded,
            &RlmSessionConfig::new()
                .dialect(RlmDialect::Typescript)
                .termination(lash_rlm_types::RlmTermination::FinishRequired { schema: None }),
        )
        .expect("an unrecorded termination accepts a write");
        assert_eq!(
            resolved.termination,
            Some(lash_rlm_types::RlmTermination::FinishRequired { schema: None })
        );
        assert_eq!(resolved.dialect, Some(RlmDialect::Typescript));
    }
}
