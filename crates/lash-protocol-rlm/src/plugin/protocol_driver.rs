use crate::dialect::SessionDialect;
use std::sync::Arc;

use super::RlmProtocolPluginConfig;
use crate::driver::{RlmPreambleConfig, build_rlm_preamble_with_dialect};
use lash_core::plugin::ProtocolDriverPlugin;
use lash_core::{ProtocolBuildInput, TurnDriverPreamble};

pub(super) struct RlmProtocolDriver {
    pub(super) config: RlmProtocolPluginConfig,
    pub(super) dialect: Arc<SessionDialect>,
}

impl ProtocolDriverPlugin for RlmProtocolDriver {
    fn resolve_render(
        &self,
        namespace: &lash_core::ProtocolTurnOptions,
    ) -> Result<Option<lash_core::RecordedRender>, lash_core::RenderFault> {
        let recorded = crate::plugin::RlmRecordedConfig::read(namespace).map_err(|error| {
            lash_core::RecordedNamespaceCorrupt {
                owner: crate::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                message: error.to_string(),
            }
        })?;
        let resolved = crate::render::ResolvedRlmRender::resolve(
            &self.config.render,
            &recorded
                .and_then(|recorded| recorded.render)
                .unwrap_or_default(),
        );
        Ok(Some(lash_core::RecordedRender {
            renderer_id: self.config.code_renderer.0.id().to_string(),
            params: serde_json::to_value(resolved).map_err(|error| {
                lash_core::RenderRefusal::new(&crate::plugin::RlmRenderRefusal::Unencodable {
                    message: error.to_string(),
                })
            })?,
        }))
    }

    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        build_rlm_preamble_with_dialect(
            input,
            RlmPreambleConfig {
                max_output_chars: self.config.max_output_chars,
            },
            Arc::clone(&self.dialect),
        )
    }
}
