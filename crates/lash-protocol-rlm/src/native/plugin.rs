use std::sync::Arc;

use crate::dialect::{RlmDialect, RlmDialectRegistry};
use crate::driver::SharedPromptUsage;
use crate::plugin::RlmProtocolPluginConfig;
use crate::plugin::budget_warning::BudgetUsageObserver;
use crate::plugin::protocol_session::RlmProtocolSession;
use crate::plugin::runtime_state::{RlmCodeExecutor, RlmRuntimeState};
use crate::plugin::tool_args::normalize_projected_tool_args;
use crate::projection::{RLM_TURN_INPUT_PLUGIN_ID, RlmProjectionExtension};
use lash_core::plugin::{PluginError, PluginRegistrar};

#[allow(clippy::too_many_arguments)]
pub(super) fn register_native_plugin(
    reg: &mut PluginRegistrar,
    config: RlmProtocolPluginConfig,
    dialect_registry: RlmDialectRegistry,
    dialect: Arc<dyn RlmDialect>,
    last_prompt_usage: SharedPromptUsage,
) -> Result<(), PluginError> {
    // The catalog contribution carries the whole registry, not just the active
    // dialect: model-facing tool prose is authored once and served to every
    // dialect, so the neutrality guard has to know all of their words.
    let catalog_dialects = dialect_registry.clone();
    let discovery = config.discovery.clone();
    let discovery_dialect = Arc::clone(&dialect);
    let runtime_state = Arc::new(
        RlmRuntimeState::new(dialect_registry, Arc::clone(&dialect))
            .map_err(|err| PluginError::Session(err.to_string()))?,
    );
    let code_executor = Arc::new(RlmCodeExecutor::new(Arc::clone(&runtime_state)));
    let protocol_session = Arc::new(RlmProtocolSession::new(
        config.clone(),
        Arc::clone(&runtime_state),
    ));
    reg.protocol().session(protocol_session.clone())?;
    reg.execution().code_executor(code_executor)?;
    reg.output()
        .assistant_prose_projector(Arc::new(NativeProseProjector))?;
    reg.protocol()
        .protocol_driver(Arc::new(NativeProtocolDriver {
            config,
            dialect: Arc::clone(&dialect),
            last_prompt_usage: Arc::clone(&last_prompt_usage),
            bound_variables_prompt: runtime_state.shared_bound_variables_prompt(),
        }))?;
    reg.tools()
        .provider(Arc::new(crate::control_tools::RlmControlToolsProvider {
            vocabulary: dialect.prompt_vocabulary(),
        }))?;
    reg.tool_catalog().contribute(Arc::new(move |ctx| {
        crate::tool_catalog::validate_discovery(
            &ctx.tools,
            discovery.as_ref(),
            discovery_dialect.as_ref(),
        )?;
        crate::tool_catalog::rlm_tool_catalog(ctx, &catalog_dialects)
    }));
    reg.tool_calls().before(Arc::new(|ctx| {
        Box::pin(async move { normalize_projected_tool_args(ctx) })
    }));

    register_projected_bindings_prompt_contributor(reg, Arc::clone(&protocol_session));

    // Per-turn `prompt_usage` is captured here and passed to the projector via a
    // shared cell so the budget line can ride in the volatile turn-tail message
    // instead of poisoning the cached system prefix.
    reg.context().prepare_turn(
        10,
        Arc::new(BudgetUsageObserver {
            cell: last_prompt_usage,
        }),
    );

    let warn_session = protocol_session.clone();
    reg.turn().checkpoint(Arc::new(move |ctx| {
        let session = warn_session.clone();
        Box::pin(async move { session.soft_warn_directives(ctx) })
    }));

    reg.output().response(Arc::new(
        move |ctx: lash_core::plugin::AssistantResponseHookContext| {
            let dialect = Arc::clone(&dialect);
            Box::pin(async move {
                let parts = super::tool::assistant_parts(
                    lash_core::facade_support::normalized_response_parts(&ctx.response),
                );
                let events = if matches!(
                    super::tool::normalize(&parts),
                    super::tool::NativeAction::Execute { .. }
                ) {
                    [
                        dialect.stream_cell_start_event_name(),
                        dialect.stream_cell_end_event_name(),
                    ]
                    .into_iter()
                    .map(|name| lash_core::PluginRuntimeEvent::Custom {
                        name: name.to_string(),
                        payload: serde_json::json!({}),
                    })
                    .collect()
                } else {
                    Vec::new()
                };
                Ok(lash_core::plugin::AssistantResponseTransform {
                    response: ctx.response,
                    events,
                })
            })
        },
    ));
    Ok(())
}

fn register_projected_bindings_prompt_contributor(
    reg: &mut PluginRegistrar,
    protocol_session: Arc<RlmProtocolSession>,
) {
    reg.prompt().contribute(Arc::new(move |ctx| {
        let session = protocol_session.clone();
        Box::pin(async move {
            let mut contributions = session.projected_binding_prompt_contributions().await;
            if let Some(extension) = ctx
                .turn_context
                .plugin_input::<RlmProjectionExtension>(RLM_TURN_INPUT_PLUGIN_ID)
            {
                contributions.extend(RlmProjectionExtension::prompt_contributions_for(
                    &extension.bindings,
                    // The session owns the dialect; a turn-scoped extension
                    // built by a host before the dialect resolved must not
                    // fall back to Lashlang copy here.
                    session.dialect_prompt_vocabulary(),
                ));
            }
            Ok(contributions)
        })
    }));
}

/// Provider-native RLM protocol session, selected by the RLM factory config.
pub struct RlmNativeToolPlugin {
    pub(crate) config: RlmProtocolPluginConfig,
    pub(crate) dialect: Arc<dyn RlmDialect>,
    pub(crate) dialect_registry: RlmDialectRegistry,
    pub(crate) last_prompt_usage: SharedPromptUsage,
}
impl lash_core::plugin::SessionPlugin for RlmNativeToolPlugin {
    fn id(&self) -> &'static str {
        crate::plugin::RLM_PROTOCOL_PLUGIN_ID
    }
    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        register_native_plugin(
            reg,
            self.config.clone(),
            self.dialect_registry.clone(),
            Arc::clone(&self.dialect),
            Arc::clone(&self.last_prompt_usage),
        )
    }
}
struct NativeProseProjector;
impl lash_core::plugin::AssistantProseProjectorPlugin for NativeProseProjector {
    fn project_assistant_prose(&self, text: &str) -> String {
        text.to_string()
    }
}
struct NativeProtocolDriver {
    config: RlmProtocolPluginConfig,
    dialect: Arc<dyn RlmDialect>,
    last_prompt_usage: SharedPromptUsage,
    bound_variables_prompt: crate::rlm_support::SharedBoundVariablesPrompt,
}
impl lash_core::plugin::ProtocolDriverPlugin for NativeProtocolDriver {
    fn build_preamble(
        &self,
        input: lash_core::ProtocolBuildInput,
    ) -> lash_core::TurnDriverPreamble {
        super::projector::build_rlm_preamble_with_dialect(
            input,
            crate::driver::RlmPreambleConfig {
                discovery: self.config.discovery.clone(),
                max_output_chars: self.config.max_output_chars,
                max_budget_tokens: self.config.continue_as_soft_warn_tokens,
                last_prompt_usage: Arc::clone(&self.last_prompt_usage),
                prompt_features: self.config.prompt_features,
            },
            Arc::clone(&self.bound_variables_prompt),
            Arc::clone(&self.dialect),
        )
    }
}
