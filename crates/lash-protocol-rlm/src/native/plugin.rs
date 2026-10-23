use std::sync::Arc;

use crate::dialect::SessionDialect;
use crate::plugin::RlmProtocolPluginConfig;

use lash_core::plugin::PluginRegistrar;
use lash_execution_contracts::plugin_error::PluginError;

pub(crate) fn register_native_plugin(
    reg: &mut PluginRegistrar<crate::plugin::RlmProtocolPlugin>,
) -> Result<(), PluginError> {
    crate::prompt_sections::register_sections(reg, |owner| {
        crate::prompt_sections::RlmSectionBehaviour {
            dialect: Arc::clone(&owner.dialect),
            channel: owner.config.channel,
            prompt_features: owner.config.prompt_features,
            discovery: owner.config.discovery.clone(),
            budget_tokens: owner.config.continue_as_soft_warn_tokens,
        }
    })?;
    reg.protocol().session(move |owner, _| {
        let protocol_session = Arc::clone(&owner.protocol_session);
        Ok(protocol_session.clone())
    })?;
    reg.execution().code_executor(move |owner, _| {
        let code_executor = Arc::clone(&owner.code_executor);
        Ok(code_executor)
    })?;
    reg.output()
        .assistant_prose_projector(move |_owner, _| Ok(Arc::new(NativeProseProjector)))?;
    reg.protocol().protocol_driver(move |owner, _| {
        let config = owner.config.clone();
        let dialect = Arc::clone(&owner.dialect);
        Ok(Arc::new(NativeProtocolDriver {
            config,
            dialect: Arc::clone(&dialect),
        }))
    })?;
    crate::control_tools::register(reg)?;
    crate::history_tools::register(reg)?;
    reg.tool_catalog()
        .contribute(lash_core::hook_key!("rlm-catalog"), move |owner, _| {
            let catalog_dialect = Arc::clone(&owner.dialect);
            let discovery = owner.config.discovery.clone();
            Ok(Arc::new(move |ctx| {
                crate::tool_catalog::validate_discovery(&ctx.tools, discovery.as_ref())?;
                crate::tool_catalog::rlm_tool_catalog(ctx, &catalog_dialect)
            }))
        })?;
    reg.turn()
        .checkpoint(lash_core::hook_key!("soft-warnings"), move |owner, _| {
            let warn_session = Arc::clone(&owner.protocol_session);
            Ok(Arc::new(move |ctx| {
                let session = warn_session.clone();
                Box::pin(async move { session.soft_warn_contributions(ctx) })
            }))
        })?;
    reg.output().response(
        lash_core::hook_key!("native-cell-events"),
        None,
        move |owner, _| {
            let dialect = Arc::clone(&owner.dialect);
            Ok(Arc::new(
                move |ctx: lash_core::plugin::AssistantResponseHookContext| {
                    let dialect = Arc::clone(&dialect);
                    Box::pin(async move {
                        let parts =
                            lash_core::facade_support::normalized_response_parts(&ctx.response);
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
            ))
        },
    )?;
    Ok(())
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
    fn attachment_omission_text(
        &self,
        notice: &lash_protocol_types::AttachmentMaterializationNotice,
    ) -> String {
        self.dialect
            .tool_presenter()
            .presenter()
            .attachment_omission_text(notice)
    }

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
            params: serde_json::to_value(resolved)
                .map_err(|error| {
                    lash_core::RenderRefusal::new(&crate::plugin::RlmRenderRefusal::Unencodable {
                        message: error.to_string(),
                    })
                })?
                .into(),
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
