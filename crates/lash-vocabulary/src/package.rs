//! Immutable namespaces, fills and scoped deferred grants.
//! A Link fills every document Import; selected execution reach is smaller.
//! Publication context is trusted host provenance, never guest authorization.
pub use crate::content_ids::{LinkId, PackageId};
use crate::{TurnControls, identity::SessionId, names::*, operations::*, providers::*, types::*};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Public by default. Internal fills require the exact package and publisher, not a shared root.
pub enum Visibility {
    #[default]
    Public,
    Internal,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Read answers are immediate and unsaved. Waited invocations are admitted durably.
pub enum OperationMode {
    Read,
    Waited,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// One declared operation intent, retained as accepted metadata.
pub struct Intent(pub String);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// The serving signature/contract and accepted intents, controls and execution mode.
pub struct OperationOffer {
    pub requirement: OperationRequirement,
    pub intents: BTreeSet<Intent>,
    pub controls: TurnControls,
    pub mode: OperationMode,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A published namespace: one path and its members. It need not own code.
pub struct Package {
    pub path: PackagePath,
    pub members: BTreeMap<Segment, Member>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// One package name's offered kind and visibility. Visibility does not expose a model tool.
pub struct Member {
    pub visibility: Visibility,
    pub kind: MemberKind,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// An operation offer or an Export of already linked code. Type/error aliases must preserve their Brand.
pub enum MemberKind {
    Operation(OperationOffer),
    Export { link: LinkId, export: ExportName },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Immutable linked code. Every document Import is filled, including Imports outside a selected start's reach.
pub struct Link {
    pub document: DocumentId,
    pub fills: BTreeMap<ImportSlot, Fill>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Chosen operation contract, linked Export, or scoped deferred grant. Deferred target outcomes live in the execution.
pub enum Fill {
    Operation(OperationRequirement),
    Export { link: LinkId, export: ExportName },
    Deferred { provider: Box<DeferredGrant> },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Logical owner/local provider identity, independent of the session/process hosting registry.
pub struct ProviderName {
    pub owner: String,
    pub key: String,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Host-supplied durable tenancy/storage-domain discriminator.
pub struct PublicationDomain(pub String);
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Authenticated host or session scope, supplied by Core rather than guest data.
pub enum PublicationScope {
    Host,
    Session(SessionId),
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// One configured publisher instance; two instances of one plugin remain distinct.
pub struct PublisherInstance(pub String);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Trusted publication provenance from the registered publisher and authenticated domain/scope.
pub struct PublicationContext {
    pub domain: PublicationDomain,
    pub scope: PublicationScope,
    pub publisher: PublisherInstance,
    pub root: Segment,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A pinned logical codec, readable format and immutable payload.
pub struct GrantPayload {
    pub codec: ProviderName,
    pub format: FormatVersion,
    pub value: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Immutable resolver config. A component is present only for recorded process-component config.
pub struct ProviderConfig {
    pub component: Option<ProcessComponentId>,
    pub format: FormatVersion,
    pub value: serde_json::Value,
    pub payload: Option<GrantPayload>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Authorized resolver scope/config/path and dynamic Json signature, pinned in the Link rather than a future match.
pub struct DeferredGrant {
    pub provider: ProviderName,
    pub context: PublicationContext,
    pub path: PackagePath,
    pub visibility: Visibility,
    pub signature: Signature,
    pub config: ProviderConfig,
    pub resolution_policy: DeferredResolutionPolicy,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A deferred path advertisement with no enumerated members. Only Operation Imports may use it.
pub struct DeferredPackage {
    pub path: PackagePath,
    pub visibility: Visibility,
    pub provider: ProviderName,
    pub config: ProviderConfig,
    pub resolution_policy: DeferredResolutionPolicy,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Enumerated or deferred namespace data. Enumerated roots are closed and never fall through.
pub enum PackageAdvertisement {
    Enumerated(Package),
    Deferred(Box<DeferredPackage>),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Explicit host authorization for members/packages. Deferred paths must be separately declared.
pub struct Attachment {
    pub members: BTreeSet<QualifiedName>,
    pub packages: BTreeSet<PackagePath>,
    pub deferred: BTreeSet<PackagePath>,
}
