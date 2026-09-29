//! Immutable process definitions, named by their content.
//!
//! A definition is immutable (ADR 0095 as amended for FIG-4174). Lash has no
//! names, revisions, compare-and-swap or replace for definitions: a host that
//! wants a name or a version keeps `(name, version) -> ProcessDefinitionId` in
//! its own database, and Lash only ever sees the id.
//!
//! [`ProcessDefinitionDraft`] is the canonical descriptor — engine kind,
//! engine-owned definition value, and the artifacts the definition reads — and
//! [`ProcessDefinitionDraft::id`] is the one derivation from that content to a
//! [`ProcessDefinitionId`]. Equal canonical descriptors share one id; any
//! changed byte of the engine kind, the canonical value or the artifact set
//! names a different definition.
//!
//! The signature is *derived*, not hashed. It is what the owning engine's
//! resolution of the descriptor says ([`ProcessEngineRegistry::derive_definition`]),
//! so it is not in the preimage; a [`ProcessDefinition`] travelling on a value
//! carries it as a claim, and
//! [`ProcessEngineRegistry::verify_definition_claim`] refuses a claim that
//! disagrees with the derivation.
//!
//! An id alone retains nothing. A host pin, a frame, a process record, a
//! subscription revision, a start or a journal retains a definition's
//! artifacts (ADR 0113); a copied id is data.
//!
//! # Model contract
//!
//! ```typescript
//! type DefinitionId = { $lash_definition_id: string };
//! type ProcessSignature = { signature: "unknown" } | { signature: "known"; encoding: unknown };
//! type Definition = { id: DefinitionId; signature: ProcessSignature };
//! type Target = { definition: Definition } | { definition_id: DefinitionId };
//! processes.create({ source: string, dialect: "typescript" }): Promise<Definition>;
//! processes.start(Target & { args?: Record<string, unknown>; label?: string }): Promise<ProcessHandle>;
//! processes.get({ definition_id: DefinitionId }): Promise<Definition>;
//! ```
//!
//! `DefinitionId` is [`ProcessDefinitionId`], `Definition` is
//! [`ProcessDefinition`] and `Target` is [`ProcessDefinitionTarget`]; each
//! refuses unknown fields.
//!
//! # Host contract
//!
//! ```text
//! publish_definition(pin: HostArtifactPin, draft: ProcessDefinitionDraft) -> Result<ProcessDefinition>
//! pin_definition(pin: HostArtifactPin, id: ProcessDefinitionId) -> Result<()>
//! get_definition(id: ProcessDefinitionId) -> Result<Option<ProcessDefinition>>
//! release(pin: HostArtifactPin) -> Result<()>
//! start(request: ProcessStartRequest, controller: ScopedEffectController) -> Result<ProcessStartReceipt>
//! ```
//!
//! A host publishes the module bytes a draft names under the same pin before it
//! publishes the draft. `get_definition` answers a snapshot and acquires no
//! lasting pin; only `publish_definition` and `pin_definition` retain.

use serde::{Deserialize, Serialize};

use crate::{ArtifactName, ArtifactStoreId};

use super::definition_ref::{
    ProcessDefinitionRef, ProcessDefinitionRefusal, ProcessDefinitionValue, ProcessEngineKind,
    ProcessSignature,
};
use super::engine::ProcessEngineRegistry;

pub use lash_sansio::{InvalidProcessDefinitionId, ProcessDefinitionId};

/// The identity family that owns the definition-id preimage.
const PROCESS_DEFINITION_ID_DOMAIN: &str = "lash.process-definition-id";

/// Version 1 of that family's grammar; the grammar is frozen by the golden
/// vectors below.
const PROCESS_DEFINITION_ID_FAMILY_VERSION: u8 = 1;

/// Store tags in the preimage. Permanent: a retired store keeps its tag.
const STORE_TAG_PROCESS_ENV: u8 = 1;
const STORE_TAG_LASHLANG_MODULE: u8 = 2;
const STORE_TAG_ENGINE: u8 = 3;

/// The canonical descriptor of one immutable process definition.
///
/// Its artifacts are held sorted by their preimage bytes and deduplicated, so
/// two drafts naming the same artifacts in any order, any number of times, are
/// the same draft. Serializes as `{engine_kind, value, artifacts}` and refuses
/// unknown fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ProcessDefinitionDraftFields")]
pub struct ProcessDefinitionDraft {
    engine_kind: ProcessEngineKind,
    value: ProcessDefinitionValue,
    artifacts: Vec<ArtifactName>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessDefinitionDraftFields {
    engine_kind: ProcessEngineKind,
    value: ProcessDefinitionValue,
    artifacts: Vec<ArtifactName>,
}

impl TryFrom<ProcessDefinitionDraftFields> for ProcessDefinitionDraft {
    type Error = ProcessDefinitionDraftError;

