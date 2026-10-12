//! Immutable document admission and preparation, scoped to one prepared
//! library and layout. Only live document allocations can reuse admission:
//! the weak reference prevents pointer reuse and copy-on-write mutation from
//! ever selecting code for a different document. Code holds neither its
//! document nor its library, so the cache has no ownership cycle.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, Weak};

use lash_kernel_doc::{
    Document, DocumentId, EncodeError, FunctionId, Invalid, KernelDataLimits,
    validate_document_with_limits,
};

use super::{DocumentCode, Executable, Missing, PreparedLibrary, compile_document};
use crate::{ImportError, StartError};

#[derive(Default)]
pub(super) struct Documents(BTreeMap<(usize, usize), Arc<Admission>>);

pub(crate) struct Admission {
    document: Weak<Document>,
    code: OnceLock<Result<Arc<DocumentCode>, Missing>>,
    identity: OnceLock<Result<DocumentId, EncodeError>>,
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

impl From<Refused> for ImportError {
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
        validate_document_with_limits(document, self.registry().as_ref(), limits)
            .map_err(Refused::Invalid)?;
        let admitted = Arc::new(Admission {
            document: Arc::downgrade(document),
            code: OnceLock::new(),
            identity: OnceLock::new(),
        });
        documents.0.insert(key, Arc::clone(&admitted));
        Ok(admitted)
    }
}

impl Admission {
    pub(crate) fn identity(&self, document: &Document) -> Result<DocumentId, EncodeError> {
        self.identity.get_or_init(|| document.identity()).clone()
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
            library: library.clone(),
            document: Arc::clone(document),
            own: Arc::clone(own),
            admission: Arc::clone(self),
        }))
    }
}

impl Executable {
    pub(crate) fn identity(&self) -> Result<DocumentId, EncodeError> {
        self.admission.identity(&self.document)
    }
}
