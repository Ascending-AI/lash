//! Host authoring projects accepted advertisements through the runtime; Core carries neutral inputs only.
use super::documents::refused;
use super::{KernelDocuments, KernelProcessEngine, definition_draft, entry_signature};
use lash_execution_contracts::process::linking::*;
use lash_execution_contracts::process::*;
use lash_kernel_dialect::{FunctionMember, NamespaceKind, NamespaceMember, Namespaces, ValueType};
use lash_kernel_doc::{DocumentBytes, Export, FunctionExport};

use lash_vocabulary::{names::*, package::*};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowEnvironmentRequest {
    pub snapshot: NamespaceSnapshot,
    pub context: PublicationContext,
    pub attachment: Attachment,
}
#[derive(Clone, Debug)]
pub struct WorkflowPublishRequest {
    pub document: DocumentBytes,
    pub selector: StartSelector,
    pub environment: WorkflowEnvironmentRequest,
}
#[derive(Clone)]
pub struct WorkflowEnvironment {
    namespaces: Namespaces,
    workers: lash_vm_client::service::Service,
    functions: Arc<lash_kernel_doc::FunctionRegistry>,
}
impl WorkflowEnvironment {
    pub fn namespaces(&self) -> &Namespaces {
        &self.namespaces
    }
    pub fn worker_service(&self) -> &lash_vm_client::service::Service {
        &self.workers
    }
    pub fn functions(&self) -> &lash_kernel_doc::FunctionRegistry {
        &self.functions
    }
    pub fn checker(&self) -> lash_kernel_check::Environment<'_> {
        lash_kernel_check::Environment::new(self.functions.as_ref())
    }
}
fn descriptor(name: QualifiedName, function: &FunctionExport) -> FunctionMember {
    FunctionMember {
        name,
        signature: function.signature.clone(),
        convention: function.convention.semantic(),
        asynchronous: function.asynchronous,
        rest: function.rest,
    }
}
impl KernelDocuments {
    pub async fn namespace(
        &self,
        request: &WorkflowEnvironmentRequest,
    ) -> Result<Namespaces, EnginePortError> {
        let namespace = lash_kernel_package::Namespace {
            advertisements: request
                .snapshot
                .advertisements
                .iter()
                .map(|ad| (&ad.publication.context, &ad.advertisement))
                .collect(),
        };
        namespace.validate().map_err(refused)?;
        let mut packages = BTreeMap::new();
        let mut loaded = BTreeMap::new();
        let mut deferred = std::collections::BTreeSet::new();
        for ad in &request.snapshot.advertisements {
            if ad.publication.context.domain != request.context.domain
                || !(ad.publication.context.scope == PublicationScope::Host
                    || ad.publication.context.scope == request.context.scope)
            {
                continue;
            }
            match &ad.advertisement {
                PackageAdvertisement::Deferred(package) => {
                    if package.visibility == Visibility::Public
                        && request.attachment.deferred.contains(&package.path)
                    {
                        deferred.insert(package.path.clone());
                    }
                }
                PackageAdvertisement::Enumerated(package) => {
                    let mut members = BTreeMap::new();
                    for (name, member) in &package.members {
                        if member.visibility != Visibility::Public {
                            continue;
                        }
                        let qualified = QualifiedName {
                            package: package.path.clone(),
                            member: name.clone(),
                            type_member: None,
                        };
                        if !request.attachment.packages.contains(&package.path)
                            && !request.attachment.members.contains(&qualified)
                        {
                            continue;
                        }
                        let kind = match &member.kind {
                            MemberKind::Operation(offer) => {
                                NamespaceKind::Operation(offer.clone().into())
                            }
                            MemberKind::Export { link, export } => {
                                if !loaded.contains_key(link) {
                                    loaded.insert(*link, self.load_linked(*link).await?);
                                }
                                let content = &loaded[link];
                                match content.export(*link, export).map_err(refused)? {
                                    Export::Function(f) => {
                                        NamespaceKind::Function(descriptor(qualified.clone(), &f))
                                    }
                                    Export::Error { .. } => NamespaceKind::Error,
                                    Export::Type(ty) => {
                                        let members = ty
                                            .members
                                            .into_iter()
                                            .map(|(name, member)| {
                                                let qualified = QualifiedName {
                                                    type_member: Some(name.clone()),
                                                    ..qualified.clone()
                                                };
                                                let function =
                                                    descriptor(qualified, member.function());
                                                let projected = match member {
                                                    lash_kernel_doc::TypeMember::Static(_) => {
                                                        lash_kernel_dialect::TypeMember::Static(
                                                            function,
                                                        )
                                                    }
                                                    lash_kernel_doc::TypeMember::Method(_) => {
                                                        lash_kernel_dialect::TypeMember::Method(
                                                            function,
                                                        )
                                                    }
                                                    lash_kernel_doc::TypeMember::Property(_) => {
                                                        lash_kernel_dialect::TypeMember::Property(
                                                            function,
                                                        )
                                                    }
                                                };
                                                (name, projected)
                                            })
                                            .collect();
                                        NamespaceKind::Type(ValueType {
                                            representation: ty.representation,
                                            members,
                                        })
                                    }
                                }
                            }
                        };
                        members.insert(
                            name.clone(),
                            NamespaceMember {
                                visibility: member.visibility,
                                kind,
                            },
                        );
                    }
                    packages.insert(package.path.clone(), members);
                }
            }
        }
        Namespaces::new(packages, deferred, &|_| false).map_err(|e| {
            EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                message: e.to_string(),
            })
        })
    }
    pub async fn publish(
        &self,
        claim: &lash_core::ReferrerClaim,
        request: &WorkflowPublishRequest,
    ) -> Result<LinkedStart, EnginePortError> {
        let document = self.admit_document(claim, &request.document).await?;
        let fulfilled = self
            .fulfil(&FulfilRequest {
                document,
                selector: ReachSelector::Start(request.selector.clone()),
                snapshot: request.environment.snapshot.clone(),
                context: request.environment.context.clone(),
                purpose: LinkPurpose::Workflow {
                    attachment: request.environment.attachment.clone(),
                },
            })
            .await?;
        let start = LinkedStart {
            link: fulfilled.content.identity(),
            selector: request.selector.clone(),
        };
        self.hold_publications(claim, &request.environment.snapshot)
            .await?;
        self.hold(claim, &ContentRef::Link(start.link)).await?;
        Ok(start)
    }
}
impl KernelProcessEngine {
    /// Inspect the retained execution without realizing cell code or rebinding captures.
    pub async fn workflow_document(
        &self,
        code: &super::InlineRequest,
    ) -> Result<lash_kernel_check::WorkflowDocument, EnginePortError> {
        match code {
            super::InlineRequest::Linked { start } => {
                self.documents.execution_document(start).await
            }
            super::InlineRequest::Callable { function, .. } => {
                let loaded = self
                    .load_execution_content(code, &Default::default())
                    .await?;
                let loaded = self.admit_execution(loaded, &lash_kernel_doc::Selection::Fresh)?;
                let document = loaded
                    .content
                    .document(
                        loaded
                            .content
                            .link(function.callable.link)
                            .map_err(super::map_link_refusal)?
                            .document,
                    )
                    .map_err(super::map_link_refusal)?
                    .clone();
                lash_kernel_check::WorkflowDocument::captured(
                    document,
                    (**function).clone(),
                    &self.helpers,
                )
                .map_err(|error| match error {
                    lash_kernel_check::WorkflowDocumentError::Callable(refusal) => match refusal {
                        lash_kernel_doc::FunctionValueRefusal::EntryAnnotation { source } => {
                            EnginePortError::Refused(AdmissionRefusal::EntryAnnotation { source })
                        }
                        lash_kernel_doc::FunctionValueRefusal::SignatureRequired { source } => {
                            EnginePortError::Refused(AdmissionRefusal::SignatureRequired { source })
                        }
                        _ => EnginePortError::Refused(AdmissionRefusal::CallableIdentity {
                            callable: function.callable,
                        }),
                    },
                    other => EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                        message: other.to_string(),
                    }),
                })
            }
            super::InlineRequest::Unlinked { .. } => Err(EnginePortError::Unsupported {
                operation: "unrealized process code".into(),
            }),
        }
    }
}

