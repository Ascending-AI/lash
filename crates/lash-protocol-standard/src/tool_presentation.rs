//! How a standard session's tools read outside their provider specs: the
//! docs its tool presenter writes, and the presenter's refusals as the
//! plugin's.

use lash_tool_presenter_api::{
    PresentationRefusal, PresentedToolSection, ToolDisclosure, ToolPresentationEntry,
    ToolPresentationRequest,
};

use crate::{STANDARD_PROTOCOL_PLUGIN_ID, StandardProtocolConfig};

impl StandardProtocolConfig {
    /// `catalog`'s tools, each as this configuration's tool presenter
    /// documents it under its admitted tool presentation, called by its
    /// name: the docs a host shows where it documents a standard session's
    /// tools outside their provider specs, such as a discovery result.
    ///
    /// # Errors
    ///
    /// The tool presenter refuses the config or a tool.
    pub fn tool_docs(
        &self,
        catalog: &lash_core::ToolCatalog,
    ) -> Result<PresentedToolSection, PresentationRefusal> {
        let config = self.tool_presenter.admit(&self.tool_presentation)?.config;
        let entries = catalog
            .tools
            .iter()
            .map(|tool| ToolPresentationEntry {
                manifest: &tool.manifest,
                contract: &tool.contract,
                call_path: &tool.manifest.name,
                pinned: false,
            })
            .collect::<Vec<_>>();
        self.tool_presenter
            .presenter()
            .present(ToolPresentationRequest {
                tools: &entries,
                config: &config,
                disclosure: ToolDisclosure::Complete,
            })
    }
}

/// `config`'s tool presenter's `refusal`, as the plugin's.
pub(crate) fn tool_presentation_refused(
    config: &StandardProtocolConfig,
    refusal: &PresentationRefusal,
) -> lash_core::plugin::PluginError {
    lash_core::plugin::PluginError::ToolPresentationRefused {
        plugin_id: STANDARD_PROTOCOL_PLUGIN_ID.to_string(),
        presenter: config.tool_presenter.presenter().id().to_owned(),
        message: refusal.to_string(),
    }
}
