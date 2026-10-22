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

    /// Bind a publisher to this host's registered kernel runtime and accepted namespace.
    /// The supplied context fixes its ownership domain before any publication.
    #[cfg(feature = "codemode")]
    pub fn package_publisher(
        &self,
        context: lash_vocabulary::package::PublicationContext,
        snapshot: lash_vocabulary::package::NamespaceSnapshot,
    ) -> Result<crate::packages::ScopedPackagePublisher> {
        let runtime = self
            .engines
            .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
            .ok_or_else(|| {
                lash_execution_contracts::PluginError::Registration(
                    "package publication requires a registered kernel document provider".into(),
                )
            })?;
        Ok(crate::packages::ScopedPackagePublisher::new(
            context,
            snapshot,
            runtime,
            self.core.backend().package_store(),
        ))
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
            lash_core::HoldEnvError::Admission(error) => {
                lash_execution_contracts::PluginError::Package(Box::new(error)).into()
            }
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

    /// Advertise the chosen process components under this host's trusted
    /// domain, retaining their publications under the caller's existing pin.
    /// This is explicit authoring configuration; it does not expose model tools.
    #[cfg(feature = "codemode")]
    pub async fn process_namespace(
        &self,
        pin: &HostArtifactPin,
        env: &ProcessEnv,
        consumer: lash_vocabulary::package::PublicationContext,
    ) -> Result<lash_core_store::tool_surface::RecordedPackages> {
        use lash_vocabulary::package::*;
        let domain = PublicationDomain(self.core.backend().binding_identity().to_string());
        if consumer.domain != domain || consumer.scope != PublicationScope::Host {
            return Err(lash_execution_contracts::PluginError::Package(Box::new(
                lash_execution_contracts::process::linking::EnginePortError::Refused(
                    lash_execution_contracts::process::linking::AdmissionRefusal::NotOwner {
                        path: lash_vocabulary::names::Vec1::new(consumer.root),
                    },
                ),
            ))
            .into());
        }
        let composition = self
            .engines
            .components()
            .compose(env, None)
            .map_err(
                |refusal| lash_execution_contracts::PluginError::ProcessEnvRefused { refusal },
            )?;
        let configured = &self.core.env.core.control.packages;
        let mut packages = composition.composition.advertise_packages(
            &lash_core::OwnerConfigView::components(Arc::new(env.clone())),
            configured,
            &domain,
            PublicationScope::Host,
        )?;
        packages.consumer = Some(consumer);
        let claim = claim(pin)?;
        let store = self.core.backend().package_store();
        let mut held = std::collections::BTreeSet::new();
        for accepted in &packages.snapshot.advertisements {
            if configured.snapshot.advertisements.contains(accepted) {
                store
                    .acquire_publication(&claim, &accepted.publication.key())
                    .await
                    .map_err(lash_execution_contracts::plugin_error::registry_error)?;
            } else {
                if held.insert(accepted.publication.context.publisher.clone()) {
                    let advertisements: Vec<_> = packages
                        .snapshot
                        .advertisements
                        .iter()
                        .filter(|candidate| {
                            candidate.publication.context.publisher
                                == accepted.publication.context.publisher
                                && !configured.snapshot.advertisements.contains(candidate)
                        })
                        .collect();
                    composition
                        .composition
                        .hold_packages(&claim, &advertisements)
                        .await
                        .map_err(|error| {
                            lash_execution_contracts::PluginError::Package(Box::new(error))
                        })?;
                }
                if let PackageAdvertisement::Enumerated(package) = &accepted.advertisement {
                    store
                        .put_package(
                            &claim,
                            &lash_core_store::packages::PackageRecord {
                                id: package.identity(),
                                package: package.clone(),
                            },
                        )
                        .await
                        .map_err(lash_execution_contracts::plugin_error::registry_error)?;
                }
                store
                    .publish(&claim, &accepted.publication)
                    .await
                    .map_err(lash_execution_contracts::plugin_error::registry_error)?;
            }
        }
        Ok(packages)
    }

    /// Select routes for one retained Link from an explicitly prepared
    /// namespace. Ambiguous providers require a selection keyed by OperationUse.
    #[cfg(feature = "codemode")]
    pub async fn link_process_env(
        &self,
        base: &ProcessEnv,
        start: &lash_vocabulary::names::LinkedStart,
        packages: &lash_core_store::tool_surface::RecordedPackages,
        selected: &std::collections::BTreeMap<
            lash_vocabulary::names::OperationUse,
            (
                lash_vocabulary::providers::ToolProviderHandle,
                lash_vocabulary::ToolId,
            ),
        >,
    ) -> Result<ProcessEnv> {
        let inspection = self
            .engines
            .inspect_link(lash_vm_runtime::LASH_VM_ENGINE_KIND, start)
            .await
            .map_err(|error| lash_execution_contracts::PluginError::Package(Box::new(error)))?;
        let composition = self
            .engines
            .components()
            .compose(base, None)
            .map_err(
                |refusal| lash_execution_contracts::PluginError::ProcessEnvRefused { refusal },
            )?;
        let mut execution = composition
            .composition
            .execution_environment(
                &inspection,
                packages,
                packages.publications(),
                lash_vocabulary::claims::RegistryKind::Process,
                selected,
            )
            .map_err(|error| lash_execution_contracts::PluginError::Package(Box::new(error)))?;
        execution.authored.insert(start.link);
        let mut builder =
            ProcessEnv::builder(base.max_tool_calls()).attachments(Arc::clone(base.attachments()));
        if let Some(model) = base.model() {
            builder = builder.model(model.clone());
        }
        for entry in base.components() {
            builder = builder.entry(entry.clone());
        }
        for (slot, value) in base.capabilities() {
            builder = builder.capability(slot.clone(), value.clone());
        }
        let env = builder
            .packages(packages.clone())
            .execution(execution)
            .build()
            .map_err(
                |refusal| lash_execution_contracts::PluginError::ProcessEnvRefused { refusal },
            )?;
        self.engines
            .check_links(&Arc::new(env.clone()), &inspection)
            .map_err(|error| lash_execution_contracts::PluginError::Package(Box::new(error)))?;
        Ok(env)
    }

    /// Runtime authoring projection of the explicit accepted namespace.
    #[cfg(feature = "codemode")]
    pub async fn workflow_environment(
        &self,
        request: &lash_vm_runtime::WorkflowEnvironmentRequest,
    ) -> Result<Option<crate::workflow::WorkflowEnvironment>> {
        let Some(provider) = self
            .engines
            .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
        else {
            return Ok(None);
        };
        let answer = provider
            .environment(lash_execution_contracts::process::ProcessDocument::new(
                request.clone(),
            ))
            .await?;
        Ok(answer.downcast().ok())
    }

    /// Inspect a captured callable without publishing or taking a lasting hold.
    #[cfg(feature = "codemode")]
    pub async fn inspect_callable(
        &self,
        function: &crate::workflow::document::FunctionValue,
        environment: &lash_vm_runtime::WorkflowEnvironmentRequest,
        origin: &lash_vocabulary::package::CallableOrigin,
    ) -> Result<ProcessDefinition> {
        let request = lash_vm_runtime::process::InlineRequest::Callable {
            link: function.callable.link,
            function: function.clone(),
            origin: origin.clone(),
        }
        .into_request(
            environment.snapshot.clone(),
            environment.context.clone(),
            environment.attachment.clone(),
        )
        .map_err(|error| lash_execution_contracts::PluginError::Package(Box::new(error)))?;
        let inspected = self
            .engines
            .inline_starts(lash_vm_runtime::LASH_VM_ENGINE_KIND)?
            .inspect(&request)
            .await
            .map_err(|error| lash_execution_contracts::PluginError::Package(Box::new(error)))?;
        Ok(ProcessDefinition::new(
            inspected.draft.id(),
            inspected.signature,
        ))
    }

    /// Retain the whole captured callable under the caller's existing pin and
    /// publish its process descriptor. Captures keep their original Links.
    #[cfg(feature = "codemode")]
    pub async fn publish_callable(
        &self,
        pin: &HostArtifactPin,
        function: &crate::workflow::document::FunctionValue,
        environment: &lash_vm_runtime::WorkflowEnvironmentRequest,
        origin: &lash_vocabulary::package::CallableOrigin,
    ) -> Result<ProcessDefinition> {
        let request = lash_vm_runtime::process::InlineRequest::Callable {
            link: function.callable.link,
            function: function.clone(),
            origin: origin.clone(),
        }
        .into_request(
            environment.snapshot.clone(),
            environment.context.clone(),
            environment.attachment.clone(),
        )
        .map_err(|error| lash_execution_contracts::PluginError::Package(Box::new(error)))?;
        let starts = self
            .engines
            .inline_starts(lash_vm_runtime::LASH_VM_ENGINE_KIND)?;
        starts
            .inspect(&request)
            .await
            .map_err(|error| lash_execution_contracts::PluginError::Package(Box::new(error)))?;
        let claim = claim(pin)?;
        for key in &origin.publications {
            self.core
                .backend()
                .package_store()
                .acquire_publication(&claim, key)
                .await
                .map_err(lash_execution_contracts::plugin_error::registry_error)?;
        }
        for link in &origin.authored {
            self.documents()
                .hold(
                    &claim,
                    &lash_execution_contracts::process::linking::ContentRef::Link(*link),
                )
                .await
                .map_err(|error| lash_execution_contracts::PluginError::Package(Box::new(error)))?;
        }
        let realized = self
            .engines
            .inline_starts(lash_vm_runtime::LASH_VM_ENGINE_KIND)?
            .realize(&claim, &request)
            .await
            .map_err(|error| lash_execution_contracts::PluginError::Package(Box::new(error)))?;
        self.publish_definition(pin, &realized.draft).await
    }

    /// Lower an unlinked workflow against explicit visible packages. Publication
    /// fulfils every Import and retains the resulting Link before exposing it.
    #[cfg(feature = "codemode")]
    pub async fn import_workflow(
        &self,
        language: crate::workflow::document::SourceLanguage,
        source: &str,
        environment: &lash_vm_runtime::WorkflowEnvironmentRequest,
        limits: Option<crate::dialect::workflow::WorkflowSourceLimits>,
    ) -> Result<crate::workflow::WorkflowImport> {
        let Some(checked) = self.workflow_environment(environment).await? else {
            return Ok(crate::workflow::WorkflowImport::Unsupported(
                crate::workflow::WorkflowImportUnsupported::EngineUnavailable,
            ));
        };
        crate::workflow::import::import(
            checked.worker_service(),
            language,
            source,
            checked.namespaces(),
            environment.clone(),
            limits,
        )
        .await
    }

    #[cfg(feature = "codemode")]
    pub async fn import_exported_workflow(
        &self,
        source: &str,
        format: crate::dialect::SourceFormat,
        base: &crate::workflow::document::Document,
        environment: &lash_vm_runtime::WorkflowEnvironmentRequest,
    ) -> Result<crate::workflow::WorkflowImport> {
        let Some(checked) = self.workflow_environment(environment).await? else {
            return Ok(crate::workflow::WorkflowImport::Unsupported(
                crate::workflow::WorkflowImportUnsupported::EngineUnavailable,
            ));
        };
        match checked
            .worker_service()
            .read_exported(source, base, format, Default::default())
            .await
        {
            Ok((document, captions)) => Ok(crate::workflow::WorkflowImport::Read {
                document: crate::workflow::document::DocumentBytes::from(document),
                captions,
            }),
            Err(error) => crate::workflow::import::source_refusal(error),
        }
    }

    /// Publish one Main or data-only function Export under its immutable Link.
    /// The explicit attachment includes any permitted deferred paths.
    #[cfg(feature = "codemode")]
    pub async fn publish_workflow(
        &self,
        pin: &HostArtifactPin,
        document: &crate::workflow::document::DocumentBytes,
        selector: &lash_vocabulary::names::StartSelector,
        environment: &lash_vm_runtime::WorkflowEnvironmentRequest,
        source: Option<(&crate::workflow::document::SourceMap, &str)>,
    ) -> Result<lash_vocabulary::names::LinkedStart> {
        let typed = document.decode(lash_vocabulary::KernelDataLimits::standard())?;
        if let Some((map, text)) = source {
            map.validate_identified(&typed, text)?;
        }
        let request = lash_vm_runtime::WorkflowPublishRequest {
            document: document.clone(),
            selector: selector.clone(),
            environment: environment.clone(),
        };
        let claim = claim(pin)?;
        let start = self
            .documents()
            .publish(&claim, &request)
            .await
            .map_err(|error| lash_execution_contracts::PluginError::Package(Box::new(error)))?;
        if let Some((map, text)) = source {
            self.documents()
                .publish_source_map(&claim, &typed, map, text)
                .await
                .map_err(lash_execution_contracts::PluginError::from)?;
        }
        Ok(start)
    }

    /// Start retained linked code using a host-owned pin for the explicit
    /// environment. Normal staging validates and holds the selected reach.
    #[cfg(feature = "codemode")]
    pub async fn start_linked(
        &self,
        pin: &HostArtifactPin,
        request: lash_core::plugin::packages::StartLinked,
        start_key: impl AsRef<[u8]>,
        environment: &lash_vm_runtime::WorkflowEnvironmentRequest,
    ) -> Result<lash_execution_contracts::process::ProcessStartReceipt> {
        let env = self.publish_process_env(pin, &request.env).await?;
        let launch = lash_execution_contracts::process::EngineLaunch::new(env, request.settings)
            .with_initial_resolutions(request.initial_resolutions);
        self.start_function(
            lash_execution_contracts::process::InlineRequest {
                code: lash_execution_contracts::process::InlineCode::Linked {
                    start: request.start,
                },
                snapshot: environment.snapshot.clone(),
                context: environment.context.clone(),
                attachment: environment.attachment.clone(),
            },
            request.args,
            start_key,
            launch,
        )
        .await
    }

    /// Start an admitted linked callable or freshly fulfil an uploaded document.
    /// The request's explicit namespace/attachment is pinned before first claim.
    #[cfg(feature = "codemode")]
    pub async fn start_function(
        &self,
        request: lash_execution_contracts::process::InlineRequest,
        args: serde_json::Map<String, serde_json::Value>,
        start_key: impl AsRef<[u8]>,
        launch: lash_execution_contracts::process::EngineLaunch,
    ) -> Result<lash_execution_contracts::process::ProcessStartReceipt> {
        self.core
            .processes()
            .start(
                lash_execution_contracts::process::ProcessStartRequest::inline(
                    lash_vm_runtime::LASH_VM_ENGINE_KIND,
                    request,
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
        lash_vm_runtime::KernelDocuments::new(
            self.core.backend().module_artifacts(),
            self.core.backend().package_store(),
        )
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

    /// Read display facts from the packages retained by this execution's recorded provenance.
    /// The Link graph selects callees; labels remain independent sidecars.
    #[cfg(all(feature = "codemode", feature = "workflow-view"))]
    pub async fn workflow_view_context(
        &self,
        reference: &lash_vocabulary::LinkedStart,
        publications: &[lash_vocabulary::package::PublicationRecord],
    ) -> Result<crate::workflow::view::ViewContext> {
        use lash_vocabulary::package::PublishedContent;
        let documents = self.documents();
        let store = self.core.backend().package_store();
        let mut content = documents
            .load_linked(reference.link)
            .await
            .map_err(|error| lash_execution_contracts::PluginError::Package(Box::new(error)))?;
        let mut pending = publications
            .iter()
            .map(lash_vocabulary::package::PublicationRecord::key)
            .collect::<Vec<_>>();
        let mut seen = std::collections::BTreeSet::new();
        let mut labels = Vec::new();
        while let Some(key) = pending.pop() {
            if !seen.insert(key.clone()) {
                continue;
            }
            let record = store
                .publication(&key)
                .await
                .map_err(lash_execution_contracts::plugin_error::registry_error)?
                .ok_or_else(|| {
                    lash_execution_contracts::PluginError::Invoke(
                        "the retained display publication is missing".into(),
                    )
                })?;
            pending.extend(record.imported_publications);
            for root in record.code_links {
                if !content.links.contains_key(&root) {
                    let retained = documents.load_linked(root).await.map_err(|error| {
                        lash_execution_contracts::PluginError::Package(Box::new(error))
                    })?;
                    content.documents.extend(retained.documents);
                    content.links.extend(retained.links);
                }
            }
            if let PublishedContent::Package(id) = record.content {
                let package = store
                    .package(&id)
                    .await
                    .map_err(lash_execution_contracts::plugin_error::registry_error)?
                    .ok_or_else(|| {
                        lash_execution_contracts::PluginError::Invoke(
                            "the retained display Package is missing".into(),
                        )
                    })?;
                content.packages.insert(id, package.package);
                labels.extend(
                    store
                        .labels(&id)
                        .await
                        .map_err(lash_execution_contracts::plugin_error::registry_error)?
                        .into_iter()
                        .map(|json| (id, json)),
                );
            }
        }
        let mut context =
            crate::workflow::view::ViewContext::new(reference.clone(), content, Default::default())
                .map_err(|error| {
                    lash_execution_contracts::PluginError::Invoke(error.to_string())
                })?;
        for (id, json) in labels {
            context.add_labels(id, &json).map_err(|error| {
                lash_execution_contracts::PluginError::Invoke(error.to_string())
            })?;
        }
        Ok(context.with_timer_operations(lash_vm_runtime::TIMER_OPERATIONS.iter().cloned()))
    }

    /// Read the workflow document `reference` names, with the entry it
    /// selects: what an execution that named it runs, a session cell's main
    /// body as much as a process's definition. An execution's start and a
    /// process's observation snapshot carry the reference. This acquires no
    /// lasting pin.
    #[cfg(feature = "codemode")]
    pub async fn execution_document(
        &self,
        reference: &lash_vocabulary::names::LinkedStart,
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
                match document.downcast::<crate::workflow::WorkflowDocument>() {
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