    fn try_from(fields: ProcessDefinitionDraftFields) -> Result<Self, Self::Error> {
        Self::new(fields.engine_kind, fields.value, fields.artifacts)
    }
}

/// Why a draft cannot name a definition.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProcessDefinitionDraftError {
    #[error("a process definition names no engine kind")]
    EmptyEngineKind,
    #[error("a process definition artifact has an empty reference")]
    EmptyArtifactRef,
    #[error("a process definition artifact names an engine store with no engine kind")]
    EmptyArtifactEngineKind,
}

impl ProcessDefinitionDraft {
    /// The canonical descriptor of `value`, owned by the `engine_kind` engine
    /// and reading `artifacts`.
    ///
    /// # Errors
    ///
    /// [`ProcessDefinitionDraftError`] for an empty engine kind, an empty
    /// artifact reference or an engine store with no kind.
    pub fn new(
        engine_kind: impl Into<ProcessEngineKind>,
        value: impl Into<ProcessDefinitionValue>,
        artifacts: impl IntoIterator<Item = ArtifactName>,
    ) -> Result<Self, ProcessDefinitionDraftError> {
        let engine_kind = engine_kind.into();
        if engine_kind.is_empty() {
            return Err(ProcessDefinitionDraftError::EmptyEngineKind);
        }
        let mut framed = Vec::new();
        for artifact in artifacts {
            if artifact.artifact_ref.is_empty() {
                return Err(ProcessDefinitionDraftError::EmptyArtifactRef);
            }
            if matches!(&artifact.store, ArtifactStoreId::Engine(kind) if kind.is_empty()) {
                return Err(ProcessDefinitionDraftError::EmptyArtifactEngineKind);
            }
            framed.push((artifact_preimage(&artifact), artifact));
        }
        framed.sort_by(|left, right| left.0.cmp(&right.0));
        framed.dedup_by(|left, right| left.0 == right.0);
        Ok(Self {
            engine_kind,
            value: value.into(),
            artifacts: framed.into_iter().map(|(_, artifact)| artifact).collect(),
        })
    }

    pub fn engine_kind(&self) -> &ProcessEngineKind {
        &self.engine_kind
    }

    pub fn value(&self) -> &ProcessDefinitionValue {
        &self.value
    }

    /// The artifacts, sorted by preimage bytes and deduplicated.
    pub fn artifacts(&self) -> &[ArtifactName] {
        &self.artifacts
    }

    /// The domain-separated, length-framed bytes the id is the SHA-256 of.
    ///
    /// The `lash.process-definition-id` family, version 1:
    ///
    /// ```text
    /// "lash-stable-identity" || salt:u8 || 1:u8 || len:u64 || "lash.process-definition-id"
    /// || len:u64 || engine kind (UTF-8)
    /// || len:u64 || canonical JSON of the value (identity_json::payload_leaf)
    /// || count:u64 || each artifact, sorted by its leaf bytes, deduplicated:
    ///      len:u64 || leaf, where leaf =
    ///        tag:u8 (1 process_env, 2 lashlang_module, 3 engine)
    ///        [|| len:u64 || engine kind (UTF-8), for tag 3]
    ///        || len:u64 || artifact reference (UTF-8)
    /// ```
    ///
    /// Every length and count is big-endian. The signature is not in it.
    pub fn canonical_preimage(&self) -> Vec<u8> {
        let mut encoder = crate::stable_identity::IdentityEncoder::new(
            PROCESS_DEFINITION_ID_DOMAIN,
            PROCESS_DEFINITION_ID_FAMILY_VERSION,
        );
        encoder.string(self.engine_kind.as_str());
        encoder.bytes(&crate::identity_json::payload_leaf(self.value.as_json()));
        encoder.sequence(&self.artifacts, |encoder, artifact| {
            encoder.bytes(&artifact_preimage(artifact));
        });
        encoder.finish()
    }

    /// The id of the definition this descriptor is.
    pub fn id(&self) -> ProcessDefinitionId {
        use sha2::Digest as _;
        ProcessDefinitionId::from_sha256_digest(
            sha2::Sha256::digest(self.canonical_preimage()).into(),
        )
    }