pub(crate) struct KernelDocumentProvider {
    pub(crate) engine: Arc<KernelProcessEngine>,
}
fn plugin(error: EnginePortError) -> lash_execution_contracts::PluginError {
    // The neutral port retains typed refusals; the plugin boundary carries the complete cause.
    lash_execution_contracts::PluginError::Package(Box::new(error))
}
#[async_trait::async_trait]
impl ProcessDocumentProvider for KernelDocumentProvider {
    fn runtime_documents(&self) -> ProcessDocument {
        ProcessDocument::new(self.engine.documents.clone())
    }
    async fn fulfil(&self, request: &FulfilRequest) -> Result<Fulfilled, EnginePortError> {
        self.engine.documents.fulfil(request).await
    }
    async fn inspect_link(
        &self,
        link: LinkId,
        selector: &ReachSelector,
    ) -> Result<LinkInspection, EnginePortError> {
        self.engine.documents.inspect_link(link, selector).await
    }
    async fn hold(
        &self,
        claim: &lash_core::ReferrerClaim,
        reference: &ContentRef,
    ) -> Result<(), EnginePortError> {
        self.engine.documents.hold(claim, reference).await
    }
    async fn load(&self, reference: &ContentRef) -> Result<LoadedContent, EnginePortError> {
        self.engine.documents.load(reference).await
    }
    fn capabilities(&self) -> lash_vocabulary::claims::NodeRequirements {
        self.engine.machine_requirements()
    }
    async fn document_ref(
        &self,
        payload: &serde_json::Value,
    ) -> Result<ProcessDocumentRefRead, lash_execution_contracts::PluginError> {
        let definition = super::payload_definition(payload)?;
        let (link, selector) = definition.code.linked().ok_or_else(|| {
            plugin(EnginePortError::Unsupported {
                operation: "unrealized process code".into(),
            })
        })?;
        self.engine
            .documents
            .inspect_link(link, &selector)
            .await
            .map_err(plugin)?;
        Ok(ProcessDocumentRefRead::Named { link, selector })
    }
    async fn execution_document(
        &self,
        start: &LinkedStart,
    ) -> Result<ProcessExecutionDocumentRead, lash_execution_contracts::PluginError> {
        Ok(
            match self.engine.documents.execution_document(start).await {
                Ok(document) => ProcessExecutionDocumentRead::Read(ProcessDocument::new(document)),
                Err(EnginePortError::Missing { artifact }) => {
                    ProcessExecutionDocumentRead::ArtifactMissing { artifact }
                }
                Err(EnginePortError::Corrupt { artifact, message }) => {
                    ProcessExecutionDocumentRead::ArtifactCorrupt { artifact, message }
                }
                Err(error) => return Err(plugin(error)),
            },
        )
    }
    async fn document(
        &self,
        payload: &serde_json::Value,
    ) -> Result<ProcessDocumentRead, lash_execution_contracts::PluginError> {
        let definition = super::payload_definition(payload)?;
        let document = match self.engine.workflow_document(&definition.code).await {
            Ok(document) => document,
            Err(EnginePortError::Missing { artifact }) => {
                return Ok(ProcessDocumentRead::ArtifactMissing { artifact });
            }
            Err(EnginePortError::Corrupt { artifact, message }) => {
                return Ok(ProcessDocumentRead::ArtifactCorrupt { artifact, message });
            }
            Err(error) => return Err(plugin(error)),
        };
        let draft = definition_draft(&definition)
            .map_err(|e| lash_execution_contracts::PluginError::Session(e.to_string()))?;
        let signature = document
            .entry_signature()
            .map_or(ProcessSignature::Unknown, entry_signature);
        Ok(ProcessDocumentRead::Inspected(Box::new(
            InspectedProcessDefinition {
                definition: ProcessDefinition::new(draft.id(), signature),
                document: ProcessDocument::new(document),
            },
        )))
    }
    async fn admit(
        &self,
        claim: &lash_core::ReferrerClaim,
        request: ProcessDocument,
    ) -> Result<ProcessDocument, lash_execution_contracts::PluginError> {
        let request = request.downcast::<WorkflowPublishRequest>().map_err(|_| {
            lash_execution_contracts::PluginError::Session(
                "expected workflow publication request".into(),
            )
        })?;
        Ok(ProcessDocument::new(
            self.engine
                .documents
                .publish(claim, &request)
                .await
                .map_err(plugin)?,
        ))
    }
    async fn environment(
        &self,
        request: ProcessDocument,
    ) -> Result<ProcessDocument, lash_execution_contracts::PluginError> {
        let request = request
            .downcast::<WorkflowEnvironmentRequest>()
            .map_err(|_| {
                lash_execution_contracts::PluginError::Session(
                    "expected workflow environment request".into(),
                )
            })?;
        let namespaces = self
            .engine
            .documents
            .namespace(&request)
            .await
            .map_err(plugin)?;
        Ok(ProcessDocument::new(WorkflowEnvironment {
            namespaces,
            workers: self.engine.workers.clone(),
            functions: self.engine.functions.clone(),
        }))
    }
    async fn replay(
        &self,
        request: ProcessDocument,
    ) -> Result<ProcessDocument, lash_execution_contracts::PluginError> {
        let request = request
            .downcast::<super::ProcessReplayRequest>()
            .map_err(|_| {
                lash_execution_contracts::PluginError::Session("expected replay request".into())
            })?;
        self.engine
            .replay(&request)
            .await
            .map(ProcessDocument::new)
            .map_err(|error| lash_execution_contracts::PluginError::Session(error.to_string()))
    }
    fn counts_statements(&self, settings: Option<&EngineSettings>) -> bool {
        matches!(
            super::KernelRecordedSettings::observed(settings),
            lash_kernel_vm::ObservationMode::Counts
        )
    }
}
