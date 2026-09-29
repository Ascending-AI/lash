//! The Lashlang module-artifact port a store set supplies (ADR 0104, B2;
//! ADR 0113).
//!
//! The port stores a module's verified store bytes under its module
//! reference, kept alive by referrer edges, and never decodes them: lashlang
//! owns the artifact codec and wraps this port in its typed
//! `LashlangArtifacts`. The port sits here, below lashlang, so
//! [`StoreSet`](crate::StoreSet) can supply it like every other persistence
//! port, and the artifacts an RLM session writes live in the storage that
//! reopens the session.

use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ArtifactReferrer, ReferrerClaim, ResolvedArtifactCleanup};

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
    #[error("failed to encode artifact: {0}")]
    Encode(String),
    #[error("failed to decode artifact: {0}")]
    Decode(String),
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
    #[error("stored {record_kind} is corrupt: {message}")]
    StoredDataCorrupt {
        record_kind: &'static str,
        message: String,
    },
    #[error("artifact store backend error: {0}")]
    Backend(String),
}

impl From<crate::StoreError> for ArtifactStoreError {
    fn from(error: crate::StoreError) -> Self {
        match error {
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
                record_kind,
                message,
            },
            other => Self::Backend(other.to_string()),
        }
    }
}

impl From<ArtifactStoreError> for crate::PluginError {
    fn from(error: ArtifactStoreError) -> Self {
        match error {
            ArtifactStoreError::ReferrerEnded { referrer } => {
                crate::PluginError::Runtime(crate::RuntimeError::artifact_referrer_ended(referrer))
            }
            ArtifactStoreError::ArtifactMissing { .. } => {
                crate::PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::ArtifactMissing,
                    error.to_string(),
                ))
            }
            ArtifactStoreError::StoredDataCorrupt { .. } => {
                crate::PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::RuntimeStoreCorrupt,
                    error.to_string(),
                ))
            }
            // The store did not answer: a fact about this attempt.
            ArtifactStoreError::Backend(_) => crate::PluginError::Session(error.to_string()),
            // What the store holds refuses the write or the read: every retry
            // by this build meets it again.
            ArtifactStoreError::Encode(_)
            | ArtifactStoreError::Decode(_)
            | ArtifactStoreError::Immutable { .. }
            | ArtifactStoreError::CarryArtifactMissing { .. } => {
                crate::PluginError::Invoke(error.to_string())
            }
        }
    }
}

/// The Lashlang module-artifact store of one store set.
///
/// A module is published once under its module reference (an opaque key) as
/// its verified store bytes, and kept alive by referrer edges. Only the
/// cleanup executor severs edges, through [`Self::end_module_referrer`]; an
/// artifact with no edge left is reclaimed there, and an ended referrer is
/// fenced against every later write. The bytes are immutable: publishing
/// different bytes under a published reference is refused.
#[async_trait::async_trait]
pub trait ModuleArtifactStore: Send + Sync {
    /// Arm a one-shot conformance pause immediately before this store's
    /// publication serialization point. Production callers never use this
    /// diagnostic seam; stores that participate in referrer conformance
    /// return a handle and pause their next publish until it is resumed.
    fn pause_next_publication_for_testing(&self) -> Option<ArtifactPublicationPause> {
        None
    }

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

/// A one-shot pause at a store's publication serialization point, armed by
/// [`ModuleArtifactStore::pause_next_publication_for_testing`].
#[derive(Clone, Default)]
pub struct ArtifactPublicationPause {
    state: Arc<Mutex<ArtifactPublicationPauseState>>,
}

#[derive(Default)]
struct ArtifactPublicationPauseState {
    reached: bool,
    resumed: bool,
    writer_waker: Option<std::task::Waker>,
}

impl ArtifactPublicationPause {
    pub fn is_reached(&self) -> bool {
        self.state.lock_recover().reached
    }

    pub fn resume(&self) {
        let mut state = self.state.lock_recover();
        state.resumed = true;
        if let Some(waker) = state.writer_waker.take() {
            waker.wake();
        }
    }

    pub async fn pause(&self) {
        std::future::poll_fn(|context| {
            let mut state = self.state.lock_recover();
            state.reached = true;
            if state.resumed {
                std::task::Poll::Ready(())
            } else {
                state.writer_waker = Some(context.waker().clone());
                std::task::Poll::Pending
            }
        })
        .await
    }
}
