//! Read/edit seam: typed checking and graph inspection; hosts retain runnable
//! documents as `lash_kernel_doc::DocumentBytes`.
//!
//! A document as a host reads it: the document, the graph the checker
//! derived from it, where it is entered and the library declarations it was
//! linked against, held together so none of them can be paired with
//! another's.
//!
//! The holder is neutral. It names no engine, process, store or dialect: a
//! runtime adapts it for admission and storage, a trace for its overlay, a
//! view crate for drawing and a dialect for printing.

use std::sync::Arc;

use lash_kernel_doc::{
    Document, DocumentId, FunctionCatalog, FunctionDeclarations, Name, Signature, Unit,
    WorkflowDocumentEntry, WorkflowDocumentRef,
};

use crate::{
    EntryEffectsRefusal, Environment, Graph, Refusal, RefusalReason, Refused, derive, derive_entry,
    entry_effects, signature_mismatch,
};

/// One checked document entered where its reference says, with everything
/// derived from it and the declarations of every library function it uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowDocument {
    reference: WorkflowDocumentRef,
    document: Arc<Document>,
    graph: Arc<Graph>,
    declarations: Arc<FunctionDeclarations>,
}

impl WorkflowDocument {
    /// `document` read at `main`, held with `declarations`: exactly the
    /// library functions it lists, as a runtime ships them
    /// ([`lash_kernel_doc::FunctionRegistry::export_declarations`], narrowed
    /// with [`lash_kernel_doc::required_declarations`]) or as a store
    /// retained them. The document is checked structurally and every
    /// declaration against the identity it is listed under before anything
    /// is derived; linking then needs no registry, engine or native code.
    ///
    /// # Errors
    ///
    /// A declaration that is not the one the document lists, a document
    /// that is not well formed, or the checker's refusal of one it cannot
    /// link.
    pub fn new(
        document: Document,
        declarations: FunctionDeclarations,
    ) -> Result<Self, WorkflowDocumentError> {
        Self::entered(document, WorkflowDocumentEntry::Main, declarations)
    }

    /// [`Self::new`] entered at `entry`. An entry links only the units it
    /// executes; the total graph still includes main and templates it merely
    /// passes to another receiver.
    ///
    /// # Errors
    ///
    /// As [`Self::new`], and an entry the document does not list.
    pub fn entered(
        document: Document,
        entry: WorkflowDocumentEntry,
        declarations: FunctionDeclarations,
    ) -> Result<Self, WorkflowDocumentError> {
        let identity = document.identity();
        lash_kernel_doc::check_declarations(&document, &declarations)?;
        lash_kernel_doc::validate_document(&document, &declarations)?;
        let graph = match &entry {
            WorkflowDocumentEntry::Entry { function } => {
                if !document.entries.contains_key(function) {
                    return Err(WorkflowDocumentError::NoSuchEntry {
                        document: identity,
                        entry: function.clone(),
                    });
                }
                derive_entry(&document, &declarations, function)?
            }
            WorkflowDocumentEntry::Main => derive(&document, &declarations)?,
        };
        Ok(Self::hold(identity, entry, document, graph, declarations))
    }

    /// [`Self::entered`] with the declarations `document` lists taken from
    /// `functions`: a registry, or a decoded [`FunctionDeclarations`].
    ///
    /// # Errors
    ///
    /// As [`Self::entered`]; a function `functions` does not hold is a
    /// missing declaration.
    pub fn derive(
        document: Document,
        entry: WorkflowDocumentEntry,
        functions: &dyn FunctionCatalog,
    ) -> Result<Self, WorkflowDocumentError> {
        let declarations = lash_kernel_doc::required_declarations(&document, functions)?;
        Self::entered(document, entry, declarations)
    }

    /// Admit an unstored document against a workflow environment and retain
    /// the view that admission derives. An entry requires only its called and
    /// spawned effects; main uses the session-style document admission. This
    /// stores nothing and runs no effect.
    ///
    /// # Errors
    ///
    /// The checker's refusal or an entry the document does not list.
    pub fn admit(
        document: Document,
        entry: WorkflowDocumentEntry,
        environment: &Environment<'_>,
    ) -> Result<Self, WorkflowAdmissionRefusal> {
        let (identity, graph) = match &entry {
            WorkflowDocumentEntry::Entry { function } => {
                admit_entry(&document, function, environment)?
            }
            WorkflowDocumentEntry::Main => {
                let admitted = crate::admit(&document, environment)
                    .map_err(WorkflowAdmissionRefusal::Document)?;
                (admitted.identity, admitted.graph)
            }
        };
        let declarations = lash_kernel_doc::required_declarations(&document, environment.functions)
            .map_err(WorkflowAdmissionRefusal::Declarations)?;
        Ok(Self::hold(identity, entry, document, graph, declarations))
    }

    fn hold(
        identity: DocumentId,
        entry: WorkflowDocumentEntry,
        document: Document,
        graph: Graph,
        declarations: FunctionDeclarations,
    ) -> Self {
        Self {
            reference: WorkflowDocumentRef {
                document: identity,
                entry,
            },
            document: Arc::new(document),
            graph: Arc::new(graph),
            declarations: Arc::new(declarations),
        }
    }

