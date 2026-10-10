use std::sync::Arc;

use crate::dialect::SessionDialect;
use crate::plugin::RlmProtocolPluginConfig;
use crate::plugin::protocol_session::RlmProtocolSession;
use crate::plugin::runtime_state::{CodeModeCodeExecutor, RlmRuntimeState};
use crate::plugin::tool_args::normalize_projected_tool_args;
use lash_core::plugin::{PluginError, PluginRegistrar};

pub(super) fn register_native_plugin(
    reg: &mut PluginRegistrar,
    config: RlmProtocolPluginConfig,
    dialect: Arc<SessionDialect>,
) -> Result<(), PluginError> {
    // The catalog contribution carries the dialect that spells every catalog
    // member's call path.
    let catalog_dialect = Arc::clone(&dialect);
    let discovery = config.discovery.clone();
    let discovery_dialect = Arc::clone(&dialect);
    let runtime_state = Arc::new(
        RlmRuntimeState::new(Arc::clone(&dialect))
            .map_err(|err| PluginError::Session(err.to_string()))?,
    );
    let code_executor = Arc::new(CodeModeCodeExecutor::new(Arc::clone(&runtime_state)));
    let protocol_session = Arc::new(RlmProtocolSession::new(
        config.clone(),
        Arc::clone(&runtime_state),
    ));
    reg.protocol().session(protocol_session.clone())?;
    crate::prompt_sections::register_sections(
        reg,
        crate::prompt_sections::RlmSectionBehaviour {
            dialect: Arc::clone(&dialect),
            channel: config.channel,
            prompt_features: config.prompt_features,
            discovery: config.discovery.clone(),
            budget_tokens: config.continue_as_soft_warn_tokens,
        },
    )?;
    reg.execution().code_executor(code_executor)?;
    reg.output()
        .assistant_prose_projector(Arc::new(NativeProseProjector))?;
    reg.protocol()
        .protocol_driver(Arc::new(NativeProtocolDriver {
            config,
            dialect: Arc::clone(&dialect),
        }))?;
    reg.tools()
        .provider(Arc::new(crate::control_tools::RlmControlToolsProvider {
            vocabulary: dialect.prompt_vocabulary(),
        }))?;
    reg.tools()
        .provider(Arc::new(crate::control_tools::finish_tool_provider(
            dialect.prompt_vocabulary(),
        )))?;
    reg.tool_catalog().contribute(
        lash_core::hook_key!("rlm-catalog"),
        Arc::new(move |ctx| {
            crate::tool_catalog::validate_discovery(
                &ctx.tools,
                discovery.as_ref(),
                discovery_dialect.as_ref(),
            )?;
            crate::tool_catalog::rlm_tool_catalog(ctx, &catalog_dialect)
        }),
    )?;
    reg.tool_calls().transform_args(
        lash_core::hook_key!("projected-args"),
        Arc::new(|input| Box::pin(async move { normalize_projected_tool_args(input) })),
    )?;

    let warn_session = protocol_session.clone();
    reg.turn().checkpoint(
        lash_core::hook_key!("soft-warnings"),
        Arc::new(move |ctx| {
            let session = warn_session.clone();
            Box::pin(async move { session.soft_warn_contributions(ctx) })
        }),
    )?;

    reg.output().response(
        lash_core::hook_key!("native-cell-events"),
        None,
        Arc::new(
            move |ctx: lash_core::plugin::AssistantResponseHookContext| {
                let dialect = Arc::clone(&dialect);
                Box::pin(async move {
                    let parts = lash_core::facade_support::normalized_response_parts(&ctx.response);
                    let events = if matches!(
                        super::tool::normalize_output(&parts),
                        super::tool::NativeAction::Execute { .. }
                    ) {
                        [
                            dialect.stream_cell_start_event_name(),
                            dialect.stream_cell_end_event_name(),
                        ]
                        .into_iter()
                        .map(|name| lash_core::PluginRuntimeEvent::Custom {
                            name,
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
        ),
    )?;
    Ok(())
}

/// Provider-native RLM protocol session, selected by the RLM factory config.
pub struct RlmNativeToolPlugin {
    pub(crate) config: RlmProtocolPluginConfig,
    pub(crate) dialect: Arc<SessionDialect>,
}
impl lash_core::plugin::SessionPlugin for RlmNativeToolPlugin {
    fn id(&self) -> &'static str {
        crate::plugin::RLM_PROTOCOL_PLUGIN_ID
    }
    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        register_native_plugin(reg, self.config.clone(), Arc::clone(&self.dialect))
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
    dialect: Arc<SessionDialect>,
}
impl lash_core::plugin::ProtocolDriverPlugin for NativeProtocolDriver {
    fn resolve_render(
        &self,
        namespace: &lash_core::ProtocolTurnOptions,
    ) -> Result<Option<lash_core::RecordedRender>, lash_core::RenderFault> {
        let recorded = crate::plugin::RlmRecordedConfig::read(namespace).map_err(|error| {
            lash_core::RecordedNamespaceCorrupt {
                owner: crate::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                message: error.to_string(),
            }
        })?;
        let resolved = crate::render::ResolvedRlmRender::resolve(
            &self.config.render,
            &recorded
                .and_then(|recorded| recorded.render)
                .unwrap_or_default(),
        );
        Ok(Some(lash_core::RecordedRender {
            renderer_id: self.config.code_renderer.0.id().to_string(),
            params: serde_json::to_value(resolved).map_err(|error| {
                lash_core::RenderRefusal::new(&crate::plugin::RlmRenderRefusal::Unencodable {
                    message: error.to_string(),
                })
            })?,
        }))
    }

    fn build_preamble(
        &self,
        input: lash_core::ProtocolBuildInput,
    ) -> lash_core::TurnDriverPreamble {
        super::projector::build_rlm_preamble_with_dialect(
            input,
            crate::driver::RlmPreambleConfig {
                max_output_chars: self.config.max_output_chars,
            },
            Arc::clone(&self.dialect),
        )
    }
}
