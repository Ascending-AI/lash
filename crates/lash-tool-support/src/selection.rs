//! [`ToolSelection`]: which of its tools a configurable provider offers.
//!
//! Which tools exist is each provider's own configuration (R2). A provider
//! that lets its host choose among its tools carries a selection in its own
//! config namespace, under the field `tools`, and filters what it advertises
//! by it under the run's config. Lash core never reads the field.

use lash_core::OwnerConfigView;
use lash_vocabulary::tool_contract::ToolId;
use serde::{Deserialize, Serialize};

/// The tools a provider offers, by tool id: only the named ones, or all
/// but the named ones.
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
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolSelection {
    Only(Vec<ToolId>),
    AllExcept(Vec<ToolId>),
}

impl Default for ToolSelection {
    /// Every tool the provider has.
    fn default() -> Self {
        Self::AllExcept(Vec::new())
    }
}

/// The part of a provider's config namespace the selection lives in.
#[derive(Deserialize)]
struct SelectionField {
    #[serde(default)]
    tools: Option<ToolSelection>,
}

impl ToolSelection {
    /// Every tool the provider has.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// Whether this selection offers `tool`.
    #[must_use]
    pub fn admits(&self, tool: &ToolId) -> bool {
        match self {
            Self::Only(tools) => tools.contains(tool),
            Self::AllExcept(tools) => !tools.contains(tool),
        }
    }

    /// The selection `owner`'s config namespace records under `tools` in
    /// `config`: every tool when the namespace or the field is absent.
    ///
    /// # Errors
    ///
    /// The namespace's `tools` field is not a selection.
    pub fn of(config: &OwnerConfigView, owner: &str) -> Result<Self, serde_json::Error> {
        Ok(config
            .decode::<SelectionField>(owner)?
            .and_then(|field| field.tools)
            .unwrap_or_default())
    }
}
