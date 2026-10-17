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
//! A manifest's kernel document and helper declarations are checkpoint
//! graph nodes (FIG-6469): each artifact's row roots its Document or
//! HelperDefinition node, so the referrer's edge to the row retains the
//! node directly. The node is stored once, shared with every definition and
//! checkpoint that names it, and goes with its last root.
//!
//! An id alone holds nothing. [`ArtifactReferrerPorts::read_definition`] is a
//! snapshot and promises nothing about retention.

use super::definition::ProcessDefinition;
use super::definition::ProcessDefinitionDraft;
use super::definition::ProcessDefinitionId;
use crate::ArtifactName;
use crate::ArtifactStoreError;
use crate::ReferrerClaim;
use crate::ResolvedArtifactCleanup;

/// The store of immutable process-definition descriptors.
///
/// Every write takes the claim's referrer lock, checks its fence
/// (`ReferrerEnded`) and arms the claim's guard if it has one and no row
/// exists, in the same transaction as the edges it adds (ADR 0113 §2.4). Only
/// the cleanup executor severs edges, through
/// [`Self::end_process_definition_referrer`].
///
/// `manifest` is always the descriptor's store-set share: names under
/// [`ArtifactStoreId::KernelDocument`] and [`ArtifactStoreId::ProcessEnv`],
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

pub fn definition_refused(message: String) -> crate::PluginError {
    crate::PluginError::Runtime(crate::RuntimeError::new(
        crate::RuntimeErrorCode::DefinitionRefused,
        message,
    ))
}
