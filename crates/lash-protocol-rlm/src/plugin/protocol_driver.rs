use crate::dialect::TypescriptDialect;
use std::sync::Arc;

use super::RlmProtocolPluginConfig;
use crate::driver::{RlmPreambleConfig, build_rlm_preamble_with_dialect};
use lash_core::plugin::ProtocolDriverPlugin;
use lash_core::{ProtocolBuildInput, TurnDriverPreamble};

pub(super) struct RlmProtocolDriver {
    pub(super) config: RlmProtocolPluginConfig,
    pub(super) dialect: Arc<TypescriptDialect>,
}

impl ProtocolDriverPlugin for RlmProtocolDriver {
    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        build_rlm_preamble_with_dialect(
            input,
            RlmPreambleConfig {
                discovery: self.config.discovery.clone(),
                max_output_chars: self.config.max_output_chars,
                max_budget_tokens: self.config.continue_as_soft_warn_tokens,
                prompt_features: self.config.prompt_features,
            },
            Arc::clone(&self.dialect),
        )
    }
}
