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
    FunctionRegistry, IdentifiedDocument, Name, WorkflowDocumentEntry, WorkflowDocumentRef,
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
    operations: lash_kernel_dialect::Operations,
    functions: Arc<FunctionRegistry>,
    workers: lash_vm_client::service::Service,
}

impl std::fmt::Debug for WorkflowEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowEnvironment")
            .field("operations", &self.operations)
            .field("functions", &self.functions)
            .finish_non_exhaustive()
    }
}

impl WorkflowEnvironment {
    /// The registered engine's worker pool, used for isolated source import.
    pub fn worker_service(&self) -> &lash_vm_client::service::Service {
        &self.workers
    }

    /// The operations a document may perform, each with what a perform of
    /// it requires.
    pub fn operations(&self) -> &lash_kernel_dialect::Operations {
        &self.operations
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
        environment.effects = requirements(&self.operations);
        environment.internal = Box::new(|effect| {
            lash_kernel_dialect::QualifiedOperation::from_effect(effect)
                .ok()
                .and_then(|operation| self.operations.get(&operation))
                .is_some_and(|offered| {
                    offered.visibility == lash_kernel_dialect::Visibility::Internal
                })
        });
        environment
    }
}

/// What the checker admits a document against: each operation's effect
/// name with the requirement it is offered under.
fn requirements(
    operations: &lash_kernel_dialect::Operations,
) -> std::collections::BTreeMap<lash_kernel_doc::EffectName, lash_kernel_doc::OperationRequirement>
{
    operations
        .iter()
        .map(|(operation, offered)| (operation.effect_name(), offered.requirement.clone()))
        .collect()
}

/// The catalog a process would run under, whose environment is asked for.
#[derive(Clone, Debug)]
pub struct WorkflowEnvironmentRequest {
    pub packages: lash_execution_contracts::CellPackages,
    pub tool_catalog: Arc<lash_core::ToolCatalog>,
}

/// Internal integrator admission seam: the facade has validated the host's
/// opaque bytes once; admission shares that decoded value with publication.
#[derive(Clone, Debug)]
pub struct WorkflowAdmissionRequest {
    pub packages: lash_execution_contracts::WorkflowPackages,
    pub document: IdentifiedDocument,
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
    pub draft: lash_execution_contracts::process::ProcessDefinitionDraft,
    pub signature: lash_execution_contracts::process::ProcessSignature,
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

fn unresolvable(message: String) -> lash_execution_contracts::plugin_error::PluginError {
    lash_execution_contracts::plugin_error::PluginError::from(
        lash_execution_contracts::process::ProcessDefinitionRefusal::UnresolvableDefinition {
            engine_kind: LASH_VM_ENGINE_KIND.into(),
            message,
        },
    )
}

pub(super) fn store_fault(
    error: super::DocumentStoreError,
) -> lash_execution_contracts::plugin_error::PluginError {
    match error {
        super::DocumentStoreError::Store(error) => {
            lash_execution_contracts::plugin_error::PluginError::from(error)
        }
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
    ) -> Result<super::StoredWorkflow, lash_execution_contracts::plugin_error::PluginError> {
        self.engine
            .documents
            .workflow(reference)
            .await
            .map_err(store_fault)
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessDocumentProvider for KernelDocumentProvider {
    async fn start_requirements(
        &self,
        payload: &serde_json::Value,
    ) -> Result<
        Option<lash_vocabulary::claims::ClaimRequirements>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let definition = super::payload_definition(payload)?;
        let Ok((document, _)) = self.engine.entry(&definition).await.map_err(store_fault)? else {
            return Ok(None);
        };
        // The record its code derives over the set it is linked under: the
        // node's own registry, or a stored set's, read once per set, never
        // per declaration.
        let (_, functions) = self
            .engine
            .linked_packages(definition.packages)
            .await
            .map_err(|message| {
                lash_execution_contracts::plugin_error::PluginError::Session(format!(
                    "the package surface a start is linked under is not read: {message}"
                ))
            })?;
        Ok(
            lash_kernel_doc::CheckedDeclarations::required(document.document(), functions.as_ref())
                .ok()
                .map(|checked| checked.requirements(document.document())),
        )
    }

    async fn owned_operations(
        &self,
        payload: &serde_json::Value,
        performed: &lash_execution_contracts::process::PerformedEffects,
    ) -> Result<
        lash_execution_contracts::process::OwnedOperations,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.engine.owned_operations(payload, performed).await
    }

    fn definition_packages(
        &self,
        payload: &serde_json::Value,
    ) -> Result<lash_kernel_doc::PackageSetDigest, lash_execution_contracts::PluginError> {
        Ok(super::payload_definition(payload)?.packages)
    }

    fn initial_packages(
        &self,
    ) -> Result<lash_execution_contracts::CellPackages, lash_execution_contracts::PluginError> {
        let held = self.held_packages()?;
        Ok(lash_execution_contracts::CellPackages::held(&held))
    }

    fn held_packages(
        &self,
    ) -> Result<Arc<lash_kernel_package::HeldPackages>, lash_execution_contracts::PluginError> {
        self.engine
            .workers
            .config()
            .held_packages()
            .map_err(|error| lash_execution_contracts::PluginError::Registration(error.to_string()))
    }

    async fn package_view(
        &self,
        packages: lash_kernel_doc::PackageSetDigest,
    ) -> Result<
        Arc<dyn lash_kernel_dialect::ExportView + Send + Sync>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (surface, functions) =
            self.engine
                .linked_packages(packages)
                .await
                .map_err(|message| {
                    lash_execution_contracts::plugin_error::PluginError::Session(format!(
                        "the package surface {packages} is not read: {message}"
                    ))
                })?;
        Ok(Arc::new(lash_kernel_package::SharedPackageView::stored(
            surface, functions,
        ))
            as Arc<dyn lash_kernel_dialect::ExportView + Send + Sync>)
    }

