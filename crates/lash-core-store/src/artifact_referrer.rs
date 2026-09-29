//! Artifact referrers (ADR 0113 §1, §2): the only things that keep artifact
//! bytes alive.
//!
//! An artifact has one exact edge per (artifact, referrer) pair, and a
//! referrer is a durable reader. There are seven kinds. Each referrer has one
//! canonical `referrer_id` text, which is what the edge, fence and cleanup
//! tables store; [`ArtifactReferrer::decode`] refuses every stored pair whose
//! text is not exactly that rendering, so a store maps a refusal to
//! `StoredDataCorrupt` rather than skipping the row.
//!
//! The cleanup vocabulary lives here too: [`ArtifactCleanup`] is the durable
//! body of one cleanup obligation, and [`ResolvedArtifactCleanup`] is what one
//! artifact store applies once the executor has resolved its plan.

use std::fmt;
use std::hash::{Hash, Hasher};

use lash_sansio::{EffectJournalIdentity, ExecutionScope};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::FrameNodeId;
use crate::process_identity::StartKey;
use crate::{ProcessId, SessionId};

/// The seven referrer labels and their canonical id encodings at the 1.0 cut.
#[cfg(not(feature = "synthetic-next"))]
pub const ARTIFACT_REFERRER_KINDS_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) declares the vocabulary one ahead
/// and still writes only the seven kinds: no new kind is written before
/// finalize (ADR 0115 §5). A label a later build writes reaches N as an
/// unknown kind, which N refuses typed and never counts as absent.
#[cfg(feature = "synthetic-next")]
pub const ARTIFACT_REFERRER_KINDS_VERSION: u32 = 2;

/// The seven referrer kinds, as the `referrer_kind` column stores them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtifactReferrerKind {
    FrameEnvironment,
    ProcessRecord,
    SubscriptionRevision,
    Start,
    Execution,
    HostPin,
    DefinitionRevision,
}

impl ArtifactReferrerKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 7] = [
        Self::FrameEnvironment,
        Self::ProcessRecord,
        Self::SubscriptionRevision,
        Self::Start,
        Self::Execution,
        Self::HostPin,
        Self::DefinitionRevision,
    ];

    /// `frame_environment`, `process_record`, `subscription_revision`,
    /// `start`, `execution`, `host_pin`, `definition_revision`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FrameEnvironment => "frame_environment",
            Self::ProcessRecord => "process_record",
            Self::SubscriptionRevision => "subscription_revision",
            Self::Start => "start",
            Self::Execution => "execution",
            Self::HostPin => "host_pin",
            Self::DefinitionRevision => "definition_revision",
        }
    }

    /// The kind stored as `text`.
    ///
    /// # Errors
    ///
    /// [`ArtifactReferrerError::UnknownKind`] for a label no build wrote.
    pub fn parse(text: &str) -> Result<Self, ArtifactReferrerError> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == text)
            .ok_or_else(|| ArtifactReferrerError::UnknownKind(text.to_owned()))
    }

    /// Whether an edge of this kind is guarded (ADR 0113 §2.4): its first
    /// acquisition arms a cleanup record, because the referrer's own record
    /// may never commit.
    #[must_use]
    pub const fn is_guarded(self) -> bool {
        matches!(
            self,
            Self::Execution | Self::Start | Self::SubscriptionRevision | Self::DefinitionRevision
        )
    }
}

impl fmt::Display for ArtifactReferrerKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One durable reader of artifact bytes.
///
/// Serialized as `{"kind": <kind>, "id": <canonical id>}`, the pair the
/// tables store, and deserialized through [`Self::decode`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactReferrer {
    FrameEnvironment(FrameEnvironmentId),
    ProcessRecord(ProcessId),
    SubscriptionRevision(SubscriptionRevisionId),
    Start(StartKey),
    Execution(EffectJournalIdentity),
    HostPin(HostArtifactPin),
    DefinitionRevision(DefinitionRevisionId),
}

/// Hashes the stored pair: two referrers are equal exactly when their kinds
/// and canonical ids are, and `EffectJournalIdentity` has no `Hash` of its
/// own.
impl Hash for ArtifactReferrer {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.kind().hash(state);
        self.canonical_id().hash(state);
    }
}

