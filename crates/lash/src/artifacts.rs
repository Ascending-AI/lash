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

use lash_core::ArtifactCleanup;
use lash_core::ArtifactReferrer;
use lash_core::ArtifactReferrerPorts;
use lash_core::Clock;
pub use lash_core::HostArtifactPin;
use lash_core::ProcessDefinitionId;
use lash_core::ProcessEngineRegistry;
use lash_core::ProcessEnv;
use lash_core::ProcessEnvRef;
use lash_core::ReferrerClaim;
use lash_core::ToolCatalog;
use lash_core::store::ArtifactCleanupLedger;
use lash_execution_contracts::process::DefinitionAcquisition;
use lash_execution_contracts::process::ProcessDefinition;
use lash_execution_contracts::process::ProcessDefinitionDraft;
use lash_execution_contracts::process::ProcessEnvStore;

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
        Self::of_code(&error.code, error.ended_referrer())
    }

    /// The judgment a refusal carried through the effect controller states:
    /// a start's realization refuses a definition it cannot hold this way.
    pub(crate) fn of_controller(error: &lash_core::RuntimeEffectControllerError) -> Option<Self> {
        Self::of_code(&error.code, error.ended_referrer())
    }

    fn of_code(
        code: &lash_core::RuntimeErrorCode,
        ended_referrer: Option<&ArtifactReferrer>,
    ) -> Option<Self> {
        use lash_core::RuntimeErrorCode;
        match code {
            RuntimeErrorCode::ArtifactReferrerEnded
                if matches!(ended_referrer, Some(ArtifactReferrer::HostPin(_))) =>
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
        self.engines.components().admit(env).map_err(|refusal| {
            lash_execution_contracts::plugin_error::PluginError::ProcessEnvRefused { refusal }
        })?;
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
                lash_execution_contracts::plugin_error::PluginError::ProcessEnvRefused { refusal }
                    .into()
            }
            lash_core::HoldEnvError::Fault(error) => error.into(),
        })
    }

    /// Publish `draft` and hold it under `pin`: its descriptor and every
    /// artifact of its manifest, which the host published under the same pin
    /// first. Answers the definition with the signature
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
    /// whole manifest, checked by its engine
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
            DefinitionAcquisition::Ended => {
                Err(lash_execution_contracts::plugin_error::PluginError::from(
                    lash_execution_contracts::module_artifacts::ArtifactStoreError::ReferrerEnded {
                        referrer: claim.referrer().clone(),
                    },
                )
                .into())
            }
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
    ) -> Result<Option<lash_execution_contracts::process::ResolvedProcessDefinition>> {
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
            .environment(lash_execution_contracts::process::ProcessDocument::new(
                request,
            ))
            .await?;
        Ok(answer.downcast().ok())
    }

    /// Lower a host-authored workflow module in `language` and admit it
    /// against the same process environment as [`Self::publish_workflow`].
    /// Every dialect lowers a module under one workflow contract
    /// ([`crate::dialect::workflow`]): each top-level async function, by
    /// declaration or by `const` (TypeScript) or `async def` (Python), is an
    /// entry; an entry captures nothing of the module, its signature is what
    /// its annotations mean in its dialect, and the contract's refusals
    /// carry the same codes in every dialect. Source printed from a document
    /// is read back with [`Self::import_exported_workflow`] instead.
    ///
    /// The answer carries the document, its source map, the source and the
    /// captions it spelled, for [`Self::publish_workflow`] to keep together; the opaque bytes retain
    /// every entry, available through an explicit typed read for editing.
    /// Select an entry when publishing. Without `limits` the dialect's standard source policy
    /// applies.
    ///
    /// The host supplies no dialect internals and links no parser: the
    /// worker lowers the source. This reads the tool catalogue and worker
    /// function registry, runs no effect and publishes nothing.
    #[cfg(feature = "codemode")]
    pub async fn import_workflow(
        &self,
        language: crate::workflow::document::SourceLanguage,
        source: &str,
        environment: &ProcessEnv,
        limits: Option<crate::dialect::workflow::WorkflowSourceLimits>,
    ) -> Result<crate::workflow::WorkflowImport> {
        use crate::workflow::{
            WorkflowAdmissionRefusal, WorkflowImport, WorkflowImportUnsupported,
        };
        let (catalog, boundary, checked) = match self.import_context(environment).await? {
            Ok(context) => context,
            Err(refused) => return Ok(refused),
        };
        let library = match lash_kernel_dialect::NamedLibrary::from_registry(checked.functions()) {
            Ok(library) => library,
            Err(refusal) => {
                return Ok(WorkflowImport::Unsupported(
                    WorkflowImportUnsupported::LibraryUnavailable(refusal),
                ));
            }
        };
        let mut tool_roots = std::collections::BTreeSet::new();
        for tool in catalog.entries() {
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
        };
        crate::workflow::import::import(
            checked.worker_service(),
            language,
            source,
            &dialect,
            &checked.checker(),
            limits,
        )
        .await
    }

    /// Read source exported from `base` (a host's edit of it, or the
    /// printed text unchanged) and admit the result against the same process
    /// environment as [`Self::publish_workflow`]. The source carries only
    /// code; the manifest, entries, private bindings, helper identities and
    /// name renames are the base document's, so an unchanged source reads
    /// back as `base` itself, with its identity. A construct the reader has
    /// no exact inverse for is a typed refusal naming it. The admitted
    /// document is answered as [`WorkflowImport::Read`](crate::workflow::WorkflowImport::Read).
    ///
    /// This reads the tool catalogue, runs no effect and publishes nothing.
    #[cfg(feature = "codemode")]
    pub async fn import_exported_workflow(
        &self,
        source: &str,
        format: crate::dialect::SourceFormat,
        base: &crate::workflow::document::Document,
        environment: &ProcessEnv,
    ) -> Result<crate::workflow::WorkflowImport> {
        let (_, _, checked) = match self.import_context(environment).await? {
            Ok(context) => context,
            Err(refused) => return Ok(refused),
        };
        let (document, captions) = match checked
            .worker_service()
            .read_exported(source, base, format, Default::default())
            .await
        {
            Ok(read) => read,
            Err(error) => return crate::workflow::import::source_refusal(error),
        };
        Ok(crate::workflow::import::admit_read(
            document,
            captions,
            &checked.checker(),
        ))
    }

    /// What importing a workflow source admits against: the process tool
    /// catalogue, its host boundary and the checked environment, or the
    /// answer that refuses the import before any source is read.
    #[cfg(feature = "codemode")]
    async fn import_context(
        &self,
        environment: &ProcessEnv,
    ) -> Result<
        std::result::Result<
            (
                Arc<lash_core::ToolCatalog>,
                lash_vm_runtime::HostBoundary,
                crate::workflow::WorkflowEnvironment,
            ),
            crate::workflow::WorkflowImport,
        >,
    > {
        use crate::workflow::{
            WorkflowAdmissionRefusal, WorkflowImport, WorkflowImportUnsupported,
        };
        if self
            .engines
            .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
            .is_none()
        {
            return Ok(Err(WorkflowImport::Unsupported(
                WorkflowImportUnsupported::EngineUnavailable,
            )));
        }
        let catalog = self.core.process_tool_catalog(environment)?;
        let boundary = match lash_vm_runtime::HostBoundary::of_catalog(&catalog) {
            Ok(boundary) => boundary,
            Err(error) => {
                return Ok(Err(WorkflowImport::Admission(
                    WorkflowAdmissionRefusal::Boundary {
                        message: error.to_string(),
                    },
                )));
            }
        };
        let Some(checked) = self
            .workflow_environment_from_catalog(Arc::clone(&catalog))
            .await?
        else {
            return Ok(Err(WorkflowImport::Unsupported(
                WorkflowImportUnsupported::EngineUnavailable,
            )));
        };
        Ok(Ok((catalog, boundary, checked)))
    }

    /// Validate opaque `document` bytes, admit their entry `entry`, and hold them
    /// under `pin`: the document, the declaration of every library function
    /// it lists, and the descriptor of the selected entry. Other functions
    /// cross inline when passed to tools; they have no stored descriptors.
    ///
    /// The checker admits the document against `environment`, the
    /// environment a process of the definition would run under. No source
    /// is printed or parsed, no editor is needed, and nothing the document
    /// states about itself is taken as true: its view, its types and its
    /// effect sets are derived again.
    ///
    /// With `source`, the host's source map of the document and the source
    /// it maps are checked against the document and held under `pin` beside
    /// it; the answer carries the map's id, which the host keeps beside the
    /// definition id and reads back with [`Self::read_source_map`]. Lash keeps
    /// no association of its own between a definition and a map.
    ///
    /// Equal content publishes to the same id and changes nothing, so an
    /// unchanged document is the definition it was read from. A refused
    /// document or map publishes nothing. A released pin is refused.
    /// Processes already started keep the document they were admitted
    /// under.
    #[cfg(feature = "codemode")]
    pub async fn publish_workflow(
        &self,
        pin: &HostArtifactPin,
        document: &crate::workflow::document::DocumentBytes,
        entry: &crate::workflow::document::Name,
        environment: &ProcessEnv,
        source: Option<(&crate::workflow::document::SourceMap, &str)>,
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
        let typed = match document.decode(lash_kernel_doc::KernelDataLimits::standard()) {
            Ok(document) => document,
            Err(refusal) => {
                return Ok(WorkflowPublish::Refused(
                    crate::workflow::WorkflowAdmissionRefusal::Bytes(refusal),
                ));
            }
        };
        if let Some((map, text)) = source
            && let Err(refusal) = map.validate_identified(&typed, text)
        {
            return Ok(WorkflowPublish::SourceMapRefused(refusal));
        }
        let claim = claim(pin)?;
        let request = lash_vm_runtime::WorkflowAdmissionRequest {
            document: typed,
            entry: entry.clone(),
            tool_catalog: self.core.process_tool_catalog(environment)?,
        };
        let outcome = provider
            .admit(
                &claim,
                lash_execution_contracts::process::ProcessDocument::new(request),
            )
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
        // The document and its declarations are held under the pin from
        // here; the map and its source join them before the descriptor
        // does, so no answer names a map whose source is not held.
        let source_map = match source {
            Some((map, text)) => Some(
                self.documents()
                    .publish_source_map(&claim, admitted.document.document(), map, text)
                    .await
                    .map_err(lash_execution_contracts::plugin_error::PluginError::from)?,
            ),
            None => None,
        };
        // The entry's descriptor joins them, checked by the engine against
        // the stored document.
        let definition = self
            .definition_ports
            .publish_definition(&self.engines, &claim, &admitted.draft)
            .await?;
        Ok(WorkflowPublish::Published(Box::new(WorkflowPublication {
            definition,
            reference: admitted.document.reference().clone(),
            source_map,
        })))
    }

    /// Start a held function value with explicit arguments, idempotency key
    /// and launch. Nothing is published until the start is realized. An
    /// exact retry returns the same process; changed code, captures,
    /// arguments or launch under the key refuse with `StartKeyConflict`.
    #[cfg(feature = "codemode")]
    pub async fn start_function(
        &self,
        value: &crate::workflow::document::FunctionValue,
        args: serde_json::Map<String, serde_json::Value>,
        start_key: impl AsRef<[u8]>,
        launch: lash_execution_contracts::process::EngineLaunch,
    ) -> Result<lash_execution_contracts::process::ProcessStartReceipt> {
        let payload = value.transport_json().map_err(|error| {
            lash_execution_contracts::plugin_error::PluginError::Invoke(error.to_string())
        })?;
        self.core
            .processes()
            .start(
                lash_execution_contracts::process::ProcessStartRequest::inline(
                    lash_vm_runtime::LASH_VM_ENGINE_KIND,
                    payload,
                    args,
                    launch,
                    lash_execution_contracts::process::ProcessOriginator::host(),
                    lash_execution_contracts::process::LifetimeDecision::Detached,
                )
                .with_host_start_key(start_key),
                self.core.effect_host(),
            )
            .await
    }

    /// The source map a publication stored under `id`, with the source it
    /// maps, both checked against each other and, while it is retained,
    /// against the document the map names. This acquires no lasting pin.
    #[cfg(feature = "codemode")]
    pub async fn read_source_map(
        &self,
        id: &crate::workflow::document::SourceMapId,
    ) -> Result<crate::workflow::SourceMapRead> {
        use crate::workflow::{SourceMapRead, WorkflowUnavailable};
        Ok(
            match self
                .documents()
                .source_map(id)
                .await
                .map_err(lash_execution_contracts::plugin_error::PluginError::from)?
            {
                lash_vm_runtime::StoredSourceMap::Read { map, source } => {
                    SourceMapRead::Read { map, source }
                }
                lash_vm_runtime::StoredSourceMap::Unretained(unretained) => {
                    SourceMapRead::Unavailable(WorkflowUnavailable::of(unretained))
                }
            },
        )
    }

    /// The store of kernel documents behind this core's module port.
    #[cfg(feature = "codemode")]
    fn documents(&self) -> lash_vm_runtime::KernelDocuments {
        lash_vm_runtime::KernelDocuments::new(self.core.backend().module_artifacts())
    }

    /// Read the definition `id` names as its workflow: its identity and
    /// signature and its document entered at its entry, or the typed reason
    /// there is none. This acquires no lasting pin.
    ///
    /// The document is the whole cell, including `main` and other entries.
    /// To publish only the selected function (with `workflow-authoring`),
    /// call `workflow::edit::extract_entry` with its entry name and checker environment;
    /// use the returned admitted `Extraction::document`
    /// against the workflow environment. This keeps that entry and every
    /// declared function it reaches, and derives their manifest during
    /// application.
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
            return Err(lash_execution_contracts::plugin_error::PluginError::from(
                lash_execution_contracts::process::ProcessDefinitionRefusal::DefinitionIdMismatch {
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
            lash_execution_contracts::process::ProcessExecutionDocumentRead::Read(document) => {
                match document.downcast::<crate::workflow::ExecutionDocument>() {
                    Ok(document) => WorkflowDocumentRead::Read(Box::new(document)),
                    Err(_) => WorkflowDocumentRead::Unsupported,
                }
            }
            lash_execution_contracts::process::ProcessExecutionDocumentRead::ArtifactMissing {
                artifact,
            } => WorkflowDocumentRead::Unavailable(WorkflowUnavailable::Artifact { artifact }),
            lash_execution_contracts::process::ProcessExecutionDocumentRead::ArtifactCorrupt {
                artifact,
                message,
            } => WorkflowDocumentRead::Unavailable(WorkflowUnavailable::Corrupt {
                artifact,
                message,
            }),
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
    ReferrerClaim::unguarded(ArtifactReferrer::HostPin(pin.clone())).map_err(|error| {
        lash_execution_contracts::plugin_error::PluginError::Session(error.to_string()).into()
    })
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
    /// `Ok(Err(refusal))` carries a stable [`lash_execution_contracts::process::ArgsMismatch::code`].
    /// Store and transport faults are outer errors; they never judge arguments.
    pub async fn check_args(
        &self,
        definition: &ProcessDefinition,
        args: &serde_json::Map<String, serde_json::Value>,
        mode: lash_execution_contracts::process::ArgsMode,
    ) -> Result<std::result::Result<(), lash_execution_contracts::process::ArgsMismatch>> {
        let Some(draft) = self
            .artifacts
            .definition_ports
            .read_definition_draft(&definition.id)
            .await?
        else {
            return Ok(Err(
                lash_execution_contracts::process::ArgsMismatch::DefinitionMissing {
                    definition_id: definition.id.clone(),
                },
            ));
        };
        let authoritative = match self
            .artifacts
            .engines
            .verify_definition_claim(&draft, definition)
            .await
        {
            Ok(definition) => definition,
            Err(lash_execution_contracts::process::ProcessDefinitionError::Refused(source)) => {
                return Ok(Err(
                    lash_execution_contracts::process::ArgsMismatch::DefinitionRefused { source },
                ));
            }
            Err(lash_execution_contracts::process::ProcessDefinitionError::Fault(error)) => {
                return Err(error.into());
            }
            Err(error) => {
                return Err(
                    lash_execution_contracts::plugin_error::PluginError::from(error).into(),
                );
            }
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
