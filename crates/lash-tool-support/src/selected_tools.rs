//! [`SelectedToolsPlugin`]: a host's tool provider whose offer its own config
//! namespace selects.
//!
//! Which tools a run is offered is each provider's own configuration (R2).
//! This plugin owns one namespace holding a [`ToolSelection`] under `tools`,
//! changed sticky by [`SetToolSelection`] and for one run by the run options
//! [`SelectedToolsPlugin::run_options`] addresses, and it offers its
//! provider's tools filtered by the selection the run's config holds.

use std::sync::Arc;

use async_trait::async_trait;
use lash_core::plugin::{
    CandidateFacts, ConfigCommand, ConfigOwner, ConfigRegistrar, ConfigRegistrationError,
    OwnerChange, PluginDeclaration, PluginError, PluginFactory, PluginMetadata, PluginRegistrar,
    PluginSessionContext, SessionPlugin,
};
use lash_core::{
    OwnerConfigView, ToolCall, ToolContract, ToolId, ToolManifest, ToolPrepareCall, ToolProvider,
};
use serde::{Deserialize, Serialize};

use crate::ToolSelection;

/// The namespace a [`SelectedToolsPlugin`] owns, and the options a run
/// states for it: the selection, every tool when absent.
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    lash_core::facade_support::schemars::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
pub struct ToolSelectionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolSelection>,
}

/// The sticky change of a [`SelectedToolsPlugin`]'s selection.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    lash_core::facade_support::schemars::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
pub struct SetToolSelection {
    pub tools: ToolSelection,
}

impl ConfigCommand for SetToolSelection {
    type Owner = ToolSelectionOwner;
    type Output = ();
    const NAME: &'static str = "set_tool_selection";
}

impl SetToolSelection {
    /// This change addressed to the [`SelectedToolsPlugin`] `owner`, for
    /// [`ConfigTransaction::then_entry`](lash_core::ConfigTransaction::then_entry).
    /// Every selected-tools plugin registers this command, and a typed
    /// transaction step names the first that does; a session with more
    /// than one names its target.
    #[must_use]
    pub fn to(self, owner: &str) -> lash_core::ConfigCommandEntry {
        lash_core::ConfigCommandEntry {
            owner: owner.to_owned(),
            command: Self::NAME.to_owned(),
            args: serde_json::to_value(self).unwrap_or_default(),
        }
    }
}

/// The owner of a [`SelectedToolsPlugin`]'s namespace. A run's selection
/// replaces the recorded one for that run alone.
pub struct ToolSelectionOwner;

impl ConfigOwner for ToolSelectionOwner {
    type Create = ToolSelectionConfig;
    type Recorded = ToolSelectionConfig;
    type Refusal = String;
    type RunOptions = ToolSelectionConfig;

    fn create(
        &self,
        input: Option<ToolSelectionConfig>,
    ) -> Result<Option<ToolSelectionConfig>, String> {
        Ok(Some(input.unwrap_or_default()))
    }

    fn validate(
        &self,
        _value: &ToolSelectionConfig,
        _base: Option<&ToolSelectionConfig>,
        _facts: &CandidateFacts<'_>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn apply_run_options(
        &self,
        recorded: &ToolSelectionConfig,
        options: ToolSelectionConfig,
    ) -> Result<ToolSelectionConfig, String> {
        Ok(ToolSelectionConfig {
            tools: options.tools.or_else(|| recorded.tools.clone()),
        })
    }
}

/// A plugin, under its own id, that offers `provider`'s tools as the
/// [`ToolSelection`] its namespace holds under each run's config admits.
/// Its provider is declared under the key `tools`.
pub struct SelectedToolsPlugin {
    id: &'static str,
    provider: Arc<dyn ToolProvider>,
}

impl SelectedToolsPlugin {
    #[must_use]
    pub fn new(id: &'static str, provider: Arc<dyn ToolProvider>) -> Self {
        Self { id, provider }
    }

    /// The key a send states this plugin's run options under.
    #[must_use]
    pub const fn run_options(&self) -> lash_core::RunOptionsKey<ToolSelectionConfig> {
        lash_core::RunOptionsKey::new(self.id)
    }
}

impl PluginMetadata for SelectedToolsPlugin {
    fn plugin_declaration(&self) -> PluginDeclaration {
        PluginDeclaration::initial(self.id)
    }
}

impl PluginFactory for SelectedToolsPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn register_config(&self, reg: &mut ConfigRegistrar) -> Result<(), ConfigRegistrationError> {
        reg.owner_with_run_options(ToolSelectionOwner, self.run_options())?;
        reg.command::<SetToolSelection>(|_recorded, command| {
            Ok(OwnerChange {
                recorded: ToolSelectionConfig {
                    tools: Some(command.tools),
                },
                output: (),
            })
        })
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(SelectedToolsSession {
            id: self.id,
            provider: Arc::new(Selected {
                owner: self.id,
                inner: Arc::clone(&self.provider),
            }),
        }))
    }
}

struct SelectedToolsSession {
    id: &'static str,
    provider: Arc<dyn ToolProvider>,
}

impl SessionPlugin for SelectedToolsSession {
    fn id(&self) -> &'static str {
        self.id
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.tools()
            .provider(lash_core::hook_key!("tools"), Arc::clone(&self.provider))
    }
}

/// `inner`, offering only what `owner`'s selection admits under the
/// config each method reads. A selection that does not decode offers
/// nothing.
struct Selected {
    owner: &'static str,
    inner: Arc<dyn ToolProvider>,
}

impl Selected {
    fn admits(&self, tool: &ToolId, config: &OwnerConfigView) -> bool {
        ToolSelection::of(config, self.owner).is_ok_and(|selection| selection.admits(tool))
    }
}

#[async_trait]
impl ToolProvider for Selected {
    fn tool_manifests(&self, config: &OwnerConfigView) -> Vec<ToolManifest> {
        self.inner
            .tool_manifests(config)
            .into_iter()
            .filter(|manifest| self.admits(&manifest.id, config))
            .collect()
    }

    fn resolve_manifest_by_id(
        &self,
        id: &ToolId,
        config: &OwnerConfigView,
    ) -> Option<ToolManifest> {
        self.inner
            .resolve_manifest_by_id(id, config)
            .filter(|manifest| self.admits(&manifest.id, config))
    }

    fn resolve_contract(&self, name: &str, config: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        let admitted = self
            .inner
            .tool_manifests(config)
            .into_iter()
            .any(|manifest| manifest.name == name && self.admits(&manifest.id, config));
        admitted
            .then(|| self.inner.resolve_contract(name, config))
            .flatten()
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<lash_core::PreparedToolCall, lash_core::ToolOutcome> {
        self.inner.prepare_tool_call(call).await
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.inner.execute(call).await
    }

    fn readable_binding_formats(&self) -> &'static [lash_core::FormatVersion] {
        self.inner.readable_binding_formats()
    }

    fn isolated_start(
        &self,
        call: lash_core::IsolatedProcessRequest<'_>,
    ) -> Result<lash_core::IsolatedStart, lash_core::ProcessEnvRefusal> {
        self.inner.isolated_start(call)
    }
}
