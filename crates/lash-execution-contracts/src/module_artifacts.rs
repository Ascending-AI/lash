//! The module-artifact port a store set supplies (ADR 0113): the store of
//! admitted kernel documents.
//!
//! The port stores a document's canonical token body under its identity,
//! kept alive by referrer edges, as the checkpoint graph node it encodes
//! ([`ModuleArtifactBody`]), and never decodes it: the kernel process engine
//! owns the encoding and wraps this port in its typed `KernelDocuments`. The
//! port sits here, below the engine, so [`StoreSet`](crate::StoreSet) can
//! supply it like every other persistence port, and the documents a code mode
//! session writes live in the storage that reopens the session.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ArtifactReferrer, ModuleArtifactCorruption, ModuleArtifactRefusal, ReferrerClaim,
    ResolvedArtifactCleanup,
};

/// Durability tier established by the execution path's concrete store or host.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityTier {
    #[default]
    Inline,
    Durable,
}

/// Why an artifact-store operation failed (ADR 0113 §2.7).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ArtifactStoreError {
    #[error(transparent)]
    PackageRefused(#[from] lash_durable::packages::PackageRefusal),
    #[error("referrer kind `{kind}` cannot hold artifact bytes")]
    ReferrerKindRefused { kind: crate::ArtifactReferrerKind },
    #[error("unusable payload schema: {source}")]
    UnusableSchema {
        source: Box<lash_vocabulary::json_schema::SchemaAdmissionError>,
    },
    #[error("failed to encode artifact: {0}")]
    Encode(String),
    /// Publish or acquire named a referrer that has a fence.
    #[error("artifact referrer `{referrer}` has ended")]
    ReferrerEnded { referrer: ArtifactReferrer },
    /// Acquire named bytes that are not stored.
    #[error("artifact `{artifact_ref}` is not stored")]
    ArtifactMissing { artifact_ref: String },
    /// A carry's bytes were gone when its cleanup ran.
    #[error("artifact `{artifact_ref}` carried to `{to}` is not stored")]
    CarryArtifactMissing {
        artifact_ref: String,
        to: ArtifactReferrer,
    },
    /// Different bytes under a stored reference.
    #[error("artifact `{artifact_ref}` is already stored with different bytes")]
    Immutable { artifact_ref: String },
    /// Stored artifact data or a stored referrer pair failed validation.
    #[error("stored artifact data is corrupt: {source}")]
    StoredDataCorrupt { source: ModuleArtifactCorruption },
    /// A stored vocabulary label written by a newer compatible release.
    #[error("{refusal}")]
    Incompatible {
        refusal: crate::compat::CompatRefusal,
    },
    #[error(transparent)]
    StoreRefusal(crate::store::StoreRefusal),
    /// Artifact verification could not obtain a worker on this host.
    #[error("worker checkout exceeded its bounded wait")]
    WorkerCheckoutTimedOut,
    /// A typed substrate fault, before the artifact operation answered.
    #[error(transparent)]
    StoreFault(crate::store::StoreFault),
    #[error("artifact store backend error: {0}")]
    Backend(String),
}

impl ArtifactStoreError {
    /// The local diagnostic code of a typed artifact refusal. Operational
    /// faults have no refusal code; the original error retains its detail.
    pub fn refusal_code(&self) -> Option<&'static str> {
        Some(match self {
            ArtifactStoreError::PackageRefused(_) => "package_refused",
            ArtifactStoreError::ReferrerKindRefused { .. } => "referrer_kind_refused",
            ArtifactStoreError::UnusableSchema { .. } => "unusable_schema",
            ArtifactStoreError::ReferrerEnded { .. } => "pin_released",
            ArtifactStoreError::ArtifactMissing { .. } => "artifact_missing",
            ArtifactStoreError::CarryArtifactMissing { .. } => "carry_artifact_missing",
            ArtifactStoreError::StoredDataCorrupt { .. } => "artifact_corrupt",
            ArtifactStoreError::Incompatible { .. } => "incompatible",
            ArtifactStoreError::StoreRefusal(_) => "store_refused",
            ArtifactStoreError::Immutable { .. } => "conflict",
            ArtifactStoreError::Encode(_)
            | ArtifactStoreError::WorkerCheckoutTimedOut
            | ArtifactStoreError::StoreFault(_)
            | ArtifactStoreError::Backend(_) => return None,
        })
    }
}

