//! A definition as a workflow document a host reads, and a host's document
//! admitted as a definition.
//!
//! The document a host reads is the kernel document itself with the total
//! view `lash-kernel-check` derives from it: node ids, edges, scopes, type
//! facets, effect sets and execution sites. Beside it the store holds only
//! the declarations of the library functions it lists, which every read
//! links it against, so a reader never needs the library of the build that
//! wrote it. Nothing the document says about itself is trusted: the view is
//! derived again on every read. Publication checks the document
//! structurally, then admits only the selected entry's called and spawned
//! effects against its offering.

use std::sync::Arc;

use lash_kernel_check::Environment;
pub use lash_kernel_check::{WorkflowAdmissionRefusal, WorkflowDocument, WorkflowDocumentError};
use lash_kernel_doc::{
    Document, FunctionRegistry, Name, Signature, WorkflowDocumentEntry, WorkflowDocumentRef,
};

use super::{
    KernelProcessDefinition, KernelProcessEngine, LASH_VM_ENGINE_KIND, definition_draft,
    entry_signature,
};
use crate::HostBoundary;

/// What a document is edited and admitted against: the effects the
/// processes of its definitions would be offered, and the library
/// functions the engine's workers hold.
#[derive(Clone)]
pub struct WorkflowEnvironment {
    effects: std::collections::BTreeMap<lash_kernel_doc::EffectName, Signature>,
    functions: Arc<FunctionRegistry>,
    workers: lash_vm_client::service::Service,
}

impl std::fmt::Debug for WorkflowEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowEnvironment")
            .field("effects", &self.effects)
            .field("functions", &self.functions)
            .finish_non_exhaustive()
    }
}

impl WorkflowEnvironment {
    /// The registered engine's worker pool, used for isolated source import.
    pub fn worker_service(&self) -> &lash_vm_client::service::Service {
        &self.workers
    }

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

fn store_fault(error: super::DocumentStoreError) -> lash_core::PluginError {
    match error {
        super::DocumentStoreError::Store(error) => lash_core::PluginError::from(error),
        other => unresolvable(other.to_string()),
    }
}

impl KernelDocumentProvider {
    /// The document `reference` names, from what the store retains: the
    /// document and the declarations it was stored with, never this
    /// engine's registry (FIG-6379).
    async fn read(
        &self,
        reference: &WorkflowDocumentRef,
    ) -> Result<super::StoredWorkflow, lash_core::PluginError> {
        self.engine
            .documents
            .workflow(reference)
            .await
            .map_err(store_fault)
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessDocumentProvider for KernelDocumentProvider {
    async fn document(
        &self,
        payload: &serde_json::Value,
    ) -> Result<lash_core::ProcessDocumentRead, lash_core::PluginError> {
        let definition = super::payload_definition(payload)?;
        let reference = WorkflowDocumentRef {
            document: definition.document,
            entry: WorkflowDocumentEntry::Entry {
                function: definition.entry.clone(),
            },
        };
        let read = match self.read(&reference).await? {
            super::StoredWorkflow::Read(read) => read,
            super::StoredWorkflow::Unretained(super::Unretained::Missing { artifact }) => {
                return Ok(lash_core::ProcessDocumentRead::ArtifactMissing { artifact });
            }
            super::StoredWorkflow::Unretained(super::Unretained::Corrupt { artifact, message }) => {
                return Ok(lash_core::ProcessDocumentRead::ArtifactCorrupt { artifact, message });
            }
        };
        let document = read.document;
        if !document
            .document()
            .manifest
            .functions
            .keys()
            .eq(definition.functions.iter())
        {
            return Err(unresolvable(format!(
                "the definition does not name the library functions document `{}` lists",
                definition.document
            )));
        }
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
        Ok(match self.read(reference).await? {
            super::StoredWorkflow::Read(read) => lash_core::ProcessExecutionDocumentRead::Read(
                lash_core::ProcessDocument::new(*read),
            ),
            super::StoredWorkflow::Unretained(super::Unretained::Missing { artifact }) => {
                lash_core::ProcessExecutionDocumentRead::ArtifactMissing { artifact }
            }
            super::StoredWorkflow::Unretained(super::Unretained::Corrupt { artifact, message }) => {
                lash_core::ProcessExecutionDocumentRead::ArtifactCorrupt { artifact, message }
            }
        })
    }

    /// The second document writer, until its module field is deleted
    /// (FIG-6352 C1c): the bytes are decoded, checked against the reference
    /// they are named by and against this engine's library, and stored
    /// through [`super::KernelDocuments::publish`] like every document.
    async fn retain_module(
        &self,
        claim: &lash_core::ReferrerClaim,
        module: &lash_core::DeclaredModuleArtifact,
    ) -> Result<(), lash_core::PluginError> {
        let document = lash_kernel_doc::Document::from_json(&module.bytes).map_err(|error| {
            unresolvable(format!("the declared module does not decode: {error}"))
        })?;
        let identity = document.identity();
        if identity.to_string() != module.module_ref {
            return Err(unresolvable(format!(
                "the declared module `{}` is the document `{identity}`",
                module.module_ref
            )));
        }
        let functions = self.engine.functions.as_ref();
        lash_kernel_doc::validate_document(&document, functions)
            .map_err(|error| unresolvable(error.to_string()))?;
        let declarations = lash_kernel_doc::CheckedDeclarations::required(&document, functions)
            .map_err(|error| unresolvable(error.to_string()))?;
        self.engine
            .documents
            .publish(claim, &document, &declarations)
            .await
            .map(|_| ())
            .map_err(store_fault)
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
            workers: self.engine.workers.clone(),
        }))
    }

