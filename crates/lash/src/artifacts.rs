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
    ModuleArtifactStore, ProcessDefinition, ProcessDefinitionDraft, ProcessDefinitionId,
    ProcessDefinitionStore, ProcessEngineRegistry, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessExecutionEnvStore, ReferrerClaim,
};

use crate::Result;

/// The artifact stores of one core, as a host publishes into them.
#[derive(Clone)]
pub struct HostArtifacts {
    #[cfg_attr(
        not(feature = "rlm"),
        expect(dead_code, reason = "modules are published only with lashlang")
    )]
    modules: Arc<dyn ModuleArtifactStore>,
    process_env: Arc<dyn ProcessExecutionEnvStore>,
    cleanup: Arc<dyn ArtifactCleanupLedger>,
    clock: Arc<dyn Clock>,
    /// The ports a definition's closure is held through, and the engines
    /// that check a definition before anything holds it.
    definition_ports: ArtifactReferrerPorts,
    engines: ProcessEngineRegistry,
}

impl HostArtifacts {
    pub(crate) fn new(
        modules: Arc<dyn ModuleArtifactStore>,
        process_env: Arc<dyn ProcessExecutionEnvStore>,
        definitions: Arc<dyn ProcessDefinitionStore>,
        attachments: Arc<dyn lash_core::AttachmentReferrers>,
        cleanup: Arc<dyn ArtifactCleanupLedger>,
        clock: Arc<dyn Clock>,
        engines: ProcessEngineRegistry,
    ) -> Self {
        let definition_ports = ArtifactReferrerPorts::new(
            Arc::clone(&modules),
            Arc::clone(&process_env),
            definitions,
            attachments,
            Arc::clone(&cleanup),
            Arc::clone(&clock),
        );
        Self {
            modules,
            process_env,
            cleanup,
            clock,
            definition_ports,
            engines,
        }
    }

    /// Publish `artifact` and hold it under `pin`. A released pin is refused.
    #[cfg(feature = "rlm")]
    pub async fn publish_module(
        &self,
        pin: &HostArtifactPin,
        artifact: &lashlang::ModuleArtifact,
    ) -> Result<()> {
        lashlang::LashlangArtifacts::new(Arc::clone(&self.modules))
            .publish_module_artifact(&claim(pin)?, artifact)
            .await
            .map_err(lash_core::PluginError::from)?;
        Ok(())
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
    /// first. Answers the definition with the signature its engine derives.
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

    /// Hold the definition `id` names under `pin`: its descriptor and its
    /// whole manifest, checked by its engine first. This is how a host keeps
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
