//! Immutable package and publication records.

use lash_vocabulary::package::{Link, LinkId, Package, PackageId};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkRecord {
    pub id: LinkId,
    pub link: Link,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageRecord {
    pub id: PackageId,
    pub package: Package,
}
/// Typed immutable-content and admission refusals shared by both backends.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PackageRefusal {
    #[error("package session owner is absent or deleted: {0}")]
    SessionNotLive(lash_vocabulary::identity::SessionId),
    #[error("content identity does not match its record: {0}")]
    Identity(String),
    #[error("immutable record already has different content: {0}")]
    Immutable(String),
    #[error("required content is absent: {0}")]
    Missing(String),
    #[error("carried artifact {artifact_ref} is missing for {to}")]
    CarryMissing {
        artifact_ref: String,
        to: crate::artifact_referrer::ArtifactReferrer,
    },
    #[error("publication does not match its path, owner or code members")]
    Publication,
    #[error("publication/content dependency graph is cyclic")]
    Cycle,
    #[error("execution is not registered to this actor/use/requested name")]
    ExecutionUse,
    #[error("resolution target contradicts its recorded contract, route or mode")]
    ResolutionTarget,
    #[error("artifact referrer has ended: {0}")]
    ReferrerEnded(crate::artifact_referrer::ArtifactReferrer),
}
