//! The immutable definition-artifact store and closure acquisition (ADR 0113
//! §3.6, §4.6).
//!
//! A published definition is its canonical descriptor
//! ([`ProcessDefinitionDraft::to_store_bytes`]) under its
//! [`ProcessDefinitionId`], in the store set's
//! [`ArtifactStoreId::ProcessDefinition`] store. A descriptor row has no owner,
//! name, revision or lifecycle, and its existence roots nothing: not itself,
//! and not the artifacts its manifest names. Like every other artifact it
//! lives exactly as long as some referrer holds an edge to it.
//!
//! A reader holds a definition as a closure: an edge to the descriptor and an
//! edge to every artifact of its manifest, all under the one referrer. The
//! manifest's store-set artifacts (modules, environments) share the
//! descriptor's database, so the store adds those edges and the descriptor's
//! in one transaction that first takes the referrer's lock and checks its
//! fence. An artifact in an engine's own store is another store: it is
//! acquired first, under the claim's guard where the claim has one, and the
//! descriptor's transaction then activates the closure (ADR 0113 §4.6's
//! prepare/acknowledge/activate/sever). A crash between the two leaves the
//! engine edge under the referrer, whose own end severs it; nothing is usable
//! until the descriptor edge commits.
//!
//! An id alone holds nothing. [`ArtifactReferrerPorts::read_definition`] is a
//! snapshot and promises nothing about retention.

use super::definition::{
    ProcessDefinition, ProcessDefinitionDraft, ProcessDefinitionId, ProcessDefinitionStoredError,
};
use super::definition_ref::ProcessDefinitionRefusal;
use super::{ArtifactReferrerPorts, ProcessEngineRegistry, ReferrerAcquisition};
use crate::{
    ArtifactName, ArtifactStoreError, ArtifactStoreId, ReferrerClaim, ResolvedArtifactCleanup,
};

/// The store of immutable process-definition descriptors.
///
/// Every write takes the claim's referrer lock, checks its fence
/// (`ReferrerEnded`) and arms the claim's guard if it has one and no row
/// exists, in the same transaction as the edges it adds (ADR 0113 §2.4). Only
/// the cleanup executor severs edges, through
/// [`Self::end_process_definition_referrer`].
///
/// `manifest` is always the descriptor's store-set share: names under
/// [`ArtifactStoreId::VmModule`] and [`ArtifactStoreId::ProcessEnv`],
/// which live in the descriptor's own database. Any other store in it is
/// refused.
#[async_trait::async_trait]
pub trait ProcessDefinitionStore: Send + Sync {
    /// Store `descriptor` under `id` if absent and verify it equals any
    /// stored descriptor byte for byte (`Immutable` otherwise, even on an
    /// existing id), and add the claim's edge to it and to every `manifest`
    /// artifact, each of which must already be stored (`ArtifactMissing`).
    async fn publish_process_definition(
        &self,
        claim: &ReferrerClaim,
        id: &ProcessDefinitionId,
        descriptor: &[u8],
        manifest: &[ArtifactName],
    ) -> Result<(), ArtifactStoreError>;

    /// Add the claim's edge to the stored descriptor of `id` and to every
    /// `manifest` artifact, with the same lock, fence check and guard arming.
    /// An absent descriptor or artifact is `ArtifactMissing`.
    async fn acquire_process_definition(
        &self,
        claim: &ReferrerClaim,
        id: &ProcessDefinitionId,
        manifest: &[ArtifactName],
    ) -> Result<(), ArtifactStoreError>;

    /// Apply one resolved cleanup to the descriptors in one transaction (ADR
    /// 0113 §2.3): fence the referrer, apply the carries, sever its edges and
    /// reclaim every descriptor no edge holds any more. Replaying an applied
    /// cleanup is a no-op.
    async fn end_process_definition_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError>;

    /// The descriptor stored under `id`, if some referrer holds it.
    async fn get_process_definition(
        &self,
        id: &ProcessDefinitionId,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError>;
}

/// A definition read from its stored descriptor: the descriptor and what its
/// owning engine derives for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedProcessDefinition {
    pub draft: ProcessDefinitionDraft,
    pub definition: ProcessDefinition,
}

impl ResolvedProcessDefinition {
    pub fn id(&self) -> &ProcessDefinitionId {
        &self.definition.id
    }

