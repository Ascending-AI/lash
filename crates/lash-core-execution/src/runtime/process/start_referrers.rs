//! Artifact acquisition and cleanup ownership at a process start.
use super::*;

impl ArtifactReferrerPorts {
    pub fn new(
        modules: Arc<dyn ModuleArtifactStore>,
        env: Arc<dyn ProcessEnvStore>,
        definitions: Arc<dyn crate::ProcessDefinitionStore>,
        attachments: Arc<dyn crate::AttachmentReferrers>,
        cleanup: Arc<dyn ArtifactCleanupLedger>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            modules,
            env,
            definitions,
            attachments,
            cleanup,
            clock,
        }
    }

    /// The ports of `backend`'s store set.
    pub fn of_backend(backend: &lash_execution_contracts::backend::Backend) -> Self {
        Self::new(
            backend.module_artifacts(),
            backend.process_env_store(),
            backend.definition_store(),
            backend.attachment_referrers(),
            backend.artifact_cleanup(),
            backend.clock(),
        )
    }

    pub fn modules(&self) -> &Arc<dyn ModuleArtifactStore> {
        &self.modules
    }

    pub fn env(&self) -> &Arc<dyn ProcessEnvStore> {
        &self.env
    }

    pub fn definitions(&self) -> &Arc<dyn crate::ProcessDefinitionStore> {
        &self.definitions
    }

    pub fn attachments(&self) -> &Arc<dyn crate::AttachmentReferrers> {
        &self.attachments
    }

    pub fn cleanup(&self) -> &Arc<dyn ArtifactCleanupLedger> {
        &self.cleanup
    }

    /// Add the claim's edge to every name, each in the store that holds it.
    ///
    /// Store-set names go first: their own transaction arms the claim's
    /// guard. Before the first engine-store name, the guard is armed in the
    /// ledger, since an engine store cannot write it (ADR 0113 §2.2). A fence
    /// on the claim's referrer stops the acquisition and answers
    /// [`ReferrerAcquisition::Ended`].
    ///
    /// # Errors
    ///
    /// A name under an engine `engines` does not have, `ArtifactMissing`, and
    /// every store failure.
    pub async fn acquire(
        &self,
        engines: &ProcessEngineRegistry,
        claim: &ReferrerClaim,
        names: &[ArtifactName],
    ) -> Result<ReferrerAcquisition, lash_execution_contracts::plugin_error::PluginError> {
        let (engine_names, store_names): (Vec<&ArtifactName>, Vec<&ArtifactName>) = names
            .iter()
            .partition(|name| matches!(name.store, ArtifactStoreId::Engine(_)));
        for name in store_names {
            let acquired = match &name.store {
                ArtifactStoreId::KernelDocument => {
                    self.modules
                        .acquire_module_artifact(claim, &name.artifact_ref)
                        .await
                }
                ArtifactStoreId::ProcessEnv => {
                    self.env
                        .acquire_process_env(claim, &ProcessEnvRef::new(name.artifact_ref.clone()))
                        .await
                }
                // A descriptor name alone: its manifest is held through
                // `acquire_definition`, which reads it.
                ArtifactStoreId::ProcessDefinition => {
                    let id =
                        crate::ProcessDefinitionId::parse(&name.artifact_ref).map_err(|error| {
                            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                                "artifact `{}` is not a process definition id: {error}",
                                name.artifact_ref
                            ))
                        })?;
                    self.definitions
                        .acquire_process_definition(claim, &id, &[])
                        .await
                }
                ArtifactStoreId::Engine(_) => continue,
                // Tool material is held by Run-segment and source leases only.
                ArtifactStoreId::ToolMaterial => {
                    return Err(
                        lash_execution_contracts::plugin_error::PluginError::Session(format!(
                            "a start cannot hold retained tool material `{}`",
                            name.artifact_ref
                        )),
                    );
                }
                // A recorded prelude is held by its turn's journal only.
                ArtifactStoreId::TurnPrelude => {
                    return Err(
                        lash_execution_contracts::plugin_error::PluginError::Session(format!(
                            "a start cannot hold the recorded turn prelude `{}`",
                            name.artifact_ref
                        )),
                    );
                }
            };
            if let Some(ended) = held_or_ended(
                claim,
                acquired.map_err(lash_execution_contracts::plugin_error::PluginError::from),
            )? {
                return Ok(ended);
            }
        }
        if engine_names.is_empty() {
            return Ok(ReferrerAcquisition::Held);
        }
        if let Some(guard) = claim.guard_cleanup() {
            self.arm(&guard).await?;
        }
        for name in engine_names {
            let ArtifactStoreId::Engine(kind) = &name.store else {
                continue;
            };
            let acquired = engines
                .require(kind)?
                .acquire_engine_artifact(claim, &name.artifact_ref)
                .await;
            if let Some(ended) = held_or_ended(claim, acquired)? {
                return Ok(ended);
            }
        }
        Ok(ReferrerAcquisition::Held)
    }

    /// Upsert `cleanup` under ADR 0113 §2.4's rule, due now.
    ///
    /// # Errors
    ///
    /// The ledger's failure.
    pub async fn arm(
        &self,
        cleanup: &ArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        self.cleanup
            .arm_cleanup(cleanup, self.clock.timestamp_ms())
            .await
            .map(|_| ())
            .map_err(lash_execution_contracts::plugin_error::PluginError::from)
    }

    /// End `referrer` now: its `Ended` record with no carries and no gate.
    ///
    /// # Errors
    ///
    /// The ledger's failure.
    pub async fn end(
        &self,
        referrer: ArtifactReferrer,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        self.arm(&ArtifactCleanup::ended(referrer, Vec::new(), None))
            .await
    }

    /// Make `referrer`'s cleanup due now, after the fact that ends it
    /// committed elsewhere. A nudge only shortens a guard's wait and decides
    /// nothing, so a failed one is logged and dropped: the guard still
    /// resolves on its own cadence.
    pub async fn nudge(&self, referrer: &ArtifactReferrer) {
        if let Err(error) = self
            .cleanup
            .nudge(referrer, self.clock.timestamp_ms())
            .await
        {
            tracing::warn!(%referrer, %error, "artifact cleanup nudge failed");
        }
    }
}

/// `Some(Ended)` when `acquired` failed on the claim's own fence, `None` when
/// it held, and the error otherwise.
pub(super) fn held_or_ended(
    claim: &ReferrerClaim,
    acquired: Result<(), lash_execution_contracts::plugin_error::PluginError>,
) -> Result<Option<ReferrerAcquisition>, lash_execution_contracts::plugin_error::PluginError> {
    match acquired {
        Ok(()) => Ok(None),
        Err(error) if artifact_referrer_ended(&error) == Some(&claim.referrer()) => {
            Ok(Some(ReferrerAcquisition::Ended))
        }
        Err(error) => Err(error),
    }
}
