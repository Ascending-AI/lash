//! Host-held artifacts (ADR 0113 §2.6, §3.5).
//!
//! A host keeps artifact bytes alive only through a [`HostArtifactPin`] it
//! minted: every publish adds the pin's edge, and releasing the pin ends it
//! for good. The pin is opaque; hosts never name a referrer of any other kind,
//! and a released pin can never publish again — a host that wants to publish
//! again mints a fresh one.
//!
//! A process definition is held the same way (ADR 0113 §3.6): an id is data
//! and holds nothing, so a host that wants a definition available between
//! starts pins it. Lash keeps no names or versions; a host that wants them
//! keeps `(name, version) -> (id, pin)` in its own tables.

use std::sync::Arc;

pub use lash_core::HostArtifactPin;
use lash_core::store::ArtifactCleanupLedger;
use lash_core::{
    ArtifactCleanup, ArtifactReferrer, ArtifactReferrerPorts, Clock, DefinitionAcquisition,
    ProcessDefinition, ProcessDefinitionDraft, ProcessDefinitionId, ProcessEngineRegistry,
    ProcessEnv, ProcessEnvRef, ProcessEnvStore, ReferrerClaim, ToolCatalog,
};

use crate::Result;

/// The host-visible judgment of an artifact operation.
/// Operational faults have no artifact refusal; their typed cause stays on
/// [`crate::EmbedError`]. The original error retains the judgment's detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostArtifactRefusal {
    PinReleased,
    DefinitionMissing,
    ArtifactMissing,
    Conflict,
    DefinitionRefused,
}

impl HostArtifactRefusal {
    /// A stable local snake_case name for this top-level alternative.
    /// Existing names keep their meaning; new alternatives add names. These
    /// are not HTTP statuses, and old clients may not recognize future names.
    pub fn code(&self) -> &'static str {
        match self {
            Self::PinReleased => "pin_released",
            Self::DefinitionMissing => "definition_missing",
            Self::ArtifactMissing => "artifact_missing",
            Self::Conflict => "conflict",
            Self::DefinitionRefused => "definition_refused",
        }
    }

    pub(crate) fn of_runtime(error: &lash_core::RuntimeError) -> Option<Self> {
        use lash_core::RuntimeErrorCode;
        match error.code {
            RuntimeErrorCode::ArtifactReferrerEnded
                if matches!(error.ended_referrer(), Some(ArtifactReferrer::HostPin(_))) =>
            {
                Some(Self::PinReleased)
            }
            RuntimeErrorCode::DefinitionMissing => Some(Self::DefinitionMissing),
            RuntimeErrorCode::ArtifactMissing => Some(Self::ArtifactMissing),
            RuntimeErrorCode::ArtifactConflict => Some(Self::Conflict),
            RuntimeErrorCode::DefinitionRefused => Some(Self::DefinitionRefused),
            _ => None,
        }
    }
}

/// The artifact stores of one core, as a host publishes into them.
#[derive(Clone)]
pub struct HostArtifacts {
    process_env: Arc<dyn ProcessEnvStore>,
    cleanup: Arc<dyn ArtifactCleanupLedger>,
    clock: Arc<dyn Clock>,
    /// The ports a definition's closure is held through, and the engines
    /// that check a definition before anything holds it.
    definition_ports: ArtifactReferrerPorts,
    engines: ProcessEngineRegistry,
    /// The core these stores belong to: what resolves the tool catalogue a
    /// workflow is admitted against.
    core: crate::LashCore,
}

impl HostArtifacts {
    pub(crate) fn new(core: &crate::LashCore) -> Self {
        let modules = core.backend().module_artifacts();
        let process_env = Arc::clone(&core.env.core.durability.process_env_store);
        let cleanup = core.backend().artifact_cleanup();
        let clock = Arc::clone(&core.env.core.clock);
        let definition_ports = ArtifactReferrerPorts::new(
            modules,
            Arc::clone(&process_env),
            core.backend().definition_store(),
            core.backend().attachment_referrers(),
            Arc::clone(&cleanup),
            Arc::clone(&clock),
        );
        Self {
            process_env,
            cleanup,
            clock,
            definition_ports,
            engines: core.host_process_engines.clone(),
            core: core.clone(),
        }
    }

