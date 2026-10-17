use super::*;
use crate::*;
use lash_execution_contracts::process::definition_store::definition_refused;
impl ArtifactReferrerPorts {
    /// Publish `draft` under `claim`, answering its definition with the
    /// signature its engine derives.
    ///
    /// The owning engine first checks the manifest and derives the signature
    /// (ADR 0095), so nothing is written for a refused draft. Every manifest
    /// artifact must already be stored: a host publishes its modules under
    /// the same pin first. Publishing equal content again changes nothing;
    /// different bytes under the id are an immutable-content refusal.
    ///
    /// # Errors
    ///
    /// `DefinitionRefused` for a draft the engine refuses, the store's
    /// `ReferrerEnded`, `ArtifactMissing` and `Immutable`, and every store
    /// failure.
    pub async fn publish_definition(
        &self,
        engines: &ProcessEngineRegistry,
        claim: &ReferrerClaim,
        draft: &ProcessDefinitionDraft,
    ) -> Result<ProcessDefinition, lash_execution_contracts::plugin_error::PluginError> {
        let resolution = engines.resolve_definition(draft).await?;
        let ended = || ArtifactStoreError::ReferrerEnded {
            referrer: claim.referrer(),
        };
        if self.publish_descriptor(engines, claim, draft).await? == ReferrerAcquisition::Ended {
            return Err(ended().into());
        }
        Ok(ProcessDefinition::new(draft.id(), resolution.signature))
    }

