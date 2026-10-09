//! A definition as a workflow document a host reads, and a host's document
//! admitted as a definition.
//!
//! The document a host reads is the kernel document itself with the total
//! view `lash-kernel-check` derives from it: node ids, edges, scopes, type
//! facets, effect sets and execution sites. Nothing else is stored and
//! nothing the document says about itself is trusted: the view is derived
//! again on every read. Publication is the checker's admission against the
//! effects the definition's processes would be offered.

use std::sync::Arc;

use lash_kernel_check::{Environment, Graph, Refusal};
use lash_kernel_doc::{Document, DocumentId, FunctionRegistry, Name, Signature, Unit};
use lash_trace::{WorkflowDocumentEntry, WorkflowDocumentRef, WorkflowOverlayDocument};

use super::{
    KernelProcessDefinition, KernelProcessEngine, LASH_VM_ENGINE_KIND, definition_draft,
    entry_signature,
};
use crate::HostBoundary;

/// One admitted document as a host reads it, entered where its reference
/// says: the document and everything derived from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowDocument {
    reference: WorkflowDocumentRef,
    document: Arc<Document>,
    graph: Arc<Graph>,
}

impl WorkflowDocument {
    /// `document` entered at `entry`, with the view `functions` derive.
    ///
    /// # Errors
    ///
    /// The checker's refusal of a document it cannot link.
    pub fn derive(
        document: Document,
        entry: WorkflowDocumentEntry,
        functions: &FunctionRegistry,
    ) -> Result<Self, WorkflowDocumentError> {
        let identity = document.identity()?;
        let graph = lash_kernel_check::derive(&document, functions)?;
        if let WorkflowDocumentEntry::Entry { function } = &entry
            && !document.entries.contains_key(function)
        {
            return Err(WorkflowDocumentError::NoSuchEntry {
                document: identity,
                entry: function.clone(),
            });
        }
        Ok(Self {
            reference: WorkflowDocumentRef {
                document: identity,
                entry,
            },
            document: Arc::new(document),
            graph: Arc::new(graph),
        })
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

    /// The unit an execution enters: `main`, or the entry function.
    pub fn entry_unit(&self) -> Unit {
        match &self.reference.entry {
            WorkflowDocumentEntry::Main => Unit::Main,
            WorkflowDocumentEntry::Entry { function } => Unit::Function(function.clone()),
        }
    }

    /// The entry function's signature; `None` for `main`.
    pub fn entry_signature(&self) -> Option<&Signature> {
        match &self.reference.entry {
            WorkflowDocumentEntry::Main => None,
            WorkflowDocumentEntry::Entry { function } => self.document.entries.get(function),
        }
    }

    /// The document as TypeScript that lowers back to the same behaviour,
    /// or the printer's typed refusal of a document it cannot spell.
    ///
    /// # Errors
    ///
    /// The printer's diagnostic.
    #[cfg(feature = "typescript")]
    pub fn typescript(&self) -> Result<String, lash_kernel_dialect::Diagnostic> {
        lash_dialect_typescript::print(&self.document)
    }

    /// What the execution overlay's reducer reads of this document: its
    /// reference and every execution site. An entry may call and spawn any
    /// declared function, so the sites of every unit are the document's.
    pub fn overlay_document(&self) -> WorkflowOverlayDocument {
        WorkflowOverlayDocument::new(
            self.reference.clone(),
            self.graph
                .execution_sites()
                .iter()
                .map(|site| site.site.clone()),
        )
    }
}

/// Why a stored document has no view.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowDocumentError {
    #[error("the document has no canonical encoding: {0}")]
    Encode(#[from] lash_kernel_doc::EncodeError),
    #[error("the document does not link: {0}")]
    Refused(#[from] Refusal),
    #[error("document `{document}` has no entry `{entry}`")]
    NoSuchEntry { document: DocumentId, entry: Name },
}

/// What a document is edited and admitted against: the effects the
/// processes of its definitions would be offered, and the library
/// functions the engine's workers hold.
#[derive(Clone, Debug)]
pub struct WorkflowEnvironment {
    effects: std::collections::BTreeMap<lash_kernel_doc::EffectName, Signature>,
    functions: Arc<FunctionRegistry>,
}

impl WorkflowEnvironment {
    /// The effects a document may perform, each with its signature.
    pub fn effects(&self) -> &std::collections::BTreeMap<lash_kernel_doc::EffectName, Signature> {
        &self.effects
    }

    /// The library functions a document may reference, by identity.
    pub fn functions(&self) -> &FunctionRegistry {
        &self.functions
    }

    /// The checker's environment: what [`lash_kernel_check::admit`] and a
    /// draft's transactions take. A process's `main` has no session
    /// bindings.
    pub fn checker(&self) -> Environment<'_> {
        let mut environment = Environment::new(self.functions.as_ref());
        environment.effects = self.effects.clone();
        environment
    }
}

/// The catalog a process would run under, whose environment is asked for.
#[derive(Clone, Debug)]
pub struct WorkflowEnvironmentRequest {
    pub tool_catalog: Arc<lash_core::ToolCatalog>,
}

/// A host's document, to be admitted as the definition of one of its
/// entries.
#[derive(Clone, Debug)]
pub struct WorkflowAdmissionRequest {
    pub document: Document,
    /// The entry the definition starts.
    pub entry: Name,
    /// The catalog a process of the definition would run under: the
    /// effects it is admitted against.
    pub tool_catalog: Arc<lash_core::ToolCatalog>,
}

/// A document admitted as a definition.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedWorkflow {
    /// The definition's descriptor, which the caller publishes.
    pub draft: lash_core::ProcessDefinitionDraft,
    pub signature: lash_core::ProcessSignature,
    /// The descriptors of the document's other entries, which a process of
    /// the definition starts by function reference.
    pub siblings: Vec<lash_core::ProcessDefinitionDraft>,
    pub document: WorkflowDocument,
}