    /// The engine start payload a start of this definition with `args` runs:
    /// the engine's definition value with the arguments beside it, the same
    /// encoding a start from a definition value makes.
    ///
    /// # Errors
    ///
    /// A refusal for a definition value that is not an object.
    pub fn start_payload(
        &self,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, crate::PluginError> {
        let serde_json::Value::Object(fields) = self.draft.value().as_json() else {
            return Err(definition_refused(format!(
                "definition `{}` has a value that is not an object, so no start payload carries \
                 its arguments",
                self.id()
            )));
        };
        let mut payload = fields.clone();
        payload.insert("args".to_owned(), serde_json::Value::Object(args.clone()));
        Ok(serde_json::Value::Object(payload))
    }
}

/// Whether a closure acquisition holds the definition or met the claim's
/// fence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefinitionAcquisition {
    /// The claim's referrer holds the descriptor and its whole manifest.
    Held(ResolvedProcessDefinition),
    /// The claim's referrer has a fence and holds nothing it did not already
    /// hold.
    Ended,
}

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
    ) -> Result<ProcessDefinition, crate::PluginError> {
        let definition = engines
            .derive_definition(draft)
            .await
            .map_err(refusal_error)?;
        let (store_set, engine_names) = partition_manifest(draft)?;
        if !engine_names.is_empty()
            && self.acquire(engines, claim, &engine_names).await? == ReferrerAcquisition::Ended
        {
            return Err(ArtifactStoreError::ReferrerEnded {
                referrer: claim.referrer(),
            }
            .into());
        }
        self.definitions()
            .publish_process_definition(claim, &definition.id, &draft.to_store_bytes(), &store_set)
            .await?;
        Ok(definition)
    }

    /// Hold the definition `id` names under `claim`: its descriptor and its
    /// whole manifest, checked by its engine before anything is acquired.
    ///
    /// A usable result exists only once the descriptor's own edge commits,
    /// after every other store's share is held. A fence on the claim's
    /// referrer answers [`DefinitionAcquisition::Ended`].
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
    ) -> Result<DefinitionAcquisition, crate::PluginError> {
        let Some(resolved) = self.read_definition(engines, id).await? else {
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
    ) -> Result<Option<ProcessDefinitionDraft>, crate::PluginError> {
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
    ) -> Result<Option<ResolvedProcessDefinition>, crate::PluginError> {
        let Some(draft) = self.read_definition_draft(id).await? else {
            return Ok(None);
        };
        let definition = engines
            .derive_definition(&draft)
            .await
            .map_err(refusal_error)?;
        Ok(Some(ResolvedProcessDefinition { draft, definition }))
    }
}

/// The manifest split by where it is held: the store-set share, which the
/// descriptor's own transaction acquires, and the engine-store share, which
/// is prepared first. A manifest naming another descriptor is refused: no
/// engine resolves a value to one.
fn partition_manifest(
    draft: &ProcessDefinitionDraft,
) -> Result<(Vec<ArtifactName>, Vec<ArtifactName>), crate::PluginError> {
    let mut store_set = Vec::new();
    let mut engine_names = Vec::new();
    for artifact in draft.artifacts() {
        match &artifact.store {
            ArtifactStoreId::VmModule | ArtifactStoreId::ProcessEnv => {
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

fn definition_missing(id: &ProcessDefinitionId) -> crate::PluginError {
    crate::PluginError::Runtime(crate::RuntimeError::new(
        crate::RuntimeErrorCode::DefinitionMissing,
        format!("process definition `{id}` is not stored: no referrer holds it"),
    ))
}

fn definition_refused(message: String) -> crate::PluginError {
    crate::PluginError::Runtime(crate::RuntimeError::new(
        crate::RuntimeErrorCode::DefinitionRefused,
        message,
    ))
}

fn refusal_error(refusal: ProcessDefinitionRefusal) -> crate::PluginError {
    match refusal {
        refusal @ ProcessDefinitionRefusal::WorkerCheckoutTimedOut { .. } => refusal.into(),
        refusal => definition_refused(refusal.to_string()),
    }
}

fn definition_corrupt(
    id: &ProcessDefinitionId,
    error: &ProcessDefinitionStoredError,
) -> crate::PluginError {
    crate::PluginError::Runtime(crate::RuntimeError::new(
        crate::RuntimeErrorCode::RuntimeStoreCorrupt,
        format!("stored process definition `{id}`: {error}"),
    ))
}
