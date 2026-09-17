//! The one typed reference a durable process row pins for its definition.
//!
//! A process definition used to travel as an untyped JSON blob on
//! [`ProcessIdentity`](super::model::ProcessIdentity), compared by equality and
//! settable through builders. Nothing could say which engine owned the bytes,
//! and nothing could tell a genuine definition from one a tool made up.
//!
//! [`ProcessDefinitionRef`] is that shape: the engine that owns the definition,
//! the engine-owned definition value itself, and the signature the holder
//! *claims* the definition has. The claim is never authority — the engine's
//! stored artifact is, and [`ProcessDefinitionResolution`] is what it returns.

use serde::{Deserialize, Serialize};

use super::events::ProcessEventType;

/// The visible kind of the engine that owns a definition.
///
/// A durable process row records this string, so the type exists to stop the
/// same value being re-spelled at each boundary it crosses. It serializes as a
/// bare string: the newtype is a source-level fact, not a wire change.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessEngineKind(String);

impl ProcessEngineKind {
    /// Constructs a `ProcessEngineKind` for protocol and process-engine implementors while running
    /// a durable process.
    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }

    /// Borrows the engine kind as the string every durable row and filter spells.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reports whether this engine kind is empty, which no registered engine may be.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&str> for ProcessEngineKind {
    fn from(kind: &str) -> Self {
        Self(kind.to_string())
    }
}

impl From<String> for ProcessEngineKind {
    fn from(kind: String) -> Self {
        Self(kind)
    }
}

impl From<ProcessEngineKind> for String {
    fn from(kind: ProcessEngineKind) -> Self {
        kind.0
    }
}

impl AsRef<str> for ProcessEngineKind {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProcessEngineKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl PartialEq<str> for ProcessEngineKind {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ProcessEngineKind {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<ProcessEngineKind> for str {
    fn eq(&self, other: &ProcessEngineKind) -> bool {
        other.0 == *self
    }
}

impl PartialEq<ProcessEngineKind> for &str {
    fn eq(&self, other: &ProcessEngineKind) -> bool {
        other.0 == **self
    }
}

/// The engine-owned encoding of a process definition.
///
/// Core never interprets these bytes: the engine that publishes a definition is
/// the only thing that can decode one. Core does compare them, through the
/// canonical payload encoding the fingerprint is taken over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessDefinitionValue(serde_json::Value);

impl ProcessDefinitionValue {
    /// Constructs a `ProcessDefinitionValue` for process-engine implementors publishing the
    /// definition their artifacts store.
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    /// Borrows the engine-owned definition value.
    pub fn as_json(&self) -> &serde_json::Value {
        &self.0
    }

    /// Takes the engine-owned definition value.
    pub fn into_json(self) -> serde_json::Value {
        self.0
    }
}

impl From<serde_json::Value> for ProcessDefinitionValue {
    fn from(value: serde_json::Value) -> Self {
        Self(value)
    }
}

impl From<ProcessDefinitionValue> for serde_json::Value {
    fn from(value: ProcessDefinitionValue) -> Self {
        value.0
    }
}

/// A process signature as it travels on a value: a claim, never authority.
///
/// The engine's stored artifact decides what a definition's real signature is;
/// see [`ProcessEngine::resolve`](super::engine::ProcessEngine::resolve). Core
/// compares claim against authority and refuses a disagreement before any
/// durable row exists.
///
/// [`ProcessSignature::Unknown`] is how a holder says it asserts nothing
/// (ADR 0090's unknown process type). It is not a wildcard that matches: it is
/// the absence of a claim, so resolution adopts the authority outright. A
/// [`ProcessSignature::Known`] claim must equal the authority exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "signature", rename_all = "snake_case")]
pub enum ProcessSignature {
    Unknown,
    Known { encoding: serde_json::Value },
}

impl ProcessSignature {
    /// Constructs a known `ProcessSignature` from its engine-owned encoding.
    pub fn known(encoding: impl Into<serde_json::Value>) -> Self {
        Self::Known {
            encoding: encoding.into(),
        }
    }

    /// The engine-owned encoding of a known signature, or `None` for unknown.
    pub fn encoding(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Unknown => None,
            Self::Known { encoding } => Some(encoding),
        }
    }

    /// Reports whether this signature asserts nothing.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

impl std::fmt::Display for ProcessSignature {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => formatter.write_str("unknown"),
            Self::Known { encoding } => write!(formatter, "{encoding}"),
        }
    }
}

/// Family version of the definition-reference fingerprint preimage.
const PROCESS_DEFINITION_REF_FAMILY_VERSION: u8 = 1;

/// A typed, verifiable reference to one process definition.
///
/// The fingerprint covers the engine kind and the definition value and
/// deliberately excludes the signature: a claim must never change which
/// definition a reference names, or a forged claim would name a different
/// process instead of being refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessDefinitionRef {
    pub engine_kind: ProcessEngineKind,
    pub definition: ProcessDefinitionValue,
    pub signature: ProcessSignature,
}

impl ProcessDefinitionRef {
    /// Constructs a `ProcessDefinitionRef` for protocol and process-engine implementors while
    /// starting or registering a durable process.
    pub fn new(
        engine_kind: impl Into<ProcessEngineKind>,
        definition: impl Into<ProcessDefinitionValue>,
        signature: ProcessSignature,
    ) -> Self {
        Self {
            engine_kind: engine_kind.into(),
            definition: definition.into(),
            signature,
        }
    }

    /// A reference that claims no signature, for a holder that cannot
    /// truthfully assert one. Resolution adopts the engine's authority.
    pub fn unclaimed(
        engine_kind: impl Into<ProcessEngineKind>,
        definition: impl Into<ProcessDefinitionValue>,
    ) -> Self {
        Self::new(engine_kind, definition, ProcessSignature::Unknown)
    }

