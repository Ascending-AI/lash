//! The Lashlang module-artifact port a store set supplies (ADR 0104, B2).
//!
//! The port stores a module's verified store bytes under its module
//! reference, retained per owner, and never decodes them: lashlang owns the
//! artifact codec and wraps this port in its typed `LashlangArtifacts`. The
//! port sits here, below lashlang, so [`StoreSet`](crate::StoreSet) can supply
//! it like every other persistence port, and the artifacts an RLM session
//! writes live in the storage that reopens the session.

use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ArtifactOwner;

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

/// Why an artifact-store operation failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ArtifactStoreError {
    #[error("failed to encode lashlang artifact: {0}")]
    Encode(String),
    #[error("failed to decode lashlang artifact: {0}")]
    Decode(String),
    /// The write named an owner a permanent retirement fence has already
    /// closed. Typed so callers classify by variant, not by message text.
    #[error("artifact owner has been permanently retired")]
    OwnerRetired,
    /// A transfer named a destination owner a permanent retirement fence has
    /// already closed.
    #[error("artifact destination owner has been permanently retired")]
    DestinationOwnerRetired,
    /// A transfer found neither the staging owner's edge nor the destination
    /// owner's edge. `artifact` is the producer's noun phrase for the
    /// artifact, e.g. `module artifact \`mod-…\``.
    #[error("{artifact} is not retained by the staging owner")]
    StagingEdgeMissing { artifact: String },
    #[error("artifact store backend error: {0}")]
    Backend(String),
}

impl From<crate::StoreError> for ArtifactStoreError {
    fn from(error: crate::StoreError) -> Self {
        match error {
            crate::StoreError::ArtifactOwnerRetired => Self::OwnerRetired,
            crate::StoreError::ArtifactDestinationOwnerRetired => Self::DestinationOwnerRetired,
            crate::StoreError::ArtifactStagingEdgeMissing { artifact } => {
                Self::StagingEdgeMissing { artifact }
            }
            other => Self::Backend(other.to_string()),
        }
    }
}

impl From<ArtifactStoreError> for crate::PluginError {
    fn from(error: ArtifactStoreError) -> Self {
        match error {
            ArtifactStoreError::OwnerRetired => {
                crate::runtime::process::artifact_owner_retired_error()
            }
            ArtifactStoreError::DestinationOwnerRetired => {
                crate::runtime::process::artifact_destination_owner_retired_error()
            }
            ArtifactStoreError::StagingEdgeMissing { artifact } => {
                crate::runtime::process::artifact_staging_edge_missing_error(artifact)
            }
            other => crate::PluginError::Session(other.to_string()),
        }
    }
}

/// The Lashlang module-artifact store of one store set.
///
/// A module is published once under its module reference (an opaque key)
/// as its verified store bytes, and retained by owner edges; the last edge
/// released reclaims it, and a retired execution owner is fenced against late
/// publication. The bytes are immutable: publishing different bytes under a
/// published reference is refused.
#[async_trait::async_trait]
pub trait ModuleArtifactStore: Send + Sync {
    /// Arm a one-shot conformance pause immediately before this store's
    /// publication serialization point. Production callers never use this
    /// diagnostic seam; stores that participate in ownership conformance
    /// return a handle and pause their next publish until it is resumed.
    fn pause_next_publication_for_testing(&self) -> Option<ArtifactPublicationPause> {
        None
    }

    /// Durability tier this artifact store provides; defaults to [`DurabilityTier::Inline`].
    fn durability_tier(&self) -> DurabilityTier {
        DurabilityTier::Inline
    }

    /// Publish a module's store bytes under `module_ref` and retain them for
    /// one exact owner.
    async fn publish_module_artifact(
        &self,
        owner: &ArtifactOwner,
        module_ref: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError>;

    /// Add an owner edge to an already published module.
    async fn retain_module_artifact(
        &self,
        owner: &ArtifactOwner,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError>;

    /// Atomically add `to` and sever `from` for one module artifact.
    async fn transfer_module_artifact(
        &self,
        from: &ArtifactOwner,
        to: &ArtifactOwner,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError>;

    /// Sever one exact owner edge and reclaim the module when it was the last.
    async fn release_module_artifact(
        &self,
        owner: &ArtifactOwner,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError>;

    /// Permanently fence an execution owner against late publication and sever
    /// every module edge it still owns.
    async fn retire_module_artifact_owner(
        &self,
        owner: &ArtifactOwner,
    ) -> Result<(), ArtifactStoreError>;

    /// The store bytes published under `module_ref`, if it is retained.
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