impl ArtifactReferrer {
    #[must_use]
    pub fn kind(&self) -> ArtifactReferrerKind {
        match self {
            Self::FrameEnvironment(_) => ArtifactReferrerKind::FrameEnvironment,
            Self::ProcessRecord(_) => ArtifactReferrerKind::ProcessRecord,
            Self::SubscriptionRevision(_) => ArtifactReferrerKind::SubscriptionRevision,
            Self::Start(_) => ArtifactReferrerKind::Start,
            Self::Execution(_) => ArtifactReferrerKind::Execution,
            Self::HostPin(_) => ArtifactReferrerKind::HostPin,
            Self::DefinitionRevision(_) => ArtifactReferrerKind::DefinitionRevision,
        }
    }

    /// The canonical `referrer_id` text below. Infallible: every id type
    /// validates at construction.
    #[must_use]
    pub fn canonical_id(&self) -> String {
        match self {
            Self::FrameEnvironment(id) => {
                json_text(&(id.session_id.as_str(), id.frame_node_id.as_str()))
            }
            Self::ProcessRecord(id) => id.to_string(),
            Self::SubscriptionRevision(id) => json_text(&(
                id.subscription_id.as_str(),
                id.incarnation.as_str(),
                id.revision,
            )),
            Self::Start(key) => key.as_str().to_owned(),
            Self::Execution(journal) => journal.key().to_owned(),
            Self::HostPin(pin) => pin.as_str().to_owned(),
            Self::DefinitionRevision(id) => json_text(&(id.definition_id.as_str(), id.revision)),
        }
    }

    /// Typed decode of a stored pair. Refuses an unknown kind, an empty id,
    /// an id that does not decode, and an id whose re-encoding differs from
    /// the stored text. Stores map a refusal to `StoredDataCorrupt`.
    ///
    /// # Errors
    ///
    /// The [`ArtifactReferrerError`] naming why the pair is not a referrer.
    pub fn decode(kind: &str, id: &str) -> Result<Self, ArtifactReferrerError> {
        let kind = ArtifactReferrerKind::parse(kind)?;
        let label = kind.as_str();
        if id.is_empty() {
            return Err(ArtifactReferrerError::EmptyId { kind: label });
        }
        if id.contains('\0') {
            return Err(malformed(kind, "the id contains NUL"));
        }
        let referrer = match kind {
            ArtifactReferrerKind::FrameEnvironment => {
                let (session_id, frame_node_id): (String, String) = json_parse(kind, id)?;
                if session_id.is_empty() {
                    return Err(malformed(kind, "empty session id"));
                }
                let frame_node_id = FrameNodeId::new(frame_node_id)
                    .map_err(|error| malformed(kind, error.to_string()))?;
                Self::FrameEnvironment(FrameEnvironmentId::new(
                    SessionId::from(session_id),
                    frame_node_id,
                ))
            }
            ArtifactReferrerKind::ProcessRecord => Self::ProcessRecord(
                ProcessId::parse(id).map_err(|error| malformed(kind, error.to_string()))?,
            ),
            ArtifactReferrerKind::SubscriptionRevision => {
                let (subscription_id, incarnation, revision): (String, String, u64) =
                    json_parse(kind, id)?;
                Self::SubscriptionRevision(
                    SubscriptionRevisionId::new(subscription_id, incarnation, revision)
                        .map_err(|error| malformed(kind, error.to_string()))?,
                )
            }
            ArtifactReferrerKind::Start => Self::Start(
                StartKey::parse_rendered(id).map_err(|error| malformed(kind, error.to_string()))?,
            ),
            ArtifactReferrerKind::Execution => {
                Self::Execution(decode_journal_identity(id).map_err(|detail| {
                    ArtifactReferrerError::Malformed {
                        kind: label,
                        detail,
                    }
                })?)
            }
            ArtifactReferrerKind::HostPin => Self::HostPin(
                HostArtifactPin::try_from(id.to_owned())
                    .map_err(|error| malformed(kind, error.to_string()))?,
            ),
            ArtifactReferrerKind::DefinitionRevision => {
                let (definition_id, revision): (String, u64) = json_parse(kind, id)?;
                Self::DefinitionRevision(
                    DefinitionRevisionId::new(definition_id, revision)
                        .map_err(|error| malformed(kind, error.to_string()))?,
                )
            }
        };
        if referrer.canonical_id() != id {
            return Err(ArtifactReferrerError::NotCanonical { kind: label });
        }
        Ok(referrer)
    }
}

