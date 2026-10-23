//! Change-driven session environments (ENV-IDENTITY, ENV-DELTA, ENV-REINCARNATE).
//!
//! This is the value contract beside [`crate::tool_surface`], not an admission
//! driver. Sources produce one coherent snapshot for both projections. The
//! session actor receives changes through its mailbox and adopts a head before
//! each model call. Replay reads that round's reference; a running cell keeps
//! the reference it captured at start. No source is polled by this module.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_vocabulary::claims::{ClaimRequirements, RegistryKind};
use lash_vocabulary::package::{AcceptedAdvertisement, DeferredGrant, PublicationContext};
use serde::{Deserialize, Serialize};

use crate::tool_surface::{AdvertisedRoute, SurfaceEntry};

mod assembly;
mod namespace_index;
pub use assembly::{AssemblyRefusal, assemble};
pub use namespace_index::NamespaceIndex;
#[cfg(test)]
mod tests;

/// Stable registered source identity, shared by its tool and package projections.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct SourceId(pub String);

/// A producer's incarnation changes on replacement/reconnect; counters are only
/// ordered within one incarnation. Neither field contributes to content identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceRevision {
    #[serde(with = "uuid_text")]
    #[schemars(with = "String")]
    pub incarnation: uuid::Uuid,
    pub counter: u64,
}

/// How a source can change. RefreshOnly sources remain static until the host's
/// explicit refresh; Notifying sources also send committed revision changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceMode {
    Immutable,
    Notifying,
    RefreshOnly,
}

/// Registration declares how the actor can learn of changes. It is control
/// metadata, separate from the fragment's behavior-affecting content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceDescriptor {
    pub source: SourceId,
    pub mode: SourceMode,
}

/// One atomic source observation. Never enumerate tools and packages separately.
/// A detached source supplies an empty fragment; all its old facts are replaced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceSnapshot {
    pub source: SourceId,
    pub revision: SourceRevision,
    pub fragment: EnvironmentFragment,
}

/// Immutable facts from one producer. Vectors are unordered membership lists;
/// assembly orders them canonically, preserving ordered data inside each value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentFragment {
    pub tools: Vec<SurfaceEntry>,
    pub model_removals: BTreeSet<lash_vocabulary::names::QualifiedOperation>,
    pub deferred_provider: Option<crate::tool_surface::ProviderKey>,
    pub package_roots: Vec<AcceptedAdvertisement>,
    pub routes: Vec<AdvertisedRoute>,
    pub deferred_grants: Vec<DeferredGrant>,
    /// Only the config/selection slices that affect this source's behavior.
    pub config: BTreeMap<String, serde_json::Value>,
    pub consumer: Option<PublicationContext>,
    /// Namespace ownership, including roots currently advertising no packages.
    pub namespace_claims: Vec<PublicationContext>,
    /// Semantic engine fingerprints keyed by the registered engine kind.
    pub engines: BTreeMap<String, String>,
    /// Exact registry, codecs, callbacks, native functions and kernel identities.
    pub identities: BTreeMap<RegistryKind, ClaimRequirements>,
    /// Pure derived fragment roots depend on these sources' roots. The source
    /// lane supplies replacement snapshots if their actual facts also change.
    pub dependencies: BTreeSet<SourceId>,
}

/// Canonical content identity. It never names a retention lifetime.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct EnvironmentId(String);

/// Hash of one canonical fragment and its dependency roots.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct FragmentId(String);

/// A prepared immutable contribution. Unchanged contributions are shared across
/// bodies, so delta assembly does not normalize, encode or hash them again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreparedFragment {
    fragment: EnvironmentFragment,
    root: FragmentId,
}
impl PreparedFragment {
    pub fn fragment(&self) -> &EnvironmentFragment {
        &self.fragment
    }
    pub fn root(&self) -> &FragmentId {
        &self.root
    }
}

/// A content-addressed environment. Revisions, timestamps, dirty flags and actor
/// epochs live outside this value. Assembly is the sole constructor; stores
/// should validate a decoded body with [`EnvironmentBody::validate`] before use.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentBody {
    fragments: BTreeMap<SourceId, Arc<PreparedFragment>>,
    id: EnvironmentId,
}
impl EnvironmentBody {
    pub fn id(&self) -> &EnvironmentId {
        &self.id
    }
    pub fn fragments(&self) -> &BTreeMap<SourceId, Arc<PreparedFragment>> {
        &self.fragments
    }
    /// Project tools and packages together from their recorded source owner.
    pub fn recorded_surface(
        &self,
    ) -> Result<crate::tool_surface::RecordedSurface, Box<crate::tool_surface::SurfaceRefusal>>
    {
        let mut removed = BTreeSet::<lash_vocabulary::names::QualifiedOperation>::new();
        let mut deferred = None;
        let mut tools = Vec::new();
        let mut packages = crate::tool_surface::RecordedPackages::default();
        for fragment in self.fragments.values().map(|f| f.fragment()) {
            removed.extend(fragment.model_removals.iter().cloned());
            if fragment.deferred_provider.is_some() {
                deferred = fragment.deferred_provider.clone();
            }
            tools.extend(fragment.tools.iter().cloned());
            packages
                .snapshot
                .advertisements
                .extend(fragment.package_roots.iter().cloned());
            packages.routes.extend(fragment.routes.iter().cloned());
            if fragment.consumer.is_some() {
                packages.consumer = fragment.consumer.clone();
            }
        }
        Ok(crate::tool_surface::RecordedSurface::new(
            tools
                .into_iter()
                .filter(|entry| !removed.contains(&entry.definition.manifest.operation)),
            deferred,
        )
        .map_err(Box::new)?
        .with_packages(packages))
    }