/// Why a document was not admitted. Nothing was stored.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkflowAdmissionRefusal {
    /// The checker refused the document, naming every fault at its site:
    /// a missing effect, an effect whose signature does not serve, a
    /// function identity the environment does not hold, an unbound
    /// variable, a statement the statement rule refuses.
    Document(Refusal),
    /// The document does not list the entry.
    NoSuchEntry { entry: Name },
    /// The catalog cannot be stated as effects.
    Boundary { message: String },
}

impl std::fmt::Display for WorkflowAdmissionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Document(refusal) => write!(f, "{refusal}"),
            Self::NoSuchEntry { entry } => write!(f, "the document lists no entry `{entry}`"),
            Self::Boundary { message } => f.write_str(message),
        }
    }
}

/// The answer to admitting a document.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowAdmissionOutcome {
    Admitted(Box<AdmittedWorkflow>),
    Refused(WorkflowAdmissionRefusal),
}

pub(crate) struct KernelDocumentProvider {
    pub(crate) engine: Arc<KernelProcessEngine>,
}

fn unresolvable(message: String) -> lash_core::PluginError {
    lash_core::PluginError::from(
        lash_core::ProcessDefinitionRefusal::UnresolvableDefinition {
            engine_kind: LASH_VM_ENGINE_KIND.into(),
            message,
        },
    )
}

fn missing(document: &DocumentId) -> lash_core::ArtifactName {
    lash_core::ArtifactName {
        store: lash_core::ArtifactStoreId::VmModule,
        artifact_ref: document.to_string(),
    }
}