impl From<crate::StoreError> for ArtifactStoreError {
    fn from(error: crate::StoreError) -> Self {
        match error {
            crate::StoreError::ReferrerKindRefused {
                kind,
                store: crate::ReferrerStore::Artifact,
            } => Self::ReferrerKindRefused { kind },
            crate::StoreError::ArtifactReferrerEnded { referrer } => {
                Self::ReferrerEnded { referrer }
            }
            crate::StoreError::ArtifactMissing { artifact_ref } => {
                Self::ArtifactMissing { artifact_ref }
            }
            crate::StoreError::ArtifactCarryMissing { artifact_ref, to } => {
                Self::CarryArtifactMissing { artifact_ref, to }
            }
            crate::StoreError::StoredDataCorrupt {
                record_kind,
                message,
            } => Self::StoredDataCorrupt {
                source: ModuleArtifactCorruption::Storage {
                    record_kind: record_kind.into(),
                    message,
                },
            },
            crate::StoreError::Incompatible { refusal } => Self::Incompatible { refusal },
            other => match crate::store::StoreRefusal::of_store_error(&other) {
                Some(refusal) => Self::StoreRefusal(refusal),
                None => match crate::store::StoreFault::of_store_error(&other) {
                    Some(fault) => Self::StoreFault(fault),
                    None => Self::Backend(other.to_string()),
                },
            },
        }
    }
}

impl From<ModuleArtifactRefusal> for ArtifactStoreError {
    fn from(refusal: ModuleArtifactRefusal) -> Self {
        match refusal {
            ModuleArtifactRefusal::Corrupt(source) => Self::StoredDataCorrupt { source },
        }
    }
}

impl From<ArtifactStoreError> for crate::PluginError {
    fn from(error: ArtifactStoreError) -> Self {
        match error {
            ArtifactStoreError::PackageRefused(refusal) => {
                crate::PluginError::PackageStoreRefused(refusal)
            }
            ArtifactStoreError::ReferrerKindRefused { kind } => {
                crate::PluginError::StoreRefusal(crate::store::StoreRefusal::ReferrerKindRefused {
                    kind,
                    store: crate::ReferrerStore::Artifact,
                })
            }
            ArtifactStoreError::UnusableSchema { source } => {
                crate::PluginError::UnusableSchema { source }
            }
            ArtifactStoreError::WorkerCheckoutTimedOut => {
                crate::PluginError::RuntimeEffectController(
                    crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::WorkerCheckoutTimedOut,
                        error.to_string(),
                    )
                    .retryable_uncommitted_derivation(),
                )
            }
            ArtifactStoreError::ReferrerEnded { referrer } => {
                crate::PluginError::Runtime(crate::RuntimeError::artifact_referrer_ended(referrer))
            }
            ArtifactStoreError::ArtifactMissing { .. } => {
                crate::PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::ArtifactMissing,
                    error.to_string(),
                ))
            }
            ArtifactStoreError::StoredDataCorrupt { source, .. } => {
                module_artifact_refused(ModuleArtifactRefusal::Corrupt(source))
            }
            ArtifactStoreError::StoreRefusal(refusal) => crate::PluginError::StoreRefusal(refusal),
            ArtifactStoreError::Incompatible { refusal } => {
                crate::StoreError::Incompatible { refusal }.into()
            }
            ArtifactStoreError::StoreFault(fault) => crate::PluginError::StoreUnavailable { fault },
            ArtifactStoreError::Immutable { .. } => {
                crate::PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::ArtifactConflict,
                    error.to_string(),
                ))
            }
            // The store did not answer: a fact about this attempt.
            ArtifactStoreError::Backend(message) => crate::PluginError::StoreUnavailable {
                fault: crate::store::StoreFault::Backend { message },
            },
            // What the store holds refuses the write or the read: every retry
            // by this build meets it again.
            ArtifactStoreError::Encode(_) | ArtifactStoreError::CarryArtifactMissing { .. } => {
                crate::PluginError::Invoke(error.to_string())
            }
        }
    }
}

fn module_artifact_refused(refusal: ModuleArtifactRefusal) -> crate::PluginError {
    crate::PluginError::Runtime(
        crate::RuntimeError::new(
            crate::RuntimeErrorCode::RuntimeStoreCorrupt,
            refusal.to_string(),
        )
        .with_cause(crate::RuntimeErrorCause::ModuleArtifactRefused {
            refusal: Box::new(refusal),
        }),
    )
}