impl fmt::Display for ArtifactReferrer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.kind(), self.canonical_id())
    }
}

#[derive(Serialize, Deserialize)]
struct StoredReferrer<'a> {
    kind: std::borrow::Cow<'a, str>,
    id: std::borrow::Cow<'a, str>,
}

impl Serialize for ArtifactReferrer {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        StoredReferrer {
            kind: self.kind().as_str().into(),
            id: self.canonical_id().into(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ArtifactReferrer {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let stored = StoredReferrer::deserialize(deserializer)?;
        Self::decode(&stored.kind, &stored.id).map_err(serde::de::Error::custom)
    }
}

/// The environment of one agent frame: the globals a turn admitted on that
/// frame may bind.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FrameEnvironmentId {
    session_id: SessionId,
    frame_node_id: FrameNodeId,
}

impl FrameEnvironmentId {
    #[must_use]
    pub fn new(session_id: SessionId, frame_node_id: FrameNodeId) -> Self {
        Self {
            session_id,
            frame_node_id,
        }
    }

    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    #[must_use]
    pub fn frame_node_id(&self) -> &FrameNodeId {
        &self.frame_node_id
    }
}

/// One revision of one trigger subscription incarnation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionRevisionId {
    subscription_id: String,
    incarnation: String,
    revision: u64,
}

impl SubscriptionRevisionId {
    /// Refuses an empty id or incarnation and revision 0.
    ///
    /// # Errors
    ///
    /// [`ArtifactReferrerError::Malformed`] for any of those.
    pub fn new(
        subscription_id: String,
        incarnation: String,
        revision: u64,
    ) -> Result<Self, ArtifactReferrerError> {
        let kind = ArtifactReferrerKind::SubscriptionRevision;
        if subscription_id.is_empty() {
            return Err(malformed(kind, "empty subscription id"));
        }
        if incarnation.is_empty() {
            return Err(malformed(kind, "empty incarnation"));
        }
        if revision == 0 {
            return Err(malformed(kind, "revision 0"));
        }
        reject_nul(kind, &subscription_id)?;
        reject_nul(kind, &incarnation)?;
        Ok(Self {
            subscription_id,
            incarnation,
            revision,
        })
    }

    #[must_use]
    pub fn subscription_id(&self) -> &str {
        &self.subscription_id
    }

    #[must_use]
    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// One revision of one named process-definition slot.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DefinitionRevisionId {
    definition_id: String,
    revision: u64,
}

impl DefinitionRevisionId {
    /// `definition_id` is the registry's primary key
    /// (`lash.process-definition:<owner namespace>:<name>`). Refuses an empty
    /// id and revision 0.
    ///
    /// # Errors
    ///
    /// [`ArtifactReferrerError::Malformed`] for either.
    pub fn new(definition_id: String, revision: u64) -> Result<Self, ArtifactReferrerError> {
        let kind = ArtifactReferrerKind::DefinitionRevision;
        if definition_id.is_empty() {
            return Err(malformed(kind, "empty definition id"));
        }
        if revision == 0 {
            return Err(malformed(kind, "revision 0"));
        }
        reject_nul(kind, &definition_id)?;
        Ok(Self {
            definition_id,
            revision,
        })
    }