    /// Observe the tools the serving build's components serve for `env`'s
    /// effect links. Admits the environment and builds only its named components with
    /// `process: None`. This is an observation of the serving build, not the
    /// catalog at the time the process started.
    pub fn process_catalog(&self, env: &ProcessEnv) -> Result<Arc<ToolCatalog>> {
        self.engines
            .components()
            .admit(env)
            .map_err(|refusal| lash_core::PluginError::ProcessEnvRefused { refusal })?;
        self.core.process_tool_catalog(env)
    }

    /// Admit `env` against this core's process components, publish it and
    /// hold it under `pin`, answering its reference (FIG-6034 §3.3). A
    /// component the env names that is not installed, a config it refuses,
    /// or a released pin is refused; nothing is published for a refusal.
    pub async fn publish_process_env(
        &self,
        pin: &HostArtifactPin,
        env: &ProcessEnv,
    ) -> Result<ProcessEnvRef> {
        let ports = lash_core::ProcessEnvPorts::admitting(
            self.process_env.as_ref(),
            self.engines.components(),
        );
        lash_core::hold_process_env(
            &ports,
            &claim(pin)?,
            &lash_core::ProcessEnvSelection::New(env.clone()),
        )
        .await
        .map_err(|error| match error {
            lash_core::HoldEnvError::Refused(refusal) => {
                lash_core::PluginError::ProcessEnvRefused { refusal }.into()
            }
            lash_core::HoldEnvError::Fault(error) => error.into(),
        })
    }

    /// Publish `draft` and hold it under `pin`: its descriptor and every
    /// artifact of its manifest, which the host published under the same pin
    /// first, and the sibling definitions its engine names (the other
    /// processes of its module). Answers the definition with the signature
    /// its engine derives.
    /// Equal content publishes to the same id and changes nothing; a released
    /// pin, a draft its engine refuses and conflicting bytes under an
    /// existing id are refused.
    pub async fn publish_definition(
        &self,
        pin: &HostArtifactPin,
        draft: &ProcessDefinitionDraft,
    ) -> Result<ProcessDefinition> {
        Ok(self
            .definition_ports
            .publish_definition(&self.engines, &claim(pin)?, draft)
            .await?)
    }

    /// Hold the definition `id` names under `pin`: its descriptor, its
    /// whole manifest and its sibling definitions, checked by its engine
    /// first. This is how a host keeps
    /// a definition available between starts; the id alone holds nothing. A
    /// definition nothing holds any more is `DefinitionMissing`, and a
    /// released pin is refused.
    pub async fn pin_definition(
        &self,
        pin: &HostArtifactPin,
        id: &ProcessDefinitionId,
    ) -> Result<()> {
        let claim = claim(pin)?;
        match self
            .definition_ports
            .acquire_definition(&self.engines, &claim, id)
            .await?
        {
            DefinitionAcquisition::Held(_) => Ok(()),
            DefinitionAcquisition::Ended => Err(lash_core::PluginError::from(
                lash_core::ArtifactStoreError::ReferrerEnded {
                    referrer: claim.referrer().clone(),
                },
            )
            .into()),
        }
    }

    /// Read a definition snapshot. This acquires no lasting pin.
    pub async fn get_definition(
        &self,
        id: &ProcessDefinitionId,
    ) -> Result<Option<ProcessDefinition>> {
        Ok(self
            .definition_ports
            .read_definition(&self.engines, id)
            .await?
            .map(|resolved| resolved.definition))
    }

    pub(crate) async fn resolved_definition(
        &self,
        id: &ProcessDefinitionId,
    ) -> Result<Option<lash_core::ResolvedProcessDefinition>> {
        Ok(self
            .definition_ports
            .read_definition(&self.engines, id)
            .await?)
    }

    /// What a workflow document is edited and admitted against for
    /// processes that run under `environment`: the effects their tool
    /// catalogue offers, in the kernel's type grammar, and the library
    /// functions the workers hold. `None` when no engine of this core
    /// reads workflow documents.
    #[cfg(feature = "codemode")]
    pub async fn workflow_environment(
        &self,
        environment: &ProcessEnv,
    ) -> Result<Option<crate::workflow::WorkflowEnvironment>> {
        if self
            .engines
            .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
            .is_none()
        {
            return Ok(None);
        }
        self.workflow_environment_from_catalog(self.core.process_tool_catalog(environment)?)
            .await
    }

