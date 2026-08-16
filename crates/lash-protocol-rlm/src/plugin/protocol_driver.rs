use std::sync::Arc;

use super::RlmProtocolPluginConfig;
use crate::dialect::RlmDialect;
use crate::driver::{RlmPreambleConfig, SharedPromptUsage, build_rlm_preamble_with_dialect};
use crate::rlm_support::SharedBoundVariablesPrompt;
use lash_core::plugin::ProtocolDriverPlugin;
use lash_core::{ProtocolBuildInput, TurnDriverPreamble};

pub(super) struct RlmProtocolDriver {
    pub(super) config: RlmProtocolPluginConfig,
    pub(super) dialect: Arc<dyn RlmDialect>,
    pub(super) last_prompt_usage: SharedPromptUsage,
    pub(super) bound_variables_prompt: SharedBoundVariablesPrompt,
}

impl ProtocolDriverPlugin for RlmProtocolDriver {
    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        build_rlm_preamble_with_dialect(
            input,
            RlmPreambleConfig {
                max_output_chars: self.config.max_output_chars,
                max_budget_tokens: self.config.continue_as_soft_warn_tokens,
                last_prompt_usage: Arc::clone(&self.last_prompt_usage),
                prompt_features: self.config.prompt_features,
                redaction_roots: self
                    .config
                    .redaction_roots
                    .clone()
                    .unwrap_or_default()
                    .into(),
            },
            Arc::clone(&self.bound_variables_prompt),
            Arc::clone(&self.dialect),
        )
    }
}
