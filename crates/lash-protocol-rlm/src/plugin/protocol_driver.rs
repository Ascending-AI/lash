use std::sync::Arc;

use lash_core::plugin::ProtocolDriverPlugin;
use lash_core::{ProtocolBuildInput, TurnDriverPreamble};
use lash_lashlang_runtime::LashlangSurface;

use super::RlmProtocolPluginConfig;
use crate::driver::{
    RlmProjectorConfig, SharedPromptUsage, build_rlm_preamble_with_bound_variables,
};
use crate::rlm_support::SharedBoundVariablesPrompt;

pub(super) struct RlmProtocolDriver {
    pub(super) config: RlmProtocolPluginConfig,
    pub(super) lashlang_surface: LashlangSurface,
    pub(super) last_prompt_usage: SharedPromptUsage,
    pub(super) bound_variables_prompt: SharedBoundVariablesPrompt,
}

impl ProtocolDriverPlugin for RlmProtocolDriver {
    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        build_rlm_preamble_with_bound_variables(
            input,
            RlmProjectorConfig {
                max_output_chars: self.config.max_output_chars,
                max_budget_tokens: self.config.continue_as_soft_warn_tokens,
                last_prompt_usage: Arc::clone(&self.last_prompt_usage),
                prompt_features: self.config.prompt_features,
                lashlang_surface: self.lashlang_surface.clone(),
                redaction_roots: self
                    .config
                    .redaction_roots
                    .clone()
                    .unwrap_or_default()
                    .into(),
            },
            Arc::clone(&self.bound_variables_prompt),
        )
    }
}