    /// What an execution of this document names it by.
    pub fn reference(&self) -> &WorkflowDocumentRef {
        &self.reference
    }

    /// The document's identity.
    pub fn identity(&self) -> DocumentId {
        self.reference.document
    }

    /// The kernel document: the only authority.
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// The total view derived from the document.
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// The definition of every library function the document's `use` lines
    /// name, as it was linked: what a reader needs for signatures, with no
    /// engine, native code or registry of its own.
    pub fn declarations(&self) -> &FunctionDeclarations {
        &self.declarations
    }

    /// The unit an execution enters: `main`, or the entry function.
    pub fn entry_unit(&self) -> Unit {
        self.reference.entry.unit()
    }

    /// The entry function's signature; `None` for `main`.
    pub fn entry_signature(&self) -> Option<&Signature> {
        match &self.reference.entry {
            WorkflowDocumentEntry::Main => None,
            WorkflowDocumentEntry::Entry { function } => self
                .document
                .entries
                .get(function)
                .map(|entry| &entry.signature),
        }
    }
}

/// Why a document has no view.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowDocumentError {
    #[error("the document does not link: {0}")]
    Refused(#[from] Refusal),
    #[error("document `{document}` has no entry `{entry}`")]
    NoSuchEntry { document: DocumentId, entry: Name },
    #[error("the document's declarations are refused: {0}")]
    Declarations(#[from] lash_kernel_doc::DeclarationRefusal),
    #[error("the document is not well formed: {0}")]
    Invalid(#[from] lash_kernel_doc::Invalid),
}

/// Why a document was not admitted. Nothing was stored.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkflowAdmissionRefusal {
    /// The opaque host blob failed receipt validation. Nothing was admitted.
    Bytes(lash_kernel_doc::DocumentBytesRefusal),
    /// The checker refused the document, naming every fault at its site:
    /// a missing effect, an effect whose signature does not serve, a
    /// function identity the environment does not hold, an unbound
    /// variable, a node structural validation refuses.
    Document(Refusal),
    /// The selected entry's called/spawned effect closure cannot be certified.
    EntryEffects(EntryEffectsRefusal),
    /// The document does not list the entry.
    NoSuchEntry { entry: Name },
    /// The catalog cannot be stated as effects.
    Boundary { message: String },
    /// The environment's library does not hold a function the document
    /// lists as the document lists it.
    Declarations(lash_kernel_doc::DeclarationRefusal),
}

impl WorkflowAdmissionRefusal {
    /// A stable local snake_case name for this top-level alternative.
    /// Existing names keep their meaning; new alternatives add names. These
    /// are not HTTP statuses, and old clients may not recognize future names.
    /// For `Document`, inspect each nested `RefusalReason::code()`; multiple errors retain their detail.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Bytes(_) => "bytes",
            Self::Document(_) => "document",
            Self::EntryEffects(_) => "entry_effects",
            Self::NoSuchEntry { .. } => "no_such_entry",
            Self::Boundary { .. } => "boundary",
            Self::Declarations(_) => "declarations",
        }
    }
}

impl std::fmt::Display for WorkflowAdmissionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(refusal) => write!(f, "{refusal}"),
            Self::Document(refusal) => write!(f, "{refusal}"),
            Self::EntryEffects(refusal) => write!(f, "{refusal}"),
            Self::NoSuchEntry { entry } => write!(f, "the document lists no entry `{entry}`"),
            Self::Boundary { message } => f.write_str(message),
            Self::Declarations(refusal) => write!(f, "{refusal}"),
        }
    }
}

/// A definition is an entry template: other entries and main need no tool
/// offering here. They are structurally checked, and linked at their own start.
fn admit_entry(
    document: &Document,
    entry: &Name,
    environment: &Environment<'_>,
) -> Result<(DocumentId, Graph), WorkflowAdmissionRefusal> {
    if !document.entries.contains_key(entry) {
        return Err(WorkflowAdmissionRefusal::NoSuchEntry {
            entry: entry.clone(),
        });
    }
    let graph = derive_entry(document, environment.functions, entry)
        .map_err(WorkflowAdmissionRefusal::Document)?;
    let effects = entry_effects(document, &graph, environment.functions, entry)
        .map_err(WorkflowAdmissionRefusal::EntryEffects)?;
    let mut errors = Vec::new();
    for (effect, expected) in effects {
        let reason = match environment.effects.get(&effect) {
            None => Some(RefusalReason::MissingEffect { effect }),
            Some(provided) => signature_mismatch(provided, &expected).map(|mismatch| {
                RefusalReason::EffectSignature {
                    effect,
                    expected,
                    provided: provided.clone(),
                    mismatch: Box::new(mismatch),
                }
            }),
        };
        if let Some(reason) = reason {
            errors.push(Refused { site: None, reason });
        }
    }
    if !errors.is_empty() {
        return Err(WorkflowAdmissionRefusal::Document(Refusal { errors }));
    }
    Ok((document.identity(), graph))
}
