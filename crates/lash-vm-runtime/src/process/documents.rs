//! The admitted documents processes run, in the store set's module port.
//!
//! A document is stored once, as its canonical JSON, under its identity:
//! the hash of its content, kernel version and manifest included
//! (`K-DOC-003`). Annotations are not stored with it: they never change
//! behaviour and are keyed to the identity by whoever keeps them.

use std::sync::Arc;

use lash_core::{ArtifactStoreError, ModuleArtifactStore, ReferrerClaim};
use lash_kernel_doc::{Document, DocumentId};

/// Why a document could not be published or read.
#[derive(Debug, thiserror::Error)]
pub enum DocumentStoreError {
    #[error("the document has no canonical encoding: {0}")]
    Encode(#[from] lash_kernel_doc::EncodeError),
    #[error("stored document `{document}` does not decode: {message}")]
    Corrupt {
        document: DocumentId,
        message: String,
    },
    #[error("stored document `{document}` hashes to `{found}`")]
    Identity {
        document: DocumentId,
        found: DocumentId,
    },
    #[error(transparent)]
    Store(#[from] ArtifactStoreError),
}

/// The store of admitted documents.
#[derive(Clone)]
pub struct KernelDocuments {
    store: Arc<dyn ModuleArtifactStore>,
}

impl std::fmt::Debug for KernelDocuments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelDocuments").finish_non_exhaustive()
    }
}

impl KernelDocuments {
    /// The documents `store` holds.
    pub fn new(store: Arc<dyn ModuleArtifactStore>) -> Self {
        Self { store }
    }

    /// Stores `document` under its identity and adds `claim`'s edge to it.
    ///
    /// # Errors
    ///
    /// [`DocumentStoreError`]; a fenced referrer is the store's
    /// `ReferrerEnded`.
    pub async fn publish(
        &self,
        claim: &ReferrerClaim,
        document: &Document,
    ) -> Result<DocumentId, DocumentStoreError> {
        let identity = document.identity()?;
        self.store
            .publish_module_artifact(claim, &identity.to_string(), document.to_json()?.as_bytes())
            .await?;
        Ok(identity)
    }

    /// Adds `claim`'s edge to a stored document.
    ///
    /// # Errors
    ///
    /// The store's refusal; absent bytes are `ArtifactMissing`.
    pub async fn acquire(
        &self,
        claim: &ReferrerClaim,
        document: &DocumentId,
    ) -> Result<(), ArtifactStoreError> {
        self.store
            .acquire_module_artifact(claim, &document.to_string())
            .await
    }

    /// The document stored under `identity`, if any referrer holds it. The
    /// bytes are checked against the identity they are read under.
    ///
    /// # Errors
    ///
    /// [`DocumentStoreError`].
    pub async fn get(&self, identity: &DocumentId) -> Result<Option<Document>, DocumentStoreError> {
        let Some(bytes) = self
            .store
            .get_module_artifact(&identity.to_string())
            .await?
        else {
            return Ok(None);
        };
        let corrupt = |message: String| DocumentStoreError::Corrupt {
            document: *identity,
            message,
        };
        let text = std::str::from_utf8(&bytes).map_err(|error| corrupt(error.to_string()))?;
        let document = Document::from_json(text).map_err(|error| corrupt(error.to_string()))?;
        let found = document.identity()?;
        if found != *identity {
            return Err(DocumentStoreError::Identity {
                document: *identity,
                found,
            });
        }
        Ok(Some(document))
    }
}