    /// The engine reference this descriptor resolves through, claiming no
    /// signature.
    fn unclaimed_reference(&self) -> ProcessDefinitionRef {
        ProcessDefinitionRef::unclaimed(self.engine_kind.clone(), self.value.clone())
    }
}

/// One artifact's leaf in the preimage, which is also its sort key: the order
/// does not depend on how any Rust type is declared.
fn artifact_preimage(artifact: &ArtifactName) -> Vec<u8> {
    let mut bytes = Vec::new();
    match &artifact.store {
        ArtifactStoreId::ProcessEnv => bytes.push(STORE_TAG_PROCESS_ENV),
        ArtifactStoreId::LashlangModule => bytes.push(STORE_TAG_LASHLANG_MODULE),
        ArtifactStoreId::Engine(kind) => {
            bytes.push(STORE_TAG_ENGINE);
            push_framed(&mut bytes, kind.as_bytes());
        }
    }
    push_framed(&mut bytes, artifact.artifact_ref.as_bytes());
    bytes
}

fn push_framed(bytes: &mut Vec<u8>, value: &[u8]) {
    bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(value);
}

/// One immutable definition as a holder sees it: its id and its signature.
///
/// Returned by create, get and publish with the derived signature. When a
/// value carries one back in, its signature is a claim that
/// [`ProcessEngineRegistry::verify_definition_claim`] checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessDefinition {
    pub id: ProcessDefinitionId,
    pub signature: ProcessSignature,
}

impl ProcessDefinition {
    pub fn new(id: ProcessDefinitionId, signature: ProcessSignature) -> Self {
        Self { id, signature }
    }
}

/// What a start or a trigger names: a definition value or a bare id.
///
/// Exactly one of `{definition}` or `{definition_id}`; anything else is
/// refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessDefinitionTarget {
    Definition(ProcessDefinition),
    DefinitionId(ProcessDefinitionId),
}

impl ProcessDefinitionTarget {
    /// The id the target names.
    pub fn definition_id(&self) -> &ProcessDefinitionId {
        match self {
            Self::Definition(definition) => &definition.id,
            Self::DefinitionId(id) => id,
        }
    }

    /// The signature the target claims; a bare id claims none.
    pub fn signature_claim(&self) -> &ProcessSignature {
        const UNCLAIMED: &ProcessSignature = &ProcessSignature::Unknown;
        match self {
            Self::Definition(definition) => &definition.signature,
            Self::DefinitionId(_) => UNCLAIMED,
        }
    }
}

impl ProcessEngineRegistry {
    /// The definition `draft` is: its content-derived id and the signature
    /// its owning engine derives for it.
    ///
    /// # Errors
    ///
    /// The engine's [`ProcessDefinitionRefusal`]: no engine of that kind, or
    /// one that cannot resolve the value.
    pub async fn derive_definition(
        &self,
        draft: &ProcessDefinitionDraft,
    ) -> Result<ProcessDefinition, ProcessDefinitionRefusal> {
        let resolution = self.resolve(&draft.unclaimed_reference()).await?;
        Ok(ProcessDefinition::new(draft.id(), resolution.signature))
    }

    /// Checks a held definition value against the descriptor its id names,
    /// answering the derived definition.
    ///
    /// The claimed id must be `draft`'s. An unknown signature asserts nothing
    /// and adopts the derivation; a known one must equal it exactly.
    ///
    /// # Errors
    ///
    /// [`ProcessDefinitionRefusal::DefinitionIdMismatch`] when the descriptor
    /// is not the claimed id's, [`ProcessDefinitionRefusal::SignatureMismatch`]
    /// for a forged signature, and any refusal of
    /// [`Self::derive_definition`].
    pub async fn verify_definition_claim(
        &self,
        draft: &ProcessDefinitionDraft,
        claimed: &ProcessDefinition,
    ) -> Result<ProcessDefinition, ProcessDefinitionRefusal> {
        let derived_id = draft.id();
        if claimed.id != derived_id {
            return Err(ProcessDefinitionRefusal::DefinitionIdMismatch {
                claimed: claimed.id.clone(),
                derived: derived_id,
            });
        }
        let derived = self.derive_definition(draft).await?;
        if !claimed.signature.is_unknown() && claimed.signature != derived.signature {
            return Err(ProcessDefinitionRefusal::SignatureMismatch {
                engine_kind: draft.engine_kind.clone(),
                claimed: claimed.signature.clone(),
                authoritative: derived.signature,
            });
        }
        Ok(derived)
    }
}

#[cfg(test)]
#[path = "definition_tests.rs"]
mod tests;