    /// Store `draft`'s descriptor and hold it and its manifest under
    /// `claim`. The caller has had its engine check the draft.
    async fn publish_descriptor(
        &self,
        engines: &ProcessEngineRegistry,
        claim: &ReferrerClaim,
        draft: &ProcessDefinitionDraft,
    ) -> Result<ReferrerAcquisition, lash_execution_contracts::plugin_error::PluginError> {
        let (store_set, engine_names) = partition_manifest(draft)?;
        if !engine_names.is_empty()
            && self.acquire(engines, claim, &engine_names).await? == ReferrerAcquisition::Ended
        {
            return Ok(ReferrerAcquisition::Ended);
        }
        match self
            .definitions()
            .publish_process_definition(claim, &draft.id(), &draft.to_store_bytes(), &store_set)
            .await
        {
            Ok(()) => Ok(ReferrerAcquisition::Held),
            Err(ArtifactStoreError::ReferrerEnded { referrer }) if referrer == claim.referrer() => {
                Ok(ReferrerAcquisition::Ended)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Hold the definition `id` names under `claim`: its descriptor and its
    /// whole manifest, checked by its engine before anything is acquired.
    ///
    /// A usable result exists only once the descriptor's own edge commits,
    /// after every other store's share is held. A fence on
    /// the claim's referrer answers [`DefinitionAcquisition::Ended`].
    ///
    /// # Errors
    ///
    /// `DefinitionMissing` when nothing holds the descriptor,
    /// `RuntimeStoreCorrupt` for stored bytes that are not its descriptor,
    /// `DefinitionRefused` for one its engine refuses, and every store
    /// failure.
    pub async fn acquire_definition(
        &self,
        engines: &ProcessEngineRegistry,
        claim: &ReferrerClaim,
        id: &ProcessDefinitionId,
    ) -> Result<DefinitionAcquisition, lash_execution_contracts::plugin_error::PluginError> {
        let Some(resolved) = self.resolve_stored_definition(engines, id).await? else {
            return Err(definition_missing(id));
        };
        let (store_set, engine_names) = partition_manifest(&resolved.draft)?;
        if !engine_names.is_empty()
            && self.acquire(engines, claim, &engine_names).await? == ReferrerAcquisition::Ended
        {
            return Ok(DefinitionAcquisition::Ended);
        }
        match self
            .definitions()
            .acquire_process_definition(claim, id, &store_set)
            .await
        {
            Ok(()) => Ok(DefinitionAcquisition::Held(resolved)),
            Err(ArtifactStoreError::ReferrerEnded { referrer }) if referrer == claim.referrer() => {
                Ok(DefinitionAcquisition::Ended)
            }
            // Reclaimed after the read: the last holder let go first.
            Err(ArtifactStoreError::ArtifactMissing { .. }) => Err(definition_missing(id)),
            Err(error) => Err(error.into()),
        }
    }

    /// The descriptor stored under `id`, checked against the id and asked of
    /// no engine. A snapshot that holds nothing.
    ///
    /// # Errors
    ///
    /// The store's failure, and a corrupt descriptor.
    pub async fn read_definition_draft(
        &self,
        id: &ProcessDefinitionId,
    ) -> Result<Option<ProcessDefinitionDraft>, lash_execution_contracts::plugin_error::PluginError>
    {
        let Some(bytes) = self.definitions().get_process_definition(id).await? else {
            return Ok(None);
        };
        ProcessDefinitionDraft::from_store_bytes(id, &bytes)
            .map(Some)
            .map_err(|error| definition_corrupt(id, &error))
    }

    /// The definition stored under `id`, as its engine derives it: a
    /// snapshot that holds nothing and promises nothing about retention.
    ///
    /// # Errors
    ///
    /// `RuntimeStoreCorrupt` for stored bytes that are not the descriptor of
    /// `id`, `DefinitionRefused` for one its engine refuses, and every store
    /// failure.
    pub async fn read_definition(
        &self,
        engines: &ProcessEngineRegistry,
        id: &ProcessDefinitionId,
    ) -> Result<
        Option<ResolvedProcessDefinition>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.resolve_stored_definition(engines, id).await
    }

    /// The definition stored under `id`, checked by its engine.
    async fn resolve_stored_definition(
        &self,
        engines: &ProcessEngineRegistry,
        id: &ProcessDefinitionId,
    ) -> Result<
        Option<ResolvedProcessDefinition>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let Some(draft) = self.read_definition_draft(id).await? else {
            return Ok(None);
        };
        let resolution = engines.resolve_definition(&draft).await?;
        let definition = ProcessDefinition::new(draft.id(), resolution.signature);
        Ok(Some(ResolvedProcessDefinition { draft, definition }))
    }
}

/// The manifest split by where it is held: the store-set share, which the
/// descriptor's own transaction acquires, and the engine-store share, which
/// is prepared first. A manifest naming another descriptor is refused: no
/// engine resolves a value to one.
fn partition_manifest(
    draft: &ProcessDefinitionDraft,
) -> Result<
    (Vec<ArtifactName>, Vec<ArtifactName>),
    lash_execution_contracts::plugin_error::PluginError,
> {
    let mut store_set = Vec::new();
    let mut engine_names = Vec::new();
    for artifact in draft.artifacts() {
        match &artifact.store {
            ArtifactStoreId::KernelDocument | ArtifactStoreId::ProcessEnv => {
                store_set.push(artifact.clone());
            }
            ArtifactStoreId::Engine(_) => engine_names.push(artifact.clone()),
            ArtifactStoreId::ProcessDefinition
            | ArtifactStoreId::ToolMaterial
            | ArtifactStoreId::TurnPrelude => {
                return Err(definition_refused(format!(
                    "definition `{}` names `{}` from store {:?} in its manifest",
                    draft.id(),
                    artifact.artifact_ref,
                    artifact.store
                )));
            }
        }
    }
    Ok((store_set, engine_names))
}

fn definition_missing(
    id: &ProcessDefinitionId,
) -> lash_execution_contracts::plugin_error::PluginError {
    lash_execution_contracts::plugin_error::PluginError::Runtime(
        lash_core_store::runtime_error::RuntimeError::new(
            lash_core_store::runtime_error::RuntimeErrorCode::DefinitionMissing,
            format!("process definition `{id}` is not stored: no referrer holds it"),
        ),
    )
}

fn definition_corrupt(
    id: &ProcessDefinitionId,
    error: &ProcessDefinitionStoredError,
) -> lash_execution_contracts::plugin_error::PluginError {
    lash_execution_contracts::plugin_error::PluginError::Runtime(
        lash_core_store::runtime_error::RuntimeError::new(
            lash_core_store::runtime_error::RuntimeErrorCode::RuntimeStoreCorrupt,
            format!("stored process definition `{id}`: {error}"),
        ),
    )
}