    #[must_use]
    pub fn definition_id(&self) -> &str {
        &self.definition_id
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

const HOST_PIN_PREFIX: &str = "host-pin:v1:";
const HOST_PIN_HEX_LEN: usize = 32;

/// An opaque, releasable host referrer. Only [`mint`](Self::mint) makes a new
/// one. Once released, a pin is fenced for good: a host that wants to publish
/// again mints a fresh pin.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostArtifactPin(String);

impl HostArtifactPin {
    /// `host-pin:v1:` followed by 32 lowercase hex digits of a random v4 UUID.
    #[must_use]
    pub fn mint() -> Self {
        Self(format!(
            "{HOST_PIN_PREFIX}{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HostArtifactPin {
    type Error = ArtifactReferrerError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let kind = ArtifactReferrerKind::HostPin;
        if text.is_empty() {
            return Err(ArtifactReferrerError::EmptyId {
                kind: kind.as_str(),
            });
        }
        let hex = text
            .strip_prefix(HOST_PIN_PREFIX)
            .ok_or_else(|| malformed(kind, format!("a pin starts with `{HOST_PIN_PREFIX}`")))?;
        let uuid = (hex.len() == HOST_PIN_HEX_LEN
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then(|| uuid::Uuid::parse_str(hex).ok())
        .flatten()
        .ok_or_else(|| {
            malformed(
                kind,
                format!("a pin ends with {HOST_PIN_HEX_LEN} lowercase hex digits"),
            )
        })?;
        if uuid.get_version_num() != 4 {
            return Err(malformed(kind, "a pin is a v4 UUID"));
        }
        Ok(Self(text))
    }
}

impl From<HostArtifactPin> for String {
    fn from(pin: HostArtifactPin) -> Self {
        pin.0
    }
}

impl fmt::Display for HostArtifactPin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Why a stored or constructed referrer is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ArtifactReferrerError {
    #[error("unknown artifact referrer kind `{0}`")]
    UnknownKind(String),
    #[error("empty {kind} referrer id")]
    EmptyId { kind: &'static str },
    #[error("malformed {kind} referrer id: {detail}")]
    Malformed { kind: &'static str, detail: String },
    #[error("{kind} referrer id is not canonical")]
    NotCanonical { kind: &'static str },
}

/// A write's referrer plus, for a guarded kind, the guard its first
/// acquisition arms (ADR 0113 §2.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferrerClaim {
    referrer: ArtifactReferrer,
    guard: Option<ArtifactCleanupPlan>,
}

impl ReferrerClaim {
    /// Unguarded kinds: `frame_environment`, `process_record`, `host_pin`.
    ///
    /// # Errors
    ///
    /// [`ArtifactReferrerError::Malformed`] for a guarded kind.
    pub fn unguarded(referrer: ArtifactReferrer) -> Result<Self, ArtifactReferrerError> {
        let kind = referrer.kind();
        if kind.is_guarded() {
            return Err(malformed(
                kind,
                "a guarded referrer is claimed with its guard",
            ));
        }
        Ok(Self {
            referrer,
            guard: None,
        })
    }

    /// `Execution` with `AwaitJournal`, `Start` with `AwaitStart`,
    /// `SubscriptionRevision` with `AwaitSubscriptionRevision`,
    /// `DefinitionRevision` with `AwaitDefinitionRevision`. Any other pairing,
    /// and every `Ended` plan, is refused.
    ///
    /// # Errors
    ///
    /// [`ArtifactReferrerError::Malformed`] for a refused pairing.
    pub fn guarded(
        referrer: ArtifactReferrer,
        guard: ArtifactCleanupPlan,
    ) -> Result<Self, ArtifactReferrerError> {
        let paired = matches!(
            (&referrer, &guard),
            (
                ArtifactReferrer::Execution(_),
                ArtifactCleanupPlan::AwaitJournal
            ) | (
                ArtifactReferrer::Start(_),
                ArtifactCleanupPlan::AwaitStart { .. }
            ) | (
                ArtifactReferrer::SubscriptionRevision(_),
                ArtifactCleanupPlan::AwaitSubscriptionRevision { .. }
            ) | (
                ArtifactReferrer::DefinitionRevision(_),
                ArtifactCleanupPlan::AwaitDefinitionRevision { .. }
            )
        );
        if !paired {
            return Err(malformed(
                referrer.kind(),
                format!("`{}` is not this referrer's guard", guard.label()),
            ));
        }
        Ok(Self {
            referrer,
            guard: Some(guard),
        })
    }

    #[must_use]
    pub fn referrer(&self) -> &ArtifactReferrer {
        &self.referrer
    }

    #[must_use]
    pub fn guard(&self) -> Option<&ArtifactCleanupPlan> {
        self.guard.as_ref()
    }

    /// The cleanup record the claim's guard arms, if it has one: the
    /// referrer, its guard plan, and no gate.
    #[must_use]
    pub fn guard_cleanup(&self) -> Option<ArtifactCleanup> {
        self.guard.as_ref().map(|plan| ArtifactCleanup {
            referrer: self.referrer.clone(),
            plan: plan.clone(),
            gate: None,
        })
    }
}

/// The store that holds an artifact.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(tag = "store", content = "kind", rename_all = "snake_case")]
pub enum ArtifactStoreId {
    ProcessEnv,
    LashlangModule,
    /// A process engine's own store, by engine kind.
    Engine(String),
}

impl ArtifactStoreId {
    /// The store behind the module artifact port.
    #[must_use]
    pub const fn module() -> Self {
        Self::LashlangModule
    }
}

/// One artifact, by the store that holds it and its reference there.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ArtifactName {
    pub store: ArtifactStoreId,
    pub artifact_ref: String,
}

/// One artifact an ended referrer hands to a successor before it is severed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactCarry {
    pub artifact: ArtifactName,
    pub to: ArtifactReferrer,
}

/// The durable body of one cleanup obligation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactCleanup {
    pub referrer: ArtifactReferrer,
    pub plan: ArtifactCleanupPlan,
    /// Sever nothing while this journal may still replay: the one execution
    /// that can still read the ended referrer's artifacts (ADR 0113 §4.1).
    #[serde(default, with = "optional_journal_identity")]
    pub gate: Option<EffectJournalIdentity>,
}

impl ArtifactCleanup {
    /// The `Ended` record of `referrer`, carrying `carries`, gated on `gate`.
    #[must_use]
    pub fn ended(
        referrer: ArtifactReferrer,
        carries: Vec<ArtifactCarry>,
        gate: Option<EffectJournalIdentity>,
    ) -> Self {
        Self {
            referrer,
            plan: ArtifactCleanupPlan::Ended { carries },
            gate,
        }
    }

    /// The record's `cleanup_json`.
    ///
    /// # Errors
    ///
    /// Never for a record this module built; the error is `serde_json`'s.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Decode a stored `cleanup_json`, and check it names `referrer`.
    ///
    /// # Errors
    ///
    /// The decode failure, or a body whose referrer is not the row's.
    pub fn from_json(text: &str, referrer: &ArtifactReferrer) -> Result<Self, String> {
        let cleanup: Self = serde_json::from_str(text).map_err(|error| error.to_string())?;
        if cleanup.referrer != *referrer {
            return Err(format!(
                "cleanup body names referrer `{}`, not the row's `{referrer}`",
                cleanup.referrer
            ));
        }
        Ok(cleanup)
    }
}

/// How a cleanup record resolves to carries.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "plan", rename_all = "snake_case")]
pub enum ArtifactCleanupPlan {
    /// The referrer has ended. Carry, then fence and sever.
    Ended { carries: Vec<ArtifactCarry> },
    /// Guard of an execution referrer: ends when its journal is settled.
    AwaitJournal,
    /// Guard of a start referrer (ADR 0113 §3.3).
    AwaitStart {
        #[serde(with = "journal_identity")]
        starter: EffectJournalIdentity,
    },
    /// Guard of a subscription revision acquired before its mutation commits.
    AwaitSubscriptionRevision {
        #[serde(with = "journal_identity")]
        creator: EffectJournalIdentity,
    },
    /// Guard of a definition revision acquired before its CAS commits.
    AwaitDefinitionRevision {
        #[serde(with = "journal_identity")]
        creator: EffectJournalIdentity,
    },
}

impl ArtifactCleanupPlan {
    /// Whether the referrer has ended; every other plan is a guard.
    #[must_use]
    pub const fn is_ended(&self) -> bool {
        matches!(self, Self::Ended { .. })
    }

    /// The plan's stored tag.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Ended { .. } => "ended",
            Self::AwaitJournal => "await_journal",
            Self::AwaitStart { .. } => "await_start",
            Self::AwaitSubscriptionRevision { .. } => "await_subscription_revision",
            Self::AwaitDefinitionRevision { .. } => "await_definition_revision",
        }
    }
}

/// What one store applies once the executor has resolved the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedArtifactCleanup {
    pub referrer: ArtifactReferrer,
    /// Only the carries whose `artifact.store` is the receiving store.
    pub carries: Vec<ArtifactCarry>,
}

impl ResolvedArtifactCleanup {
    /// The share of `carries` that `store` applies, in `artifact_ref` order.
    #[must_use]
    pub fn for_store(
        referrer: &ArtifactReferrer,
        carries: &[ArtifactCarry],
        store: &ArtifactStoreId,
    ) -> Self {
        let mut carries: Vec<ArtifactCarry> = carries
            .iter()
            .filter(|carry| carry.artifact.store == *store)
            .cloned()
            .collect();
        carries.sort_by(|left, right| {
            left.artifact
                .artifact_ref
                .cmp(&right.artifact.artifact_ref)
                .then_with(|| left.to.canonical_id().cmp(&right.to.canonical_id()))
        });
        Self {
            referrer: referrer.clone(),
            carries,
        }
    }
}

/// The journal named by a stored `execution` referrer id: the key must
/// decode to an execution scope whose journal key is the same text.
fn decode_journal_identity(key: &str) -> Result<EffectJournalIdentity, String> {
    let scope = ExecutionScope::from_journal_key(key)
        .ok_or_else(|| "not a journal key this build writes".to_owned())?;
    scope.journal_identity().map_err(|error| error.to_string())
}

/// Serde for a journal identity as its key text.
mod journal_identity {
    use super::*;

