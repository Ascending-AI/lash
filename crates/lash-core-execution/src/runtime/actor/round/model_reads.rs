//! Model read slots are saved before bodies, and their facts with tool results.

use serde::{Deserialize, Serialize};

/// A read slot in the model response's original order. Recorded siblings keep
/// their call identities; this slot never owns an admitted execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReadPlan {
    pub ordinal: u64,
    pub slot: u64,
    pub caller: lash_protocol_types::ToolReadCaller,
    pub provider: crate::ProviderKey,
    pub definition: lash_vocabulary::tool_contract::ToolDefinition,
    pub call_id: crate::ToolCallId,
    pub presentation: lash_core_store::tool_run::PresentationBinding,
    pub request: lash_core_store::tool_run::MaterialRef,
    #[serde(serialize_with = "lash_protocol_types::durable_serde::serialize")]
    pub refusal: Option<crate::ToolFailure>,
}

impl ModelReadPlan {
    pub fn metadata(&self) -> crate::ToolReadMetadata {
        crate::ToolReadMetadata {
            caller: self.caller.clone(),
            provider: self.provider.clone(),
            ordinal: self.ordinal,
            site: String::new(),
            origin: String::new(),
            elapsed_us: 0,
            args_bytes: 0,
            answer_bytes: 0,
        }
    }
}

/// The caller's saved read result, including its final presentation decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelReadResult {
    pub fact: lash_protocol_types::ReadFact,
    pub presentation: crate::runtime::effect::ToolPresentation,
}

impl ModelReadResult {
    pub fn completed(&self, call: &lash_sansio::sansio::PendingToolCall) -> super::CompletedCall {
        super::CompletedCall {
            read: None,
            call_id: call.call_id.clone(),
            provider_call_id: call.provider_call_id.clone(),
            tool_name: call.tool_name.clone(),
            args: call.args.clone(),
            model_return: self.presentation.model_return.clone(),
            output: self.fact.record().output,
            control_origin: lash_sansio::ToolControlOrigin::OwnCall,
            intent_outcomes: Vec::new(),
            replay: call.replay.clone(),
        }
    }
}