    async fn replay(
        &self,
        request: lash_core::ProcessDocument,
    ) -> Result<lash_core::ProcessDocument, lash_core::PluginError> {
        let request = request
            .downcast::<super::ProcessReplayRequest>()
            .map_err(|_| {
                lash_core::PluginError::Invoke(
                    "the kernel engine replays a `ProcessReplayRequest`".to_owned(),
                )
            })?;
        let answers = self
            .engine
            .replay(&request)
            .await
            .map_err(|fault| lash_core::PluginError::Invoke(fault.to_string()))?;
        Ok(lash_core::ProcessDocument::new(answers))
    }

    fn counts_statements(&self, settings: Option<&lash_core::EngineSettings>) -> bool {
        super::KernelRecordedSettings::observed(settings) == lash_kernel_vm::Observe::Counts
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
            WorkflowDocument::admit(
                request.document,
                WorkflowDocumentEntry::Entry {
                    function: request.entry.clone(),
                },
                &environment,
            )
        };
        let document = match admission {
            Ok(document) => document,
            Err(refusal) => return refused(refusal),
        };
        let Some(signature) = document.entry_signature().map(entry_signature) else {
            return refused(WorkflowAdmissionRefusal::NoSuchEntry {
                entry: request.entry,
            });
        };
        let draft_of = |entry: &Name| {
            definition_draft(&KernelProcessDefinition::of(
                document.document(),
                document.identity(),
                entry.clone(),
            ))
            .map_err(|error| unresolvable(error.to_string()))
        };
        let draft = draft_of(&request.entry)?;
        let siblings = document
            .document()
            .entries
            .keys()
            .filter(|entry| **entry != request.entry)
            .map(draft_of)
            .collect::<Result<Vec<_>, _>>()?;
        // Only an admitted document is held, with the declarations it was
        // admitted against.
        let declarations = lash_kernel_doc::CheckedDeclarations::required(
            document.document(),
            self.engine.functions.as_ref(),
        )
        .map_err(|refusal| store_fault(refusal.into()))?;
        self.engine
            .documents
            .publish(claim, document.document(), &declarations)
            .await
            .map_err(store_fault)?;
        Ok(lash_core::ProcessDocument::new(
            WorkflowAdmissionOutcome::Admitted(Box::new(AdmittedWorkflow {
                draft,
                signature,
                siblings,
                document,
            })),
        ))
    }
}