    #[cfg(feature = "codemode")]
    async fn workflow_environment_from_catalog(
        &self,
        tool_catalog: Arc<lash_core::ToolCatalog>,
    ) -> Result<Option<crate::workflow::WorkflowEnvironment>> {
        let Some(provider) = self
            .engines
            .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
        else {
            return Ok(None);
        };
        let request = lash_vm_runtime::WorkflowEnvironmentRequest { tool_catalog };
        let answer = provider
            .environment(lash_core::ProcessDocument::new(request))
            .await?;
        Ok(answer.downcast().ok())
    }

    /// Lower a host-authored TypeScript workflow and admit it against the
    /// same process environment as [`Self::publish_workflow`]. Exported async
    /// declarations and const-bound async literals become document entries;
    /// the printer's exact source is accepted too. The returned reference
    /// selects `main`, with every entry available for editing and publication.
    ///
    /// The host supplies no dialect internals. This reads the tool catalogue
    /// and worker function registry, runs no effect and publishes nothing.
    #[cfg(feature = "typescript")]
    pub async fn import_typescript_workflow(
        &self,
        source: &str,
        environment: &ProcessEnv,
    ) -> Result<crate::workflow::WorkflowImport> {
        self.import_typescript_workflow_with_limits(
            source,
            environment,
            crate::typescript::SourceLimits::standard(),
        )
        .await
    }

    /// Import a TypeScript workflow under the host's validated source policy.
    #[cfg(feature = "typescript")]
    pub async fn import_typescript_workflow_with_limits(
        &self,
        source: &str,
        environment: &ProcessEnv,
        limits: crate::typescript::SourceLimits,
    ) -> Result<crate::workflow::WorkflowImport> {
        use crate::workflow::{
            WorkflowAdmissionRefusal, WorkflowImport, WorkflowImportUnsupported,
        };
        if self
            .engines
            .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
            .is_none()
        {
            return Ok(WorkflowImport::Unsupported(
                WorkflowImportUnsupported::EngineUnavailable,
            ));
        }
        let catalog = self.core.process_tool_catalog(environment)?;
        let boundary = match lash_vm_runtime::HostBoundary::of_catalog(&catalog) {
            Ok(boundary) => boundary,
            Err(error) => {
                return Ok(WorkflowImport::Admission(
                    WorkflowAdmissionRefusal::Boundary {
                        message: error.to_string(),
                    },
                ));
            }
        };
        let Some(checked) = self
            .workflow_environment_from_catalog(Arc::clone(&catalog))
            .await?
        else {
            return Ok(WorkflowImport::Unsupported(
                WorkflowImportUnsupported::EngineUnavailable,
            ));
        };
        // Exact imports already name every dependency by identity. A library
        // may hold several definitions with one label; only authored lowering
        // needs to resolve those labels before admission.
        if crate::typescript::is_generated_workflow(source) {
            let document = match checked
                .worker_service()
                .compile_generated(
                    source,
                    lash_kernel_dialect::SourceFormat::TypeScriptV1,
                    Default::default(),
                )
                .await
            {
                Ok(document) => document,
                Err(error) => return crate::workflow::typescript::source_refusal(error),
            };
            let identity = match document.identity() {
                Ok(identity) => identity,
                Err(error) => {
                    return Ok(WorkflowImport::Refused(vec![
                        lash_kernel_dialect::Diagnostic {
                            code: "GENERATED_SOURCE_INVALID".into(),
                            message: error.to_string(),
                            span: None,
                            kind: lash_kernel_dialect::DiagnosticKind::ProgramDefect,
                            repairs: Vec::new(),
                        },
                    ]));
                }
            };
            let mut annotations = lash_kernel_doc::Annotations::new(identity);
            annotations.dialect = Some("typescript".into());
            return Ok(crate::workflow::typescript::admit(
                document,
                annotations,
                &checked.checker(),
            ));
        }
        let library = match lash_kernel_dialect::NamedLibrary::from_registry(checked.functions()) {
            Ok(library) => library,
            Err(refusal) => {
                return Ok(WorkflowImport::Unsupported(
                    WorkflowImportUnsupported::LibraryUnavailable(refusal),
                ));
            }
        };
        let mut tool_roots = std::collections::BTreeSet::new();
        for tool in &catalog.tools {
            let binding = match lash_vm_runtime::required_tool_executable(&tool.manifest) {
                Ok(binding) => binding,
                Err(error) => {
                    return Ok(WorkflowImport::Admission(
                        WorkflowAdmissionRefusal::Boundary {
                            message: error.to_string(),
                        },
                    ));
                }
            };
            if let Some(root) = binding.module_path.first() {
                tool_roots.insert(lash_kernel_doc::Name::new(root));
            }
        }
        let dialect = lash_kernel_dialect::Environment {
            library: &library,
            effects: checked.effects(),
            tool_roots: &tool_roots,
            controls: &boundary.controls(),
            bindings: &std::collections::BTreeSet::new(),
            functions: &std::collections::BTreeMap::new(),
        };
        crate::workflow::typescript::import(
            checked.worker_service(),
            source,
            &dialect,
            &checked.checker(),
            limits,
        )
        .await
    }