    async fn document(
        &self,
        payload: &serde_json::Value,
    ) -> Result<
        lash_execution_contracts::process::ProcessDocumentRead,
        lash_execution_contracts::plugin_error::PluginError,
    > {
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
                return Ok(
                    lash_execution_contracts::process::ProcessDocumentRead::ArtifactMissing {
                        artifact,
                    },
                );
            }
            super::StoredWorkflow::Unretained(super::Unretained::Corrupt { artifact, message }) => {
                return Ok(
                    lash_execution_contracts::process::ProcessDocumentRead::ArtifactCorrupt {
                        artifact,
                        message,
                    },
                );
            }
        };
        let document = read.document;
        let (surface, functions) = self
            .engine
            .linked_packages(definition.packages)
            .await
            .map_err(unresolvable)?;
        let selected =
            lash_kernel_package::SelectedPackages::new(&surface, &definition.declared_packages);
        let reach = lash_kernel_check::requirements(document.document(), functions.as_ref());
        let members = selected.undeclared(
            &reach.functions,
            reach.effects.iter().chain(reach.performs.keys()),
        );
        if !members.is_empty() {
            return Err(unresolvable(
                WorkflowAdmissionRefusal::UndeclaredMembers { members }.to_string(),
            ));
        }
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
        let signature = document.entry_signature().map_or(
            lash_execution_contracts::process::ProcessSignature::Unknown,
            entry_signature,
        );
        Ok(
            lash_execution_contracts::process::ProcessDocumentRead::Inspected(Box::new(
                lash_execution_contracts::process::InspectedProcessDefinition {
                    definition: lash_execution_contracts::process::ProcessDefinition::new(
                        draft.id(),
                        signature,
                    ),
                    document: lash_execution_contracts::process::ProcessDocument::new(document),
                },
            )),
        )
    }

    async fn document_ref(
        &self,
        payload: &serde_json::Value,
    ) -> Result<
        lash_execution_contracts::process::ProcessDocumentRefRead,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let definition = super::payload_definition(payload)?;
        Ok(
            lash_execution_contracts::process::ProcessDocumentRefRead::Named(WorkflowDocumentRef {
                document: definition.document,
                entry: WorkflowDocumentEntry::Entry {
                    function: definition.entry,
                },
            }),
        )
    }

    async fn execution_document(
        &self,
        reference: &WorkflowDocumentRef,
    ) -> Result<
        lash_execution_contracts::process::ProcessExecutionDocumentRead,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(match self.read(reference).await? {
            super::StoredWorkflow::Read(read) => {
                lash_execution_contracts::process::ProcessExecutionDocumentRead::Read(
                    lash_execution_contracts::process::ProcessDocument::new(*read),
                )
            }
            super::StoredWorkflow::Unretained(super::Unretained::Missing { artifact }) => {
                lash_execution_contracts::process::ProcessExecutionDocumentRead::ArtifactMissing {
                    artifact,
                }
            }
            super::StoredWorkflow::Unretained(super::Unretained::Corrupt { artifact, message }) => {
                lash_execution_contracts::process::ProcessExecutionDocumentRead::ArtifactCorrupt {
                    artifact,
                    message,
                }
            }
        })
    }

    async fn environment(
        &self,
        request: lash_execution_contracts::process::ProcessDocument,
    ) -> Result<
        lash_execution_contracts::process::ProcessDocument,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let request = request
            .downcast::<WorkflowEnvironmentRequest>()
            .map_err(|_| {
                lash_execution_contracts::plugin_error::PluginError::Invoke(
                    "the kernel engine answers a `WorkflowEnvironmentRequest`".to_owned(),
                )
            })?;
        let mut boundary = HostBoundary::of_catalog(&request.tool_catalog)
            .map_err(|error| unresolvable(error.to_string()))?;
        boundary.in_packages(&request.packages.view());
        Ok(lash_execution_contracts::process::ProcessDocument::new(
            WorkflowEnvironment {
                operations: boundary.operations().clone(),
                functions: Arc::clone(request.packages.catalog()),
                workers: self.engine.workers.clone(),
            },
        ))
    }

    async fn replay(
        &self,
        request: lash_execution_contracts::process::ProcessDocument,
    ) -> Result<
        lash_execution_contracts::process::ProcessDocument,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let request = request
            .downcast::<super::ProcessReplayRequest>()
            .map_err(|_| {
                lash_execution_contracts::plugin_error::PluginError::Invoke(
                    "the kernel engine replays a `ProcessReplayRequest`".to_owned(),
                )
            })?;
        let answers = self.engine.replay(&request).await.map_err(|fault| {
            lash_execution_contracts::plugin_error::PluginError::Invoke(fault.to_string())
        })?;
        Ok(lash_execution_contracts::process::ProcessDocument::new(
            answers,
        ))
    }

    fn counts_statements(
        &self,
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> bool {
        super::KernelRecordedSettings::observed(settings) == lash_kernel_vm::ObservationMode::Counts
    }

    async fn admit(
        &self,
        claim: &lash_core::ReferrerClaim,
        request: lash_execution_contracts::process::ProcessDocument,
    ) -> Result<
        lash_execution_contracts::process::ProcessDocument,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let request = request
            .downcast::<WorkflowAdmissionRequest>()
            .map_err(|_| {
                lash_execution_contracts::plugin_error::PluginError::Invoke(
                    "the kernel engine admits a `WorkflowAdmissionRequest`".to_owned(),
                )
            })?;
        let refused = |refusal| {
            Ok(lash_execution_contracts::process::ProcessDocument::new(
                WorkflowAdmissionOutcome::Refused(refusal),
            ))
        };
        let mut boundary = match HostBoundary::of_catalog(&request.tool_catalog) {
            Ok(boundary) => boundary,
            Err(error) => {
                return refused(WorkflowAdmissionRefusal::Boundary {
                    message: error.to_string(),
                });
            }
        };
        boundary.in_packages(&request.packages.view());
        let identified = request.document;
        // A document written under a stored set is checked, and held, against
        // that set's definitions.
        let functions = Arc::clone(request.packages.packages().catalog());
        let selected = request.packages.view();
        let reach = lash_kernel_check::requirements(identified.document(), functions.as_ref());
        let members = selected.undeclared(
            &reach.functions,
            reach.effects.iter().chain(reach.performs.keys()),
        );
        if !members.is_empty() {
            return refused(WorkflowAdmissionRefusal::UndeclaredMembers { members });
        }
        let catalog = lash_kernel_package::SelectedCatalog {
            catalog: functions.as_ref(),
            packages: &selected,
        };
        let admission = {
            let mut environment = Environment::new(&catalog);
            environment.effects = requirements(boundary.operations())
                .into_iter()
                .filter(|(effect, _)| {
                    lash_kernel_dialect::QualifiedOperation::from_effect(effect)
                        .is_ok_and(|operation| selected.permits_operation(&operation))
                })
                .collect();
            environment.internal = Box::new(|effect| {
                boundary.effect(effect).is_some_and(|(_, offered)| {
                    offered.visibility == lash_kernel_dialect::Visibility::Internal
                })
            });
            WorkflowDocument::admit(
                identified.document().clone(),
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
        let draft = definition_draft(&KernelProcessDefinition::of(
            document.document(),
            document.identity(),
            request.entry.clone(),
            request.packages.packages().digest(),
            request.packages.declared().clone(),
        ))
        .map_err(|error| unresolvable(error.to_string()))?;
        // Only an admitted document is held, with the declarations it was
        // admitted against.
        let declarations =
            lash_kernel_doc::CheckedDeclarations::required(document.document(), functions.as_ref())
                .map_err(|refusal| store_fault(refusal.into()))?;
        self.engine
            .documents
            .publish(claim, &identified, &declarations)
            .await
            .map_err(store_fault)?;
        Ok(lash_execution_contracts::process::ProcessDocument::new(
            WorkflowAdmissionOutcome::Admitted(Box::new(AdmittedWorkflow {
                draft,
                signature,
                document,
            })),
        ))
    }
}
