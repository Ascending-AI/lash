//! Authority carried by a recorded deferred match.
use crate::{OperationRoute, ToolContract, ToolDefinition, ToolManifest};
use serde::{Deserialize, Serialize};

/// The authority to execute a tool a recorded deferred match resolved,
/// outside the run's recorded surface: the match's [`OperationRoute`], whose
/// provider prepares and executes its calls under the definition it
/// retains, and whose payload the provider reads from its prepare and
/// execute contexts.
// `PartialEq` but not `Eq`: the payload is a `serde_json::Value`, whose float
// arm has no total equality.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "OperationRoute", into = "OperationRoute")]
pub struct ToolExecutionGrant {
    /// A [`crate::RouteSource::Resolved`] link: the one identity its calls
    /// bind to and dispatch through.
    link: OperationRoute,
    definition: ToolDefinition,
}

impl TryFrom<OperationRoute> for ToolExecutionGrant {
    type Error = String;

    fn try_from(link: OperationRoute) -> Result<Self, Self::Error> {
        let definition = link
            .resolution()
            .cloned()
            .ok_or_else(|| format!("a grant of tool `{}` retains no definition", link.tool))?;
        Ok(Self { link, definition })
    }
}

impl From<ToolExecutionGrant> for OperationRoute {
    fn from(grant: ToolExecutionGrant) -> Self {
        grant.link
    }
}

impl ToolExecutionGrant {
    /// A grant of `definition`, executed by the provider declared as
    /// `provider`.
    pub fn from_definition(
        provider: crate::plugin::ToolProviderHandle,
        definition: ToolDefinition,
    ) -> Self {
        Self {
            link: OperationRoute::resolved(provider, definition.clone(), None),
            definition,
        }
    }

    /// The grant a committed deferred match's `link` makes; `None` for a
    /// link that retains no definition.
    pub fn of_link(link: &OperationRoute) -> Option<Self> {
        Self::try_from(link.clone()).ok()
    }

    /// The grant's link.
    pub fn link(&self) -> &OperationRoute {
        &self.link
    }

    /// The declared provider that prepares and executes this grant's calls.
    pub fn provider(&self) -> &crate::plugin::ToolProviderHandle {
        &self.link.provider
    }

    /// The definition the grant's calls are admitted under.
    pub fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    /// Returns the tool identity and model-facing metadata authorized by this grant.
    pub fn manifest(&self) -> &ToolManifest {
        &self.definition.manifest
    }

    /// Returns the contract used to validate granted call arguments.
    pub fn contract(&self) -> &ToolContract {
        &self.definition.contract
    }

    /// The match's codec payload, passed to prepare and execute contexts.
    pub fn payload(&self) -> Option<&crate::ExecutionBinding> {
        self.link.payload.as_ref()
    }

    /// The grant with the match's codec payload.
    #[must_use]
    pub fn with_execution_binding(mut self, payload: Option<crate::ExecutionBinding>) -> Self {
        self.link.payload = payload;
        self
    }

    /// The callbacks a call under this grant runs on: its provider, then
    /// its payload's codec when it carries one. What a cell holding the
    /// grant owes until it ends (S7).
    pub fn callbacks(&self) -> impl Iterator<Item = crate::plugin::PluginCallbackIdentity> + '_ {
        self.link.callbacks()
    }
}