/// What one held module artifact is stored as.
///
/// A kernel document and a library function's definition are checkpoint
/// graph nodes: a durable store keeps each once, as its node, which the
/// artifact's row roots (the graph's artifact bridge, FIG-6469), so a
/// checkpoint graph that names the same node shares that one copy. A read
/// of the artifact answers the node's logical body. Anything else is bytes
/// the module namespace's codec owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleArtifactBody<'a> {
    /// Opaque bytes, stored as they are.
    Bytes(&'a [u8]),
    /// A kernel document's canonical token body: its Document node (kind
    /// 10).
    Document(&'a [u8]),
    /// A library function's definition as its token body: its
    /// HelperDefinition node (kind 14).
    HelperDefinition(&'a [u8]),
}

impl<'a> ModuleArtifactBody<'a> {
    /// What a read of the artifact answers: the bytes, or the node's
    /// logical body.
    pub fn bytes(&self) -> &'a [u8] {
        match self {
            Self::Bytes(bytes) | Self::Document(bytes) | Self::HelperDefinition(bytes) => bytes,
        }
    }

    /// The graph node kind the body is stored as, if it is a node.
    #[must_use]
    pub fn node_kind(&self) -> Option<lash_core_store::checkpoint_graph::NodeKind> {
        use lash_core_store::checkpoint_graph::NodeKind;
        match self {
            Self::Bytes(_) => None,
            Self::Document(_) => Some(NodeKind::DOCUMENT),
            Self::HelperDefinition(_) => Some(NodeKind::HELPER_DEFINITION),
        }
    }
}

/// One artifact a [`ModuleArtifactStore::hold_module_artifacts`] batch holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleArtifactHold<'a> {
    /// Acquire only existing bytes; a raced reclamation is Missing, never resurrection.
    Retain { module_ref: &'a str },
    /// Stored when absent and verified against what is stored, as
    /// [`ModuleArtifactStore::publish_module_artifact`] holds it.
    Publish {
        module_ref: &'a str,
        body: ModuleArtifactBody<'a>,
    },
    /// Acquired whatever is stored, as
    /// [`ModuleArtifactStore::acquire_module_artifact`] holds it, and stored
    /// as `body` when nothing is.
    Acquire {
        module_ref: &'a str,
        body: ModuleArtifactBody<'a>,
    },
}

impl<'a> ModuleArtifactHold<'a> {
    pub fn module_ref(&self) -> &'a str {
        match self {
            Self::Publish { module_ref, .. }
            | Self::Acquire { module_ref, .. }
            | Self::Retain { module_ref } => module_ref,
        }
    }

    pub fn body(&self) -> Option<ModuleArtifactBody<'a>> {
        match self {
            Self::Publish { body, .. } | Self::Acquire { body, .. } => Some(*body),
            Self::Retain { .. } => None,
        }
    }
}

/// The module-artifact store of one store set: the admitted kernel documents.
///
/// A module is published once under its module reference (an opaque key) as
/// its verified [`ModuleArtifactBody`], and kept alive by referrer edges. A
/// node body's row roots its graph node, so the node lives while an edge
/// holds the row or another root reaches it. Only the
/// cleanup executor severs edges, through [`Self::end_module_referrer`]; an
/// artifact with no edge left is reclaimed there, and an ended referrer is
/// fenced against every later write. The bytes are immutable: publishing
/// different bytes under a published reference is refused.
#[async_trait::async_trait]
pub trait ModuleArtifactStore: Send + Sync {
    /// Durability tier this artifact store provides; defaults to [`DurabilityTier::Inline`].
    fn durability_tier(&self) -> DurabilityTier {
        DurabilityTier::Inline
    }

    /// Store `bytes` under `module_ref` if absent, verify they equal any
    /// stored bytes, and add the claim's edge, in one transaction that first
    /// takes the referrer's lock and checks its fence (`ReferrerEnded`). The
    /// same transaction inserts the claim's guard row if it has one and no
    /// row exists (ADR 0113 §2.4).
    async fn publish_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError>;

    /// Add the claim's edge to bytes already stored, with the same lock,
    /// fence check and guard arming. Absent bytes are `ArtifactMissing`.
    async fn acquire_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError>;

