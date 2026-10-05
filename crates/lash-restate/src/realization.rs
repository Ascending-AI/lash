#![allow(
    deprecated,
    reason = "the named service binding uses the SDK trait dispatcher"
)]

//! ADR 0130: the durable invocation journal of one admitted final's intents.
use crate::compat::{Call, Reply};
use lash_core::tool_dispatch::{RealizationReceipt, RealizationRequest, ToolRealizer};
use restate_sdk::context::Context;
use restate_sdk::errors::HandlerResult;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct LashToolRealizationImpl {
    realizer: Arc<dyn ToolRealizer>,
    authority: crate::RestateAuthorityId,
    generation: lash_core::engine::BuildGeneration,
    namespace: crate::RestateNamespace,
}
impl LashToolRealizationImpl {
    pub(crate) fn new(
        realizer: Arc<dyn ToolRealizer>,
        authority: crate::RestateAuthorityId,
        generation: lash_core::engine::BuildGeneration,
        namespace: crate::RestateNamespace,
    ) -> Self {
        Self {
            realizer,
            authority,
            generation,
            namespace,
        }
    }
}
#[restate_sdk::service]
pub(crate) trait LashToolRealization {
    async fn realize(call: Call<RealizationRequest>) -> HandlerResult<Reply<RealizationReceipt>>;
}
impl LashToolRealization for LashToolRealizationImpl {
    async fn realize(
        &self,
        ctx: Context<'_>,
        call: Call<RealizationRequest>,
    ) -> HandlerResult<Reply<RealizationReceipt>> {
        let (wire, request) = call.open()?;
        let recorded = crate::sentinel::record_generation!(&ctx, &self.generation)?;
        crate::sentinel::check_generation(
            "LashToolRealization/realize",
            &recorded,
            &self.generation,
        )?;
        let controller = crate::RestateRuntimeEffectController::new(
            ctx,
            self.authority.clone(),
            self.generation.clone(),
        )
        .in_namespace(self.namespace.clone());
        let scoped = controller
            .realization_controller(request.scope.clone())
            .map_err(|error| {
                crate::process::handler_error_from_plugin(lash_core::PluginError::Runtime(error))
            })?;
        self.realizer
            .realize(request, scoped)
            .await
            .map(|receipt| Reply::at(wire, receipt))
            .map_err(|error| {
                let error = lash_core::PluginError::RuntimeEffectController(error);
                match error.class() {
                    lash_core::PluginErrorClass::Terminal => {
                        crate::process::handler_error_from_plugin(error)
                    }
                    lash_core::PluginErrorClass::Retryable
                    | lash_core::PluginErrorClass::Redrivable => {
                        crate::turn_handler::retried_attempt_failure(error.attempt_failure_text())
                    }
                }
            })
    }
}
