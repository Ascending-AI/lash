//! Host-held artifacts (ADR 0113 §2.6, §3.5).
//!
//! A host keeps artifact bytes alive only through a [`HostArtifactPin`] it
//! minted: every publish adds the pin's edge, and releasing the pin ends it
//! for good. The pin is opaque; hosts never name a referrer of any other kind,
//! and a released pin can never publish again — a host that wants to publish
//! again mints a fresh one.

use std::sync::Arc;

pub use lash_core::HostArtifactPin;
use lash_core::store::ArtifactCleanupLedger;
use lash_core::{
    ArtifactCleanup, ArtifactReferrer, Clock, ModuleArtifactStore, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ReferrerClaim,
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
}

impl HostArtifacts {
    pub(crate) fn new(
        modules: Arc<dyn ModuleArtifactStore>,
        process_env: Arc<dyn ProcessExecutionEnvStore>,
        cleanup: Arc<dyn ArtifactCleanupLedger>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            modules,
            process_env,
            cleanup,
            clock,
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
