//! Owner-requested helper replacement metadata. Executable references stay exact hashes.
use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_value::{DocumentId, FunctionId, LinkedStart, WorkflowReference};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Permanent package-qualified contract name, used only when pairing releases.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct HelperId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelperChange {
    pub helper: HelperId,
    pub from: FunctionId,
    pub to: FunctionId,
    pub body: bool,
    pub charge: bool,
    pub native_version: bool,
}

/// A validated ledger pairing, including anonymous bodies that cannot be paired by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelperUpgrade {
    pub functions: BTreeMap<FunctionId, FunctionId>,
    pub changes: Vec<HelperChange>,
    pub changed: BTreeSet<FunctionId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, thiserror::Error)]
#[serde(tag = "refused", rename_all = "snake_case", deny_unknown_fields)]
pub enum HelperUpgradeRefusal {
    #[error("helper {helper:?} changed contract from {from} to {to}")]
    HelperContractChanged {
        helper: HelperId,
        from: FunctionId,
        to: FunctionId,
    },
    #[error("helper {helper:?} was removed")]
    HelperTombstoned { helper: HelperId },
    #[error("helper {helper:?} has no current implementation")]
    HelperMissing { helper: HelperId },
    #[error("helper {function} has no unambiguous ledger pairing")]
    HelperUnpaired { function: FunctionId },
    #[error("helper archive is invalid: {message}")]
    Archive { message: String },
    #[error("helper upgrade cannot rewrite the document: {message}")]
    Document { message: String },
    #[error("retained callable still reaches changed helper {function}")]
    RetainedCallable { function: FunctionId },
    #[error("retained branded data was constructed by changed helper {function}")]
    BrandedData { function: FunctionId },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelperPublicationUpgradeRequest {
    pub publication: LinkedStart,
}

/// A new immutable publication revision. The host chooses whether its alias moves.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelperPublicationUpgrade {
    pub from: WorkflowReference,
    pub to: WorkflowReference,
    pub document: DocumentId,
    pub links: BTreeMap<lash_kernel_value::LinkId, lash_kernel_value::LinkId>,
    pub upgrade: HelperUpgrade,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelperRunUpgradeRequest {
    pub request_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum HelperUpgradeSafety {
    Safe,
    Deferred { functions: BTreeSet<FunctionId> },
}

/// Schema version of the helper run upgrade response, independent of kernel code.
/// version_surface = "coexist"
/// format_outside_manifest = "host response JSON, not durable execution state"
/// version_guard(roots(HelperRunUpgrade), items(from_json))
pub const HELPER_RUN_UPGRADE_VERSION: u32 = 2;

/// The artifact prepared by this request, coupled to the succession result.
/// A publication is not evidence that the owner has applied the successor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum HelperRunUpgradeOutcome {
    /// Succession is pending, possibly until an active continuation drains.
    Requested {
        publication: HelperPublicationUpgrade,
    },
    /// The prepared revision is already selected.
    AlreadyCurrent {
        publication: HelperPublicationUpgrade,
    },
    /// The request id has settled. No new publication is returned.
    AlreadyResolved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelperRunUpgrade {
    pub request_id: String,
    pub outcome: HelperRunUpgradeOutcome,
}

impl HelperRunUpgrade {
    /// Decode the current response schema, refusing the old state/publication pair.
    pub fn from_json(text: &str) -> Result<Self, crate::DecodeError> {
        crate::document::from_json_with_limits(
            text,
            lash_kernel_value::data_limits::KernelDataLimits::standard(),
        )
    }
}
