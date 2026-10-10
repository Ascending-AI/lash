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
    ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ReferrerClaim,
};

use crate::Result;

/// The artifact stores of one core, as a host publishes into them.
#[derive(Clone)]
pub struct HostArtifacts {
    process_env: Arc<dyn ProcessExecutionEnvStore>,
    cleanup: Arc<dyn ArtifactCleanupLedger>,
    clock: Arc<dyn Clock>,
    /// The ports a definition's closure is held through, and the engines
    /// that check a definition before anything holds it.
    definition_ports: ArtifactReferrerPorts,
    engines: ProcessEngineRegistry,
    /// The core these stores belong to: what resolves the tool catalogue a
    /// workflow is admitted against.
    #[cfg(feature = "codemode")]
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
            #[cfg(feature = "codemode")]
            core: core.clone(),
        }
    }

    /// Publish `spec` and hold it under `pin`, answering its reference. A
    /// released pin is refused.
    pub async fn publish_process_env(
        &self,
        pin: &HostArtifactPin,
        spec: &ProcessExecutionEnvSpec,
    ) -> Result<ProcessExecutionEnvRef> {
        Ok(lash_core::runtime::publish_process_execution_env(
            self.process_env.as_ref(),
            &claim(pin)?,
            spec,
        )
        .await?)
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

    /// What a workflow document is edited and admitted against for
    /// processes that run under `environment`: the effects their tool
    /// catalogue offers, in the kernel's type grammar, and the library
    /// functions the workers hold. `None` when no engine of this core
    /// reads workflow documents.
    #[cfg(feature = "codemode")]
    pub async fn workflow_environment(
        &self,
        environment: &ProcessExecutionEnvSpec,
    ) -> Result<Option<crate::workflow::WorkflowEnvironment>> {
        let Some(provider) = self
            .engines
            .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
        else {
            return Ok(None);
        };
        let request = lash_vm_runtime::WorkflowEnvironmentRequest {
            tool_catalog: self.core.process_tool_catalog(environment)?,
        };
        let answer = provider
            .environment(lash_core::ProcessDocument::new(request))
            .await?;
        Ok(answer.downcast().ok())
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
        environment: &ProcessExecutionEnvSpec,
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
    pub async fn check_args(
        &self,
        definition: &ProcessDefinition,
        args: &serde_json::Map<String, serde_json::Value>,
        mode: lash_core::ArgsMode,
    ) -> std::result::Result<(), lash_core::ArgsMismatch> {
        let resolved = self
            .artifacts
            .definition_ports
            .read_definition(&self.artifacts.engines, &definition.id)
            .await
            .map_err(|source| lash_core::ArgsMismatch::DefinitionRead { source })?
            .ok_or_else(|| lash_core::ArgsMismatch::DefinitionMissing {
                definition_id: definition.id.clone(),
            })?;
        self.artifacts
            .engines
            .verify_definition_claim(&resolved.draft, definition)
            .await
            .map_err(|source| lash_core::ArgsMismatch::DefinitionRefused { source })?;
        let engine = self
            .artifacts
            .engines
            .require(resolved.draft.engine_kind().as_str())
            .map_err(|source| lash_core::ArgsMismatch::DefinitionRead { source })?;
        engine
            .check_args(&resolved.definition.signature, args, mode)
            .await
    }
}
