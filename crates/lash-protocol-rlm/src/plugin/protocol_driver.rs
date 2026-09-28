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
    fn resolve_render(
        &self,
        options: &lash_core::ProtocolTurnOptions,
    ) -> Result<Option<lash_core::RecordedRender>, String> {
        let options = crate::rlm_support::decode_rlm_options(options)?;
        let resolved = crate::render::ResolvedRlmRender::resolve(
            &self.config.render,
            &options.render.unwrap_or_default(),
        );
        Ok(Some(lash_core::RecordedRender {
            renderer_id: self.config.code_renderer.0.id().to_string(),
            params: serde_json::to_value(resolved).map_err(|error| error.to_string())?,
        }))
    }

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