    /// Check both global invariants and all stored roots against canonical facts.
    pub fn validate(&self) -> Result<(), AssemblyRefusal> {
        let snapshots = self
            .fragments
            .iter()
            .map(|(source, prepared)| SourceSnapshot {
                source: source.clone(),
                revision: SourceRevision {
                    incarnation: uuid::Uuid::nil(),
                    counter: 0,
                },
                fragment: prepared.fragment.clone(),
            });
        if assemble(None, snapshots)? != *self {
            return Err(AssemblyRefusal::ContentIdentity);
        }
        Ok(())
    }
}

/// One version lifetime, freshly minted even when retired content is used again.
/// Store registrars persist permanent retired fences and never reuse this id.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct VersionOwnerId(
    #[serde(with = "uuid_text")]
    #[schemars(with = "String")]
    uuid::Uuid,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VersionOwnerState {
    Preparing,
    Published,
    Retired,
}

/// Create Preparing before any package hold. Publish with head/run references;
/// retire with cleanup enqueue in the same transaction as final graph retirement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VersionOwner {
    id: VersionOwnerId,
    version: EnvironmentId,
    state: VersionOwnerState,
}
impl VersionOwner {
    pub fn preparing(version: EnvironmentId) -> Self {
        Self {
            id: VersionOwnerId(uuid::Uuid::new_v4()),
            version,
            state: VersionOwnerState::Preparing,
        }
    }
    pub fn id(&self) -> &VersionOwnerId {
        &self.id
    }
    pub fn version(&self) -> &EnvironmentId {
        &self.version
    }
    pub fn state(&self) -> VersionOwnerState {
        self.state
    }
    /// A retired owner can never return to Preparing or Published. Preparing
    /// may retire directly to clean up interrupted preparation.
    pub fn transition(&mut self, to: VersionOwnerState) -> Result<(), OwnerTransitionRefusal> {
        use VersionOwnerState::{Preparing, Published, Retired};
        if !matches!(
            (self.state, to),
            (Preparing, Published | Retired) | (Published, Retired)
        ) {
            return Err(OwnerTransitionRefusal {
                owner: self.id.clone(),
                from: self.state,
                to,
            });
        }
        self.state = to;
        Ok(())
    }
}
#[derive(
    Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
#[error("environment owner {owner:?} cannot transition from {from:?} to {to:?}")]
pub struct OwnerTransitionRefusal {
    pub owner: VersionOwnerId,
    pub from: VersionOwnerState,
    pub to: VersionOwnerState,
}

pub type InputFrontier = BTreeMap<SourceId, SourceRevision>;

/// Durable session head, published under the actor's fence at a model boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentHead {
    pub version: EnvironmentId,
    pub owner: VersionOwnerId,
    pub input_frontier: InputFrontier,
}
/// Recorded once per model round, even when the previous round used this version.
/// Cells copy this reference at start and keep it through parks and replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoundEnvironment {
    pub version: EnvironmentId,
    pub owner: VersionOwnerId,
}
impl EnvironmentHead {
    pub fn round(&self) -> RoundEnvironment {
        RoundEnvironment {
            version: self.version.clone(),
            owner: self.owner.clone(),
        }
    }
}

/// Mailbox facts, never permission for a host to drive a turn. The actor
/// reconciles incarnation changes with one coherent snapshot before adoption.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentChange {
    ConfigSelection {
        sources: BTreeSet<SourceId>,
    },
    SourceRevision {
        source: SourceId,
        revision: SourceRevision,
    },
    /// Explicit host refresh; None means every mutable source, including those
    /// without notifications. Immutable sources are never re-asked.
    Refresh {
        sources: Option<BTreeSet<SourceId>>,
    },
}

// Keep the existing UUID feature graph; UUID text is validated on decode.
mod uuid_text {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(id: &uuid::Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(id)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<uuid::Uuid, D::Error> {
        uuid::Uuid::parse_str(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for EnvironmentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::fmt::Display for FragmentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::fmt::Display for VersionOwnerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
