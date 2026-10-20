//! Immutable document admission and preparation, scoped to one prepared
//! library and layout. Only live document allocations can reuse admission:
//! the weak reference prevents pointer reuse and copy-on-write mutation from
//! ever selecting code for a different document. Code holds neither its
//! document nor its library, so the cache has no ownership cycle.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock, Weak};

use lash_kernel_doc::{
    Document, DocumentId, FunctionId, Invalid, KernelDataLimits, validate_document_with_limits,
};

use super::*;
use crate::{OpenError, StartError};

#[derive(Default)]
pub(super) struct Documents(
    BTreeMap<(usize, usize), Arc<Admission>>,
    /// The identities validated against this library, with the document
    /// depth they were validated under: a later admission of one skips
    /// validation (FIG-6931).
    BTreeSet<(DocumentId, usize)>,
);

/// The most validated identities a library remembers; past it, it starts
/// again.
const VALIDATED: usize = 4096;

pub(crate) struct Admission {
    document: Weak<Document>,
    code: OnceLock<Result<Arc<DocumentCode>, Missing>>,
    identity: OnceLock<DocumentId>,
}

pub(crate) enum Refused {
    Missing(FunctionId),
    Invalid(Invalid),
}

impl From<Refused> for StartError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Missing(function) => Self::MissingFunction { function },
            Refused::Invalid(invalid) => Self::NotAdmitted(invalid),
        }
    }
}

impl From<Refused> for OpenError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Missing(function) => Self::MissingFunction { function },
            Refused::Invalid(invalid) => Self::NotAdmitted(invalid),
        }
    }
}

impl PreparedLibrary {
    /// Admission precedes compilation, preserving import's refusal order:
    /// document identity and pins are checked before missing body callees.
    pub(crate) fn admit(
        &self,
        document: &Arc<Document>,
        limits: KernelDataLimits,
    ) -> Result<Arc<Admission>, Refused> {
        self.admit_as(document, None, limits)
    }

    /// Records that the document of `identity` was validated against this
    /// library's functions under `limits`, so a later admission of it is not
    /// validated again (FIG-6931). Only a caller that validated that very
    /// document records it, as a dialect that validates while it lowers.
    pub fn note_validated(&self, identity: DocumentId, limits: KernelDataLimits) {
        let mut documents = self
            .0
            .documents
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if documents.1.len() >= VALIDATED {
            documents.1.clear();
        }
        documents.1.insert((identity, limits.document_depth()));
    }

    /// Admits `document`, whose identity is `identity`, ahead of its start
    /// under `limits`, and answers whether it is admitted: an identity
    /// validated against this library before is not validated again
    /// (FIG-6931). A refused document's start refuses it with its typed
    /// cause.
    pub fn admit_identified(
        &self,
        document: &Arc<Document>,
        identity: DocumentId,
        limits: KernelDataLimits,
    ) -> bool {
        self.admit_as(document, Some(identity), limits).is_ok()
    }

    fn admit_as(
        &self,
        document: &Arc<Document>,
        identity: Option<DocumentId>,
        limits: KernelDataLimits,
    ) -> Result<Arc<Admission>, Refused> {
        let mut documents = self
            .0
            .documents
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Only document depth affects structural validation. Runtime value
        // and inline limits are applied by each run's heap, not admission.
        let key = (Arc::as_ptr(document) as usize, limits.document_depth());
        if let Some(admitted) = documents.0.get(&key) {
            return Ok(Arc::clone(admitted));
        }
        // Reclaim preparation for documents no caller or machine retains.
        // Weak references keep their allocation addresses reserved until this
        // sweep, so even a dropped document can never produce a false hit.
        documents
            .0
            .retain(|_, admitted| admitted.document.strong_count() != 0);
        for function in document.manifest.functions.keys() {
            if self.registry().get(function).is_none() {
                return Err(Refused::Missing(*function));
            }
        }
        let validated = identity
            .is_some_and(|identity| documents.1.contains(&(identity, limits.document_depth())));
        if !validated {
            validate_document_with_limits(document, self.registry().as_ref(), limits)
                .map_err(Refused::Invalid)?;
            if let Some(identity) = identity {
                if documents.1.len() >= VALIDATED {
                    documents.1.clear();
                }
                documents.1.insert((identity, limits.document_depth()));
            }
        }
        let admitted = Arc::new(Admission {
            document: Arc::downgrade(document),
            code: OnceLock::new(),
            identity: identity.map_or_else(OnceLock::new, OnceLock::from),
        });
        documents.0.insert(key, Arc::clone(&admitted));
        Ok(admitted)
    }
}

impl Admission {
    pub(crate) fn identity(&self, document: &Document) -> DocumentId {
        *self.identity.get_or_init(|| document.identity())
    }

    pub(crate) fn executable(
        self: &Arc<Self>,
        document: &Arc<Document>,
        library: &PreparedLibrary,
    ) -> Result<Arc<Executable>, Missing> {
        let own = self
            .code
            .get_or_init(|| compile_document(document, library).map(Arc::new));
        let own = own.as_ref().map_err(|missing| *missing)?;
        Ok(Arc::new(Executable {
            code_base: library.0.tables.codes.len(),
            block_base: library.0.tables.blocks.len(),
            stmt_base: library.0.tables.stmts.len(),
            library: library.clone(),
            document: Arc::clone(document),
            own: Arc::clone(own),
            admission: Arc::clone(self),
        }))
    }
}

impl Executable {
    pub(crate) fn identity(&self) -> DocumentId {
        self.admission.identity(&self.document)
    }
}