    /// Admit the document `draft` holds as the definition of its entry
    /// `entry`, and hold it under `pin`: the document, and the descriptor
    /// of every entry it lists. The answer names the one `entry` selects;
    /// the others are what a process of it starts by function reference,
    /// so one publication is all a run needs.
    ///
    /// The checker admits the document against `environment`, the
    /// environment a process of the definition would run under. No source
    /// is printed or parsed, and nothing the document states about itself
    /// is taken as true: its view, its types and its effect sets are
    /// derived again.
    ///
    /// Equal content publishes to the same id and changes nothing, so an
    /// unchanged document is the definition it was read from. A refused
    /// document publishes nothing. A released pin is refused. Processes
    /// already started keep the document they were admitted under.
    #[cfg(feature = "codemode")]
    pub async fn publish_workflow(
        &self,
        pin: &HostArtifactPin,
        draft: &crate::workflow::edit::Draft,
        entry: &crate::workflow::document::Name,
        environment: &ProcessEnv,
    ) -> Result<crate::workflow::WorkflowPublish> {
        use crate::workflow::{WorkflowPublication, WorkflowPublish};
        let unsupported = || WorkflowPublish::Unsupported {
            engine_kind: lash_vm_runtime::LASH_VM_ENGINE_KIND.into(),
        };
        let Some(provider) = self
            .engines
            .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
        else {
            return Ok(unsupported());
        };
        let claim = claim(pin)?;
        let request = lash_vm_runtime::WorkflowAdmissionRequest {
            document: draft.document().clone(),
            entry: entry.clone(),
            tool_catalog: self.core.process_tool_catalog(environment)?,
        };
        let outcome = provider
            .admit(&claim, lash_core::ProcessDocument::new(request))
            .await?;
        let Ok(outcome) = outcome.downcast::<lash_vm_runtime::WorkflowAdmissionOutcome>() else {
            return Ok(unsupported());
        };
        let admitted = match outcome {
            lash_vm_runtime::WorkflowAdmissionOutcome::Admitted(admitted) => *admitted,
            lash_vm_runtime::WorkflowAdmissionOutcome::Refused(refusal) => {
                return Ok(WorkflowPublish::Refused(refusal));
            }
        };
        // The document is held under the pin from here; the entry's
        // descriptor joins it, checked by the engine against the stored
        // document, with the descriptor of every other entry.
        let definition = self
            .definition_ports
            .publish_definition(&self.engines, &claim, &admitted.draft)
            .await?;
        Ok(WorkflowPublish::Published(Box::new(WorkflowPublication {
            definition,
            document: admitted.document,
            correspondence: draft.correspondence_since_open().clone(),
        })))
    }

    /// Read the definition `id` names as its workflow: its identity and
    /// signature and its document entered at its entry, or the typed reason
    /// there is none. This acquires no lasting pin.
    #[cfg(feature = "codemode")]
    pub async fn definition_graph(
        &self,
        id: &ProcessDefinitionId,
    ) -> Result<crate::workflow::WorkflowRead> {
        use crate::workflow::{WorkflowRead, WorkflowUnavailable};
        let Some(draft) = self.definition_ports.read_definition_draft(id).await? else {
            return Ok(WorkflowRead::Unavailable(WorkflowUnavailable::Definition {
                definition_id: id.clone(),
            }));
        };
        let read =
            crate::workflow::read(&self.engines, draft.engine_kind(), draft.value().as_json())
                .await?;
        if let WorkflowRead::Inspected(inspection) = &read
            && inspection.definition.id != *id
        {
            return Err(lash_core::PluginError::from(
                lash_core::ProcessDefinitionRefusal::DefinitionIdMismatch {
                    claimed: id.clone(),
                    derived: inspection.definition.id.clone(),
                },
            )
            .into());
        }
        Ok(read)
    }