    /// Hold every one of `artifacts` under the claim in one transaction: the
    /// referrer's lock, fence check and guard arming once, then each artifact
    /// as its [`ModuleArtifactHold`] says, then the claim's edge to each. A
    /// refusal holds none of them.
    async fn hold_module_artifacts(
        &self,
        claim: &ReferrerClaim,
        artifacts: &[ModuleArtifactHold<'_>],
    ) -> Result<(), ArtifactStoreError>;

    /// Apply one resolved cleanup in one transaction (ADR 0113 §2.3): fence
    /// the referrer, apply the carries, sever its edges and reclaim what no
    /// edge holds any more. Replaying an applied cleanup is a no-op.
    async fn end_module_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError>;

    /// The bytes published under `module_ref`, or the logical body of the
    /// node it roots, if any referrer holds it.
    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError>;
}

impl From<ArtifactStoreError> for lash_durable::DurableError {
    fn from(error: ArtifactStoreError) -> Self {
        use lash_durable::packages::PackageRefusal;
        use lash_durable::{DurableError, StoreFailure, StoreFailureKind};
        match error {
            ArtifactStoreError::PackageRefused(refusal) => DurableError::Package(refusal),
            ArtifactStoreError::ReferrerEnded { referrer } => {
                PackageRefusal::ReferrerEnded(referrer).into()
            }
            ArtifactStoreError::ArtifactMissing { artifact_ref } => {
                PackageRefusal::Missing(artifact_ref).into()
            }
            ArtifactStoreError::Immutable { artifact_ref } => {
                PackageRefusal::Immutable(artifact_ref).into()
            }
            ArtifactStoreError::CarryArtifactMissing { artifact_ref, to } => {
                PackageRefusal::CarryMissing { artifact_ref, to }.into()
            }
            ArtifactStoreError::ReferrerKindRefused { kind } => {
                DurableError::StoreRefused(crate::store::StoreRefusal::ReferrerKindRefused {
                    kind,
                    store: crate::ReferrerStore::Artifact,
                })
            }
            ArtifactStoreError::Incompatible { refusal } => {
                DurableError::StoreRefused(crate::store::StoreRefusal::Incompatible { refusal })
            }
            ArtifactStoreError::StoreRefusal(refusal) => DurableError::StoreRefused(refusal),
            ArtifactStoreError::StoreFault(fault) => DurableError::Store(StoreFailure {
                kind: if matches!(fault, crate::store::StoreFault::Contended) {
                    StoreFailureKind::Contended
                } else {
                    StoreFailureKind::Unavailable
                },
                message: fault.to_string(),
            }),
            error => DurableError::Store(StoreFailure {
                kind: match &error {
                    ArtifactStoreError::Backend(_) | ArtifactStoreError::WorkerCheckoutTimedOut => {
                        StoreFailureKind::Unavailable
                    }
                    _ => StoreFailureKind::Corrupt,
                },
                message: error.to_string(),
            }),
        }
    }
}

impl From<lash_durable::DurableError> for ArtifactStoreError {
    fn from(error: lash_durable::DurableError) -> Self {
        use lash_durable::packages::PackageRefusal;
        use lash_durable::{DurableError, StoreFailureKind};
        match error {
            DurableError::Package(PackageRefusal::ReferrerEnded(referrer)) => {
                Self::ReferrerEnded { referrer }
            }
            DurableError::Package(PackageRefusal::Missing(artifact_ref)) => {
                Self::ArtifactMissing { artifact_ref }
            }
            DurableError::Package(PackageRefusal::Immutable(artifact_ref)) => {
                Self::Immutable { artifact_ref }
            }
            DurableError::Package(PackageRefusal::CarryMissing { artifact_ref, to }) => {
                Self::CarryArtifactMissing { artifact_ref, to }
            }
            DurableError::Package(refusal) => Self::PackageRefused(refusal),
            DurableError::StoreRefused(refusal) => Self::StoreRefusal(refusal),
            DurableError::Store(failure) if failure.kind == StoreFailureKind::Contended => {
                Self::StoreFault(crate::store::StoreFault::Contended)
            }
            DurableError::Store(failure) if failure.kind == StoreFailureKind::Unavailable => {
                Self::Backend(failure.message)
            }
            error => Self::StoredDataCorrupt {
                source: ModuleArtifactCorruption::Storage {
                    record_kind: "package artifact".into(),
                    message: error.to_string(),
                },
            },
        }
    }
}