impl KernelDocumentProvider {
    async fn read(
        &self,
        document: &DocumentId,
        entry: WorkflowDocumentEntry,
    ) -> Result<Option<WorkflowDocument>, lash_core::PluginError> {
        let Some(stored) = self
            .engine
            .documents
            .get(document)
            .await
            .map_err(|error| unresolvable(error.to_string()))?
        else {
            return Ok(None);
        };
        WorkflowDocument::derive(stored, entry, &self.engine.functions)
            .map(Some)
            .map_err(|error| unresolvable(error.to_string()))
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessDocumentProvider for KernelDocumentProvider {
    async fn document(
        &self,
        payload: &serde_json::Value,
    ) -> Result<lash_core::ProcessDocumentRead, lash_core::PluginError> {
        let definition = super::payload_definition(payload)?;
        let entry = WorkflowDocumentEntry::Entry {
            function: definition.entry.clone(),
        };
        let Some(document) = self.read(&definition.document, entry).await? else {
            return Ok(lash_core::ProcessDocumentRead::ArtifactMissing {
                artifact: missing(&definition.document),
            });
        };
        let draft =
            definition_draft(&definition).map_err(|error| unresolvable(error.to_string()))?;
        let signature = document
            .entry_signature()
            .map_or(lash_core::ProcessSignature::Unknown, entry_signature);
        Ok(lash_core::ProcessDocumentRead::Inspected(Box::new(
            lash_core::InspectedProcessDefinition {
                definition: lash_core::ProcessDefinition::new(draft.id(), signature),
                document: lash_core::ProcessDocument::new(document),
            },
        )))
    }

    async fn document_ref(
        &self,
        payload: &serde_json::Value,
    ) -> Result<lash_core::ProcessDocumentRefRead, lash_core::PluginError> {
        let definition = super::payload_definition(payload)?;
        Ok(lash_core::ProcessDocumentRefRead::Named(
            WorkflowDocumentRef {
                document: definition.document,
                entry: WorkflowDocumentEntry::Entry {
                    function: definition.entry,
                },
            },
        ))
    }

    async fn execution_document(
        &self,
        reference: &WorkflowDocumentRef,
    ) -> Result<lash_core::ProcessExecutionDocumentRead, lash_core::PluginError> {
        Ok(
            match self
                .read(&reference.document, reference.entry.clone())
                .await?
            {
                Some(document) => lash_core::ProcessExecutionDocumentRead::Read(
                    lash_core::ProcessDocument::new(document),
                ),
                None => lash_core::ProcessExecutionDocumentRead::ArtifactMissing {
                    artifact: missing(&reference.document),
                },
            },
        )
    }

    async fn environment(
        &self,
        request: lash_core::ProcessDocument,
    ) -> Result<lash_core::ProcessDocument, lash_core::PluginError> {
        let request = request
            .downcast::<WorkflowEnvironmentRequest>()
            .map_err(|_| {
                lash_core::PluginError::Invoke(
                    "the kernel engine answers a `WorkflowEnvironmentRequest`".to_owned(),
                )
            })?;
        let boundary = HostBoundary::of_catalog(&request.tool_catalog)
            .map_err(|error| unresolvable(error.to_string()))?;
        Ok(lash_core::ProcessDocument::new(WorkflowEnvironment {
            effects: boundary.signatures(),
            functions: Arc::clone(&self.engine.functions),
        }))
    }

    async fn admit(
        &self,
        claim: &lash_core::ReferrerClaim,
        request: lash_core::ProcessDocument,
    ) -> Result<lash_core::ProcessDocument, lash_core::PluginError> {
        let request = request
            .downcast::<WorkflowAdmissionRequest>()
            .map_err(|_| {
                lash_core::PluginError::Invoke(
                    "the kernel engine admits a `WorkflowAdmissionRequest`".to_owned(),
                )
            })?;
        let refused = |refusal| {
            Ok(lash_core::ProcessDocument::new(
                WorkflowAdmissionOutcome::Refused(refusal),
            ))
        };
        let boundary = match HostBoundary::of_catalog(&request.tool_catalog) {
            Ok(boundary) => boundary,
            Err(error) => {
                return refused(WorkflowAdmissionRefusal::Boundary {
                    message: error.to_string(),
                });
            }
        };
        let admission = {
            let mut environment = Environment::new(self.engine.functions.as_ref());
            environment.effects = boundary.signatures();
            lash_kernel_check::admit(&request.document, &environment)
        };
        let admitted = match admission {
            Ok(admitted) => admitted,
            Err(refusal) => return refused(WorkflowAdmissionRefusal::Document(refusal)),
        };
        let Some(signature) = request.document.entries.get(&request.entry) else {
            return refused(WorkflowAdmissionRefusal::NoSuchEntry {
                entry: request.entry,
            });
        };
        let draft_of = |entry: &Name| {
            definition_draft(&KernelProcessDefinition {
                document: admitted.identity,
                entry: entry.clone(),
            })
            .map_err(|error| unresolvable(error.to_string()))
        };
        let draft = draft_of(&request.entry)?;
        let siblings = request
            .document
            .entries
            .keys()
            .filter(|entry| **entry != request.entry)
            .map(draft_of)
            .collect::<Result<Vec<_>, _>>()?;
        let signature = entry_signature(signature);
        // Only an admitted document is held.
        self.engine
            .documents
            .publish(claim, &request.document)
            .await
            .map_err(|error| match error {
                super::DocumentStoreError::Store(error) => lash_core::PluginError::from(error),
                other => unresolvable(other.to_string()),
            })?;
        Ok(lash_core::ProcessDocument::new(
            WorkflowAdmissionOutcome::Admitted(Box::new(AdmittedWorkflow {
                draft,
                signature,
                siblings,
                document: WorkflowDocument {
                    reference: WorkflowDocumentRef {
                        document: admitted.identity,
                        entry: WorkflowDocumentEntry::Entry {
                            function: request.entry,
                        },
                    },
                    document: Arc::new(request.document),
                    graph: Arc::new(admitted.graph),
                },
            })),
        ))
    }
}