    /// Read the workflow document `reference` names, with the entry it
    /// selects: what an execution that named it runs, a session cell's main
    /// body as much as a process's definition. An execution's start and a
    /// process's observation snapshot carry the reference. This acquires no
    /// lasting pin.
    #[cfg(feature = "codemode")]
    pub async fn execution_document(
        &self,
        reference: &crate::workflow::WorkflowDocumentRef,
    ) -> Result<crate::workflow::WorkflowDocumentRead> {
        use crate::workflow::{WorkflowDocumentRead, WorkflowUnavailable};
        let Some(provider) = self
            .engines
            .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
        else {
            return Ok(WorkflowDocumentRead::Unsupported);
        };
        Ok(match provider.execution_document(reference).await? {
            lash_core::ProcessExecutionDocumentRead::Read(document) => {
                match document.downcast::<crate::workflow::WorkflowDocument>() {
                    Ok(document) => WorkflowDocumentRead::Read(Box::new(document)),
                    Err(_) => WorkflowDocumentRead::Unsupported,
                }
            }
            lash_core::ProcessExecutionDocumentRead::ArtifactMissing { artifact } => {
                WorkflowDocumentRead::Unavailable(WorkflowUnavailable::Artifact { artifact })
            }
        })
    }

    /// Ends the pin: its `Ended` record, and on the store set's own
    /// database its fence, in one core transaction. The artifact-cleanup
    /// relay then severs every edge the pin holds in every store. The pin can
    /// never publish again.
    pub async fn release(&self, pin: HostArtifactPin) -> Result<()> {
        let cleanup = ArtifactCleanup::ended(ArtifactReferrer::HostPin(pin), Vec::new(), None);
        self.cleanup
            .arm_cleanup(&cleanup, self.clock.timestamp_ms())
            .await?;
        Ok(())
    }
}

fn claim(pin: &HostArtifactPin) -> Result<ReferrerClaim> {
    ReferrerClaim::unguarded(ArtifactReferrer::HostPin(pin.clone()))
        .map_err(|error| lash_core::PluginError::Session(error.to_string()).into())
}

/// Snapshot checks of a retained process definition, without starting a process.
#[derive(Clone)]
pub struct ProcessDefinitions {
    pub(crate) artifacts: HostArtifacts,
}

impl ProcessDefinitions {
    /// Check every supplied argument; `Complete` also requires every declared argument.
    ///
    /// The stored artifact's signature is authoritative. A forged signature claim,
    /// a missing definition, or an engine without argument validation is refused.
    /// This read acquires no lasting artifact pin and writes nothing.
    /// `Ok(Err(refusal))` carries a stable [`lash_core::ArgsMismatch::code`].
    /// Store and transport faults are outer errors; they never judge arguments.
    pub async fn check_args(
        &self,
        definition: &ProcessDefinition,
        args: &serde_json::Map<String, serde_json::Value>,
        mode: lash_core::ArgsMode,
    ) -> Result<std::result::Result<(), lash_core::ArgsMismatch>> {
        let Some(draft) = self
            .artifacts
            .definition_ports
            .read_definition_draft(&definition.id)
            .await?
        else {
            return Ok(Err(lash_core::ArgsMismatch::DefinitionMissing {
                definition_id: definition.id.clone(),
            }));
        };
        let authoritative = match self
            .artifacts
            .engines
            .verify_definition_claim(&draft, definition)
            .await
        {
            Ok(definition) => definition,
            Err(lash_core::ProcessDefinitionError::Refused(source)) => {
                return Ok(Err(lash_core::ArgsMismatch::DefinitionRefused { source }));
            }
            Err(lash_core::ProcessDefinitionError::Fault(error)) => return Err(error.into()),
            Err(error) => return Err(lash_core::PluginError::from(error).into()),
        };
        let engine = self
            .artifacts
            .engines
            .require(draft.engine_kind().as_str())?;
        Ok(engine
            .check_args(&authoritative.signature, args, mode)
            .await)
    }
}
