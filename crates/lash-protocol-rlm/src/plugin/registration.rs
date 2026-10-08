use std::sync::Arc;

use super::RlmProtocolPluginConfig;
use super::prose_projector::RlmAssistantProseProjector;
use super::protocol_driver::RlmProtocolDriver;
use super::protocol_session::RlmProtocolSession;
use super::runtime_state::{RlmCodeExecutor, RlmRuntimeState};
use super::tool_args::normalize_projected_tool_args;
use crate::dialect::SessionDialect;
use crate::stream_mask;
use lash_core::plugin::{PluginError, PluginRegistrar};

pub(super) fn register_rlm_protocol_plugin(
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
    let code_executor = Arc::new(RlmCodeExecutor::new(Arc::clone(&runtime_state)));
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
    reg.output().transcript_projector(
        lash_core::hook_key!("rlm-transcript"),
        Arc::new(crate::projection::transcript::RlmTranscriptProjector),
    )?;
    reg.execution().code_executor(code_executor)?;
    reg.output()
        .assistant_prose_projector(Arc::new(RlmAssistantProseProjector {
            dialect: Arc::clone(&dialect),
        }))?;
    reg.protocol().protocol_driver(Arc::new(RlmProtocolDriver {
        config,
        dialect: Arc::clone(&dialect),
    }))?;
    reg.tools()
        .provider(Arc::new(crate::control_tools::RlmControlToolsProvider {
            vocabulary: dialect.prompt_vocabulary(),
        }))?;
    reg.tools().provider(Arc::new(
        lash_lashlang_runtime::process_create_tool_provider(
            dialect.language_id(),
            dialect.surface(),
            dialect.worker_service(),
        ),
    ))?;
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

    stream_mask::register_stream_mask(reg, dialect)?;
    Ok(())
}