    pub(super) fn serialize<S: Serializer>(
        journal: &EffectJournalIdentity,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(journal.key())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<EffectJournalIdentity, D::Error> {
        let key = String::deserialize(deserializer)?;
        let journal = decode_journal_identity(&key).map_err(serde::de::Error::custom)?;
        if journal.key() != key {
            return Err(serde::de::Error::custom("journal key is not canonical"));
        }
        Ok(journal)
    }
}

/// Serde for an optional journal identity as its key text or null.
mod optional_journal_identity {
    use super::*;

    pub(super) fn serialize<S: Serializer>(
        journal: &Option<EffectJournalIdentity>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match journal {
            Some(journal) => serializer.serialize_some(journal.key()),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<EffectJournalIdentity>, D::Error> {
        let Some(key) = Option::<String>::deserialize(deserializer)? else {
            return Ok(None);
        };
        let journal = decode_journal_identity(&key).map_err(serde::de::Error::custom)?;
        if journal.key() != key {
            return Err(serde::de::Error::custom("journal key is not canonical"));
        }
        Ok(Some(journal))
    }
}

fn json_text<T: Serialize>(value: &T) -> String {
    #[expect(
        clippy::expect_used,
        reason = "tuples of strings and integers always encode"
    )]
    serde_json::to_string(value).expect("a tuple of strings and integers encodes")
}

fn json_parse<T: for<'de> Deserialize<'de>>(
    kind: ArtifactReferrerKind,
    id: &str,
) -> Result<T, ArtifactReferrerError> {
    serde_json::from_str(id).map_err(|error| malformed(kind, error.to_string()))
}

fn malformed(kind: ArtifactReferrerKind, detail: impl Into<String>) -> ArtifactReferrerError {
    ArtifactReferrerError::Malformed {
        kind: kind.as_str(),
        detail: detail.into(),
    }
}

fn reject_nul(kind: ArtifactReferrerKind, text: &str) -> Result<(), ArtifactReferrerError> {
    if text.contains('\0') {
        return Err(malformed(kind, "the id contains NUL"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> EffectJournalIdentity {
        ExecutionScope::turn("session", "turn")
            .journal_identity()
            .expect("a turn scope has a journal")
    }

    fn start_key() -> StartKey {
        StartKey::parse_rendered(&format!(
            "process-start-key:v1:intent:blake3:{}",
            "a".repeat(64)
        ))
        .expect("a rendered start key")
    }

    fn every_kind() -> Vec<ArtifactReferrer> {
        vec![
            ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(
                SessionId::from("s-1"),
                FrameNodeId::new("frame-\"1\"").expect("frame node id"),
            )),
            ArtifactReferrer::ProcessRecord(ProcessId::fixture("record")),
            ArtifactReferrer::SubscriptionRevision(
                SubscriptionRevisionId::new("sub".to_owned(), "inc".to_owned(), 3)
                    .expect("revision"),
            ),
            ArtifactReferrer::Start(start_key()),
            ArtifactReferrer::Execution(journal()),
            ArtifactReferrer::HostPin(HostArtifactPin::mint()),
            ArtifactReferrer::DefinitionRevision(
                DefinitionRevisionId::new("lash.process-definition:ns:name".to_owned(), 2)
                    .expect("revision"),
            ),
        ]
    }

    #[test]
    fn every_kind_round_trips_through_its_canonical_id() {
        let referrers = every_kind();
        assert_eq!(
            referrers
                .iter()
                .map(ArtifactReferrer::kind)
                .collect::<Vec<_>>(),
            ArtifactReferrerKind::ALL.to_vec()
        );
        for referrer in referrers {
            let id = referrer.canonical_id();
            assert!(!id.is_empty() && !id.contains('\0'));
            let decoded =
                ArtifactReferrer::decode(referrer.kind().as_str(), &id).expect("round trip");
            assert_eq!(decoded, referrer);
            assert_eq!(decoded.canonical_id(), id);
            let json = serde_json::to_string(&referrer).expect("encode");
            assert_eq!(
                serde_json::from_str::<ArtifactReferrer>(&json).expect("decode"),
                referrer
            );
        }
    }

    #[test]
    fn canonical_ids_are_the_pinned_texts() {
        let frame = ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(
            SessionId::from("s"),
            FrameNodeId::new("f").expect("frame node id"),
        ));
        assert_eq!(frame.canonical_id(), r#"["s","f"]"#);
        let revision = ArtifactReferrer::SubscriptionRevision(
            SubscriptionRevisionId::new("sub".to_owned(), "inc".to_owned(), 7).expect("revision"),
        );
        assert_eq!(revision.canonical_id(), r#"["sub","inc",7]"#);
        let definition = ArtifactReferrer::DefinitionRevision(
            DefinitionRevisionId::new("def".to_owned(), 1).expect("revision"),
        );
        assert_eq!(definition.canonical_id(), r#"["def",1]"#);
        assert_eq!(
            ArtifactReferrer::Execution(journal()).canonical_id(),
            r#"{"version":2,"kind":"turn","session_id":"session","execution_id":"turn"}"#
        );
    }

    #[test]
    fn stored_pairs_that_are_not_referrers_are_refused() {
        for (kind, id, expected) in [
            (
                "owner",
                "x",
                ArtifactReferrerError::UnknownKind("owner".to_owned()),
            ),
            (
                "host_pin",
                "",
                ArtifactReferrerError::EmptyId { kind: "host_pin" },
            ),
            (
                "frame_environment",
                r#"[ "s","f"]"#,
                ArtifactReferrerError::NotCanonical {
                    kind: "frame_environment",
                },
            ),
            (
                "execution",
                r#"{"kind":"turn","version":2,"session_id":"session","execution_id":"turn"}"#,
                ArtifactReferrerError::NotCanonical { kind: "execution" },
            ),
        ] {
            assert_eq!(ArtifactReferrer::decode(kind, id), Err(expected), "{kind}");
        }
        for (kind, id) in [
            ("frame_environment", r#"["s"]"#),
            ("frame_environment", r#"["","f"]"#),
            ("subscription_revision", r#"["sub","inc",0]"#),
            ("definition_revision", r#"["",1]"#),
            ("process_record", "not-a-process"),
            ("start", "process-start:x"),
            ("execution", "{}"),
            ("host_pin", "host-pin:v1:XYZ"),
            ("host_pin", "host-pin:v1:0000000000000000000000000000000g"),
        ] {
            assert!(
                matches!(
                    ArtifactReferrer::decode(kind, id),
                    Err(ArtifactReferrerError::Malformed { .. })
                ),
                "{kind} {id}"
            );
        }
    }

    #[test]
    fn a_minted_pin_is_a_v4_uuid_under_its_prefix() {
        let pin = HostArtifactPin::mint();
        assert!(pin.as_str().starts_with(HOST_PIN_PREFIX));
        assert_eq!(
            HostArtifactPin::try_from(pin.as_str().to_owned()),
            Ok(pin.clone())
        );
        let json = serde_json::to_string(&pin).expect("encode");
        assert_eq!(
            serde_json::from_str::<HostArtifactPin>(&json).expect("decode"),
            pin
        );
        assert!(serde_json::from_str::<HostArtifactPin>(r#""host-pin:v1:zz""#).is_err());
    }

    #[test]
    fn claims_pair_each_guarded_kind_with_its_own_guard() {
        let referrers = every_kind();
        for referrer in &referrers {
            let guarded = referrer.kind().is_guarded();
            assert_eq!(ReferrerClaim::unguarded(referrer.clone()).is_ok(), !guarded);
        }
        let execution = ArtifactReferrer::Execution(journal());
        assert!(
            ReferrerClaim::guarded(execution.clone(), ArtifactCleanupPlan::AwaitJournal).is_ok()
        );
        assert!(
            ReferrerClaim::guarded(
                execution.clone(),
                ArtifactCleanupPlan::AwaitStart { starter: journal() }
            )
            .is_err()
        );
        assert!(
            ReferrerClaim::guarded(execution, ArtifactCleanupPlan::Ended { carries: vec![] })
                .is_err()
        );
        let start = ArtifactReferrer::Start(start_key());
        let claim = ReferrerClaim::guarded(
            start.clone(),
            ArtifactCleanupPlan::AwaitStart { starter: journal() },
        )
        .expect("a start's guard");
        assert_eq!(
            claim.guard_cleanup(),
            Some(ArtifactCleanup {
                referrer: start,
                plan: ArtifactCleanupPlan::AwaitStart { starter: journal() },
                gate: None,
            })
        );
    }

    #[test]
    fn cleanup_bodies_round_trip_and_check_their_row() {
        let referrers = every_kind();
        let ended = ArtifactCleanup::ended(
            referrers[0].clone(),
            vec![ArtifactCarry {
                artifact: ArtifactName {
                    store: ArtifactStoreId::Engine("lashlang".to_owned()),
                    artifact_ref: "mod-1".to_owned(),
                },
                to: referrers[1].clone(),
            }],
            Some(journal()),
        );
        let json = ended.to_json().expect("encode");
        assert_eq!(
            ArtifactCleanup::from_json(&json, &referrers[0]),
            Ok(ended.clone())
        );
        assert!(ArtifactCleanup::from_json(&json, &referrers[1]).is_err());
        for plan in [
            ArtifactCleanupPlan::AwaitJournal,
            ArtifactCleanupPlan::AwaitStart { starter: journal() },
            ArtifactCleanupPlan::AwaitSubscriptionRevision { creator: journal() },
            ArtifactCleanupPlan::AwaitDefinitionRevision { creator: journal() },
        ] {
            let cleanup = ArtifactCleanup {
                referrer: referrers[4].clone(),
                plan,
                gate: None,
            };
            let json = cleanup.to_json().expect("encode");
            assert_eq!(
                ArtifactCleanup::from_json(&json, &referrers[4]),
                Ok(cleanup)
            );
        }
    }

    #[test]
    fn a_store_receives_only_its_carries_in_artifact_order() {
        let to = ArtifactReferrer::ProcessRecord(ProcessId::fixture("to"));
        let carry = |store: ArtifactStoreId, artifact_ref: &str| ArtifactCarry {
            artifact: ArtifactName {
                store,
                artifact_ref: artifact_ref.to_owned(),
            },
            to: to.clone(),
        };
        let carries = vec![
            carry(ArtifactStoreId::LashlangModule, "b"),
            carry(ArtifactStoreId::ProcessEnv, "env"),
            carry(ArtifactStoreId::LashlangModule, "a"),
        ];
        let resolved =
            ResolvedArtifactCleanup::for_store(&to, &carries, &ArtifactStoreId::LashlangModule);
        assert_eq!(
            resolved.carries,
            vec![
                carry(ArtifactStoreId::LashlangModule, "a"),
                carry(ArtifactStoreId::LashlangModule, "b"),
            ]
        );
    }
}
