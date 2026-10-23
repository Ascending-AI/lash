//! Call-local authority retaining the selected route and admission definition.
use crate::{OperationRoute, ToolContract, ToolDefinition, ToolManifest};
use serde::{Deserialize, Serialize};

/// The authority to execute a selected operation. The original route
/// determines whether each attempt checks the live provider or uses a
/// committed deferred definition; the admission definition supplies the
/// call contract and metadata on any owner.
// `PartialEq` but not `Eq`: the payload is a `serde_json::Value`, whose float
// arm has no total equality.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExecutionGrant {
    /// The original selected route, including its source of authority.
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
    /// Retain the selected route unchanged beside its admission definition.
    pub(crate) fn from_route(link: OperationRoute, definition: ToolDefinition) -> Self {
        Self { link, definition }
    }

    /// A grant of `definition`, executed by the provider declared as
    /// `provider`.
    pub fn from_definition(
        provider: lash_vocabulary::providers::ToolProviderHandle,
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
    pub fn provider(&self) -> &lash_vocabulary::providers::ToolProviderHandle {
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
    pub fn payload(&self) -> Option<&lash_vocabulary::providers::ExecutionBinding> {
        self.link.payload.as_ref()
    }

    /// The grant with the match's codec payload.
    #[must_use]
    pub fn with_execution_binding(
        mut self,
        payload: Option<lash_vocabulary::providers::ExecutionBinding>,
    ) -> Self {
        self.link.payload = payload;
        self
    }

    /// The callbacks a call under this grant runs on: its provider, then
    /// its payload's codec when it carries one. What a cell holding the
    /// grant owes until it ends (S7).
    pub fn callbacks(
        &self,
    ) -> impl Iterator<Item = lash_vocabulary::providers::PluginCallbackIdentity> + '_ {
        self.link.callbacks()
    }
}
