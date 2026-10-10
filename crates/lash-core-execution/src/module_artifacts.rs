//! The module-artifact port a store set supplies (ADR 0113): the store of
//! admitted kernel documents.
//!
//! The port stores a document's canonical bytes under its identity, kept
//! alive by referrer edges, and never decodes them: the kernel process engine
//! owns the encoding and wraps this port in its typed `KernelDocuments`. The
//! port sits here, below the engine, so [`StoreSet`](crate::StoreSet) can
//! supply it like every other persistence port, and the documents an RLM
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
    #[error("referrer kind `{kind}` cannot hold artifact bytes")]
    ReferrerKindRefused { kind: crate::ArtifactReferrerKind },
    #[error("unusable payload schema: {source}")]
    UnusableSchema {
        source: Box<crate::SchemaAdmissionError>,
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
    #[error("artifact store backend error: {0}")]
    Backend(String),
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
                None => Self::Backend(other.to_string()),
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
            // The store did not answer: a fact about this attempt.
            ArtifactStoreError::Backend(message) => crate::PluginError::StoreUnavailable {
                fault: crate::store::StoreFault::Backend { message },
            },
            // What the store holds refuses the write or the read: every retry
            // by this build meets it again.
            ArtifactStoreError::Encode(_)
            | ArtifactStoreError::Immutable { .. }
            | ArtifactStoreError::CarryArtifactMissing { .. } => {
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

/// The module-artifact store of one store set: the admitted kernel documents.
///
/// A module is published once under its module reference (an opaque key) as
/// its verified store bytes, and kept alive by referrer edges. Only the
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

    /// Apply one resolved cleanup in one transaction (ADR 0113 §2.3): fence
    /// the referrer, apply the carries, sever its edges and reclaim what no
    /// edge holds any more. Replaying an applied cleanup is a no-op.
    async fn end_module_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError>;

    /// The store bytes published under `module_ref`, if any referrer holds
    /// them.
    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError>;
}
