// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core::facade_support::SessionGraphFacadeOps;
use lash_core::llm::types::{
    AttachmentSource, LlmContentBlock, LlmMessage, LlmRole, LlmToolChoice,
};
use lash_core::plugin::PluginSessionRequest;
use lash_core::plugin::{ProtocolDriverPlugin, ProtocolSessionPlugin};
use lash_core::testing::TestTurnExecution as _;
use lash_sansio::sync::MutexExt;
mod fig1127;

mod response_settlement;

const SEED: u64 = 0x5_e100;

#[path = "effect_direct_llm.rs"]
mod direct_llm;

#[cfg(test)]
mod effect_driver_support;
use effect_driver_support::{
    EffectControllerTestCodeExecutor, EffectControllerTestProtocolFactory, PROMPT_REFUSAL,
    PromptRefusingProtocolFactory,
};

struct PreludeContextHooks {
    live_revision: Arc<std::sync::atomic::AtomicUsize>,
    pressure_calls: Arc<std::sync::atomic::AtomicUsize>,
    prepare_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl lash_core::plugin::ContextPressureHook for PreludeContextHooks {
    fn id(&self) -> &'static str {
        "prelude-pressure"
    }

    async fn decide(
        &self,
        _ctx: &lash_core::plugin::ContextPressureContext<'_>,
    ) -> Result<lash_core::plugin::ContextPressureDecision, lash_core::plugin::ContextError> {
        self.pressure_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(
            if self.live_revision.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                lash_core::plugin::ContextPressureDecision::Record { nodes: Vec::new() }
            } else {
                lash_core::plugin::ContextPressureDecision::Continue
            },
        )
    }
}

#[async_trait::async_trait]
impl lash_core::plugin::TurnContextTransform for PreludeContextHooks {
    fn id(&self) -> &'static str {
        "prelude-prepare"
    }

    async fn transform(
        &self,
        _ctx: &lash_core::plugin::TurnTransformContext<'_>,
        mut input: lash_core::facade_support::PreparedContext,
    ) -> Result<lash_core::facade_support::PreparedContext, lash_core::plugin::ContextError> {
        self.prepare_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.live_revision.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            input.messages = lash_core::facade_support::MessageSequence::default();
        }
        Ok(input)
    }
}