    /// This reference with the engine's authoritative signature in place of
    /// whatever it claimed. Only the registry calls this, after a successful
    /// resolve, so a durable row pins the authority rather than the claim.
    pub fn with_resolved_signature(mut self, signature: ProcessSignature) -> Self {
        self.signature = signature;
        self
    }

    /// The stable identity of the definition this reference names.
    ///
    /// Two references with the same engine kind and definition value fingerprint
    /// equal whatever signature either one claims, which is what makes a
    /// definition filter and a pinned durable record comparable.
    pub fn fingerprint(&self) -> String {
        let mut encoder = crate::stable_identity::IdentityEncoder::new(
            "lash.process-definition-reference",
            PROCESS_DEFINITION_REF_FAMILY_VERSION,
        );
        encoder.string(self.engine_kind.as_str());
        encoder.bytes(&crate::identity_json::payload_leaf(
            self.definition.as_json(),
        ));
        crate::stable_identity::rendered_hash(
            "process-definition-reference",
            PROCESS_DEFINITION_REF_FAMILY_VERSION,
            &encoder.finish(),
        )
    }

    /// Tests whether two references name the same definition, ignoring claims.
    pub fn names_same_definition(&self, other: &Self) -> bool {
        self.engine_kind == other.engine_kind && self.definition == other.definition
    }
}

/// What an engine's stored artifact says about a definition it owns.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessDefinitionResolution {
    /// The authoritative signature, read from the engine's stored artifact.
    pub signature: ProcessSignature,
    /// The signal event types the definition declares or infers.
    pub signals: Vec<ProcessEventType>,
}

impl ProcessDefinitionResolution {
    /// Constructs a `ProcessDefinitionResolution` for process-engine implementors answering
    /// [`ProcessEngine::resolve`](super::engine::ProcessEngine::resolve).
    pub fn new(
        signature: impl Into<ProcessSignature>,
        signals: impl IntoIterator<Item = ProcessEventType>,
    ) -> Self {
        Self {
            signature: signature.into(),
            signals: signals.into_iter().collect(),
        }
    }
}

/// Why a process definition reference was refused.
///
/// Every variant is a refusal that must happen *before* a durable process row
/// exists, so each one is the reason a start never became a process.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProcessDefinitionRefusal {
    /// No engine of that kind is installed on this host.
    UnknownEngine { engine_kind: ProcessEngineKind },
    /// The engine of that kind stores no artifacts and resolves no reference.
    UnsupportedByEngine { engine_kind: ProcessEngineKind },
    /// The engine has no artifact for the definition this reference names.
    UnresolvableDefinition {
        engine_kind: ProcessEngineKind,
        message: String,
    },
    /// The signature travelling on the reference disagrees with the artifact.
    SignatureMismatch {
        engine_kind: ProcessEngineKind,
        claimed: ProcessSignature,
        authoritative: ProcessSignature,
    },
}

impl std::fmt::Display for ProcessDefinitionRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownEngine { engine_kind } => write!(
                formatter,
                "process definition names engine `{engine_kind}`, which is not configured"
            ),
            Self::UnsupportedByEngine { engine_kind } => write!(
                formatter,
                "process engine `{engine_kind}` resolves no definition reference"
            ),
            Self::UnresolvableDefinition {
                engine_kind,
                message,
            } => write!(
                formatter,
                "process engine `{engine_kind}` cannot resolve the definition: {message}"
            ),
            Self::SignatureMismatch {
                engine_kind,
                claimed,
                authoritative,
            } => write!(
                formatter,
                "process definition signature claim `{claimed}` disagrees with the `{engine_kind}` \
                 artifact signature `{authoritative}`"
            ),
        }
    }
}

impl std::error::Error for ProcessDefinitionRefusal {}

impl From<ProcessDefinitionRefusal> for crate::PluginError {
    fn from(refusal: ProcessDefinitionRefusal) -> Self {
        crate::PluginError::Session(refusal.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROZEN_FINGERPRINT: &str = "process-definition-reference:v1:blake3:90c45963ed9181c48b645bdf2b8a681d1f91efac64d92cb12dd8fe5c58052e32";

    fn reference(signature: ProcessSignature) -> ProcessDefinitionRef {
        ProcessDefinitionRef::new(
            "scripted-engine",
            serde_json::json!({"marked": true, "process_name": "scan"}),
            signature,
        )
    }

    #[test]
    fn fingerprint_ignores_the_signature_claim() {
        let claimed = reference(ProcessSignature::known(
            serde_json::json!({"params": [], "output": "bool"}),
        ));
        let forged = reference(ProcessSignature::known(serde_json::json!("anything else")));

        assert_eq!(claimed.fingerprint(), forged.fingerprint());
        assert!(claimed.names_same_definition(&forged));
        assert_ne!(claimed, forged);
    }

    #[test]
    fn fingerprint_separates_engine_kinds_and_definitions() {
        let base = reference(ProcessSignature::Unknown);
        let other_engine =
            ProcessDefinitionRef::unclaimed("other", base.definition.clone().into_json());
        let other_definition = ProcessDefinitionRef::unclaimed(
            "scripted-engine",
            serde_json::json!({"marked": true, "process_name": "other"}),
        );

        assert_ne!(base.fingerprint(), other_engine.fingerprint());
        assert_ne!(base.fingerprint(), other_definition.fingerprint());
    }

    #[test]
    fn fingerprint_is_frozen() {
        assert_eq!(
            reference(ProcessSignature::Unknown).fingerprint(),
            FROZEN_FINGERPRINT
        );
    }
}
