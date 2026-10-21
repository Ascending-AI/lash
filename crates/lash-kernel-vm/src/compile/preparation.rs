//! Immutable document admission and preparation, scoped to one prepared
//! library and layout. Only live document allocations can reuse admission:
//! the weak reference prevents pointer reuse and copy-on-write mutation from
//! ever selecting code for a different document. Code holds neither its
//! document nor its library, so the cache has no ownership cycle.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock, Weak};

use lash_kernel_doc::{Document, Invalid, validate_document_with_limits};
use lash_vocabulary::data_limits::KernelDataLimits;
use lash_vocabulary::legacy_names::DocumentId;

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
    identity: OnceLock<DocumentId>,
}

pub(crate) enum Refused {
    Invalid(Invalid),
}

impl From<Refused> for StartError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Invalid(invalid) => Self::NotAdmitted(invalid),
        }
    }
}

impl From<Refused> for OpenError {
    fn from(refused: Refused) -> Self {
        match refused {
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
        documents: &BTreeMap<DocumentId, Arc<Document>>,
        instances: &BTreeMap<
            lash_vocabulary::content_ids::LinkId,
            crate::interface::PreparedInstance,
        >,
    ) -> Result<Arc<Executable>, Missing> {
        // Link tables are instance-specific; compile the reached documents together,
        // deduplicating identical FunctionIds while keeping frame Links separate.
        let mut reached = document.as_ref().clone();
        for loaded in documents.values() {
            reached.functions.extend(loaded.functions.clone());
        }
        let loaded;
        let library = if reached
            .functions
            .keys()
            .any(|id| library.registry().get(id).is_none())
        {
            let mut registry = library.registry().as_ref().clone();
            let mut pending: BTreeSet<_> = reached
                .functions
                .keys()
                .filter(|id| registry.get(id).is_none())
                .copied()
                .collect();
            while let Some(blocked) = pending.first().copied() {
                let ready = pending.iter().copied().find(|id| {
                    reached.functions[id].body().is_none_or(|body| {
                        body.functions.keys().all(|dependency| {
                            dependency == id || registry.get(dependency).is_some()
                        })
                    })
                });
                let id = ready.ok_or(Missing(blocked))?;
                registry
                    .register(reached.functions[&id].clone(), None)
                    .map_err(|_| Missing(id))?;
                pending.remove(&id);
            }
            loaded = PreparedLibrary::with_layout_and_charge(
                Arc::new(registry),
                library.0.layout,
                library.0.charge,
            );
            &loaded
        } else {
            library
        };
        let own = Arc::new(compile_document(&reached, library)?);
        Ok(Arc::new(Executable {
            code_base: library.0.tables.codes.len(),
            block_base: library.0.tables.blocks.len(),
            stmt_base: library.0.tables.stmts.len(),
            instances: instances
                .iter()
                .filter_map(|(link, instance)| {
                    documents
                        .get(&instance.document)
                        .map(|doc| (*link, Arc::clone(doc)))
                })
                .collect(),
            library: library.clone(),
            document: Arc::clone(document),
            own,
            admission: Arc::clone(self),
        }))
    }
}

impl Executable {
    pub(crate) fn identity(&self) -> DocumentId {
        self.admission.identity(&self.document)
    }
}
