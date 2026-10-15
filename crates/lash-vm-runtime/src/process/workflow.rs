//! A definition as a workflow document a host reads, and a host's document
//! admitted as a definition.
//!
//! The document a host reads is the kernel document itself with the total
//! view `lash-kernel-check` derives from it: node ids, edges, scopes, type
//! facets, effect sets and execution sites. Nothing else is stored and
//! nothing the document says about itself is trusted: the view is derived
//! again on every read. Publication checks the document structurally, then
//! admits only the selected entry's called and spawned effects against its offering.

use std::sync::Arc;

use lash_kernel_check::Environment;
pub use lash_kernel_check::{WorkflowAdmissionRefusal, WorkflowDocument, WorkflowDocumentError};
use lash_kernel_doc::{
    Document, DocumentId, FunctionRegistry, Name, Signature, WorkflowDocumentEntry,
    WorkflowDocumentRef,
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

fn missing(document: &DocumentId) -> lash_core::ArtifactName {
    lash_core::ArtifactName {
        store: lash_core::ArtifactStoreId::KernelDocument,
        artifact_ref: document.to_string(),
    }
}

impl KernelDocumentProvider {
    async fn read(
        &self,
        document: &DocumentId,
        entry: WorkflowDocumentEntry,
    ) -> Result<Option<WorkflowDocument>, lash_core::PluginError> {
        let Some(stored) = self.engine.documents.get(document).await? else {
            return Ok(None);
        };
        WorkflowDocument::derive(stored, entry, self.engine.functions.as_ref())
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
            definition_draft(&KernelProcessDefinition {
                document: document.identity(),
                entry: entry.clone(),
            })
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
        // Only an admitted document is held.
        self.engine
            .documents
            .publish(claim, document.document())
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
                document,
            })),
        ))
    }
}
