use std::sync::Arc;

use super::prose_projector::RlmAssistantProseProjector;
use super::protocol_driver::RlmProtocolDriver;

use super::tool_args::normalize_projected_tool_args;

use lash_core::plugin::PluginRegistrar;
use lash_execution_contracts::plugin_error::PluginError;

pub(super) fn register_rlm_protocol_plugin(
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
    reg.output()
        .stream(lash_core::hook_key!("cell-mask"), |owner, _| {
            Ok(Arc::clone(&owner.mask.stream))
        })?;
    reg.output()
        .stream_finished(lash_core::hook_key!("cell-mask"), |owner, _| {
            Ok(Arc::clone(&owner.mask.finished))
        })?;
    reg.output().response(
        lash_core::hook_key!("cell-mask"),
        Some(lash_core::hook_key!("cell-mask")),
        |owner, _| Ok(Arc::clone(&owner.mask.response)),
    )?;

    reg.protocol().session(move |owner, _| {
        let protocol_session = Arc::clone(&owner.protocol_session);
        Ok(protocol_session.clone())
    })?;
    reg.execution().code_executor(move |owner, _| {
        let code_executor = Arc::clone(&owner.code_executor);
        Ok(code_executor)
    })?;
    reg.output().assistant_prose_projector(move |owner, _| {
        let dialect = Arc::clone(&owner.dialect);
        Ok(Arc::new(RlmAssistantProseProjector {
            dialect: Arc::clone(&dialect),
        }))
    })?;
    reg.protocol().protocol_driver(move |owner, _| {
        let config = owner.config.clone();
        let dialect = Arc::clone(&owner.dialect);
        Ok(Arc::new(RlmProtocolDriver {
            config,
            dialect: Arc::clone(&dialect),
        }))
    })?;
    crate::control_tools::register(reg)?;
    reg.tool_catalog()
        .contribute(lash_core::hook_key!("rlm-catalog"), move |owner, _| {
            let catalog_dialect = Arc::clone(&owner.dialect);
            let discovery = owner.config.discovery.clone();
            Ok(Arc::new(move |ctx| {
                crate::tool_catalog::validate_discovery(&ctx.tools, discovery.as_ref())?;
                crate::tool_catalog::rlm_tool_catalog(ctx, &catalog_dialect)
            }))
        })?;
    reg.tool_calls()
        .transform_args(super::PROJECTED_ARGS, move |_owner, _| {
            Ok(Arc::new(|input| {
                Box::pin(async move { normalize_projected_tool_args(input) })
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
    Ok(())
}
