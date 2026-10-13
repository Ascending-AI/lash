//! Authority carried by a recorded deferred match.
use crate::{ToolContract, ToolDefinition, ToolManifest};
use serde::{Deserialize, Serialize};

/// The authority to execute a tool a recorded deferred match resolved,
/// outside the run's recorded surface: the provider the match names, the
/// definition its calls are admitted under and the match's execution
/// binding, which the provider reads from its prepare and execute contexts.
// `PartialEq` but not `Eq`: the binding's payload is a `serde_json::Value`,
// whose float arm has no total equality.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExecutionGrant {
    /// The declared provider that prepares and executes this grant's calls:
    /// the one identity its calls bind to and dispatch through.
    pub provider: crate::plugin::ToolProviderHandle,
    /// Tool identity and model-facing metadata authorized by the grant.
    pub(crate) manifest: ToolManifest,
    /// Contract used to validate granted call arguments without consulting the
    /// current Tool Catalog.
    pub(crate) contract: Box<ToolContract>,
    /// The match's execution binding, passed to prepare and execute contexts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<crate::ExecutionBinding>,
}

impl ToolExecutionGrant {
    /// A grant of `definition`, executed by the provider declared as
    /// `provider`.
    pub fn from_definition(
        provider: crate::plugin::ToolProviderHandle,
        definition: ToolDefinition,
    ) -> Self {
        Self {
            provider,
            manifest: definition.manifest(),
            contract: Box::new(definition.contract()),
            binding: None,
        }
    }

    /// Returns the tool identity and model-facing metadata authorized by this grant.
    pub fn manifest(&self) -> &ToolManifest {
        &self.manifest
    }

    /// Returns the contract used to validate granted call arguments.
    pub fn contract(&self) -> &ToolContract {
        &self.contract
    }

    /// The grant with the match's execution binding.
    pub fn with_execution_binding(mut self, binding: Option<crate::ExecutionBinding>) -> Self {
        self.binding = binding;
        self
    }

    /// The callbacks a call under this grant runs on: its provider, then
    /// its binding's codec when it carries one. What a cell holding the
    /// grant owes until it ends (S7).
    pub fn callbacks(&self) -> impl Iterator<Item = crate::plugin::PluginCallbackIdentity> + '_ {
        std::iter::once(self.provider.callback_identity()).chain(
            self.binding
                .iter()
                .map(|binding| binding.codec.callback_identity()),
        )
    }
}
