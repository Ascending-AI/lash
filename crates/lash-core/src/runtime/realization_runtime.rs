//! Intent execution under the declaring Run's owner, in an independent journal.
use super::session_manager::{RealizationServicesPorts, RuntimeSessionServices};
use crate::tool_dispatch::{IntentRealizationContext, RealizationReceipt, RealizationRequest};
use std::sync::Arc;

/// Realize an admitted final using its recorded dispatch and this deployment's ports.
pub async fn realize_tool_intents(
    ports: super::ProcessRuntimePorts,
    request: RealizationRequest,
    scoped: crate::ActorContext,
) -> Result<RealizationReceipt, crate::RuntimeEffectControllerError> {
    let dispatch = request.payload.dispatch.ok_or_else(|| {
        crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeToolRunShape,
            "an intent realization has no recorded dispatch",
        )
    })?;
    let plugins = ports.plugin_host.isolated_registry().defer_session(
        crate::plugin::PluginSessionRequest {
            owner: dispatch.owner.runtime_owner(),
            parent_session_id: None,
            materialization: crate::plugin::PluginSessionMaterializationRequest::Creation {
                config: crate::plugin::SessionAuthorityContext {
                    plugin_config: dispatch.environment.plugin_config.clone(),
                    ..Default::default()
                },
                seed_snapshot: None,
            },
            tool_catalog_overlay: Default::default(),
            tool_snapshot: None,
        },
    )?;
    if let Some(admission) = dispatch.plugin_admission {
        ports.plugin_host.validate_plugin_admission(&admission)?;
        plugins.adopt_plugin_admission(admission);
    }
    let mut core = ports.host;
    let attachments = Arc::clone(&core.durability.attachment_store);
    core.durability.attachment_store = Arc::new(
        crate::RuntimeAttachmentStore::new_with_clock(
            Arc::clone(attachments.backend()),
            core.backend().attachment_referrers(),
            dispatch.owner.runtime_owner(),
            Arc::clone(&core.clock),
        )
        .with_max_attachment_bytes(attachments.max_attachment_bytes())
        .with_read_policy(attachments.read_policy())
        .with_upload_expiry_ms(attachments.upload_expiry_ms())
        .with_output_retention(attachments.output_retention()),
    );
    let process_engines = core.process_engines.clone();
    let services = Arc::new(RuntimeSessionServices::for_realization(
        dispatch.owner.clone(),
        RealizationServicesPorts {
            environment: dispatch.environment,
            host: super::host::RuntimeHost {
                core,
                work: super::host::RuntimeWork::processes(ports.process_work, ports.queued_work),
            },
            plugins,
            runtime_lease_owner: ports.lease_owner,
            turn_phase_probe: ports.turn_phase_probe,
        },
    ));
    let context = IntentRealizationContext {
        effect_controller: scoped,
        owner: dispatch.owner,
        processes: services.process_service(),
        trigger_router: services.trigger_router(),
        process_engines,
        parent_invocation: dispatch.parent_invocation,
        process_lineage: dispatch.process_lineage,
        process_originator: dispatch.process_originator,
        run: std::marker::PhantomData,
    };
    let outcomes = crate::tool_dispatch::execute_final_tool_intents(
        &context,
        &request.call_id,
        &request.payload.intents,
        None,
    )
    .await?;
    Ok(RealizationReceipt { outcomes })
}
