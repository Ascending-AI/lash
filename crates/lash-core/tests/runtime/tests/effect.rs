// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core::facade_support::SessionGraphFacadeOps;
use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRole, LlmToolChoice};
use lash_core::plugin::PluginSessionRequest;
use lash_core::plugin::{ProtocolDriverPlugin, ProtocolSessionPlugin};
use lash_sansio::sync::MutexExt;
mod fig1127;

const SEED: u64 = 0x5_e100;

#[path = "effect_direct_llm.rs"]
mod direct_llm;

#[cfg(test)]
mod effect_driver_support;
use effect_driver_support::{
    EffectControllerTestCodeExecutor, EffectControllerTestProtocolFactory, PROMPT_REFUSAL,
    PromptRefusingProtocolFactory,
};
