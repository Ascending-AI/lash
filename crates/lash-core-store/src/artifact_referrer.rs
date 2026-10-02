//! Artifact referrers (ADR 0113 §1, §2): the only things that keep artifact
//! bytes alive.
//!
//! An artifact has one exact edge per (artifact, referrer) pair, and a
//! referrer is a durable reader. There are nine kinds. Each referrer has one
//! canonical `referrer_id` text, which is what the edge, fence and cleanup
//! tables store; [`ArtifactReferrer::decode`] refuses every stored pair whose
//! text is not exactly that rendering, and a store classifies the refusal
//! with [`ArtifactReferrerError::into_store_error`] rather than skipping the
//! row: a kind a newer build wrote is `Incompatible(UnknownVocabulary)`,
//! anything else `StoredDataCorrupt` (ADR 0115 §5).
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

/// The referrer labels and their canonical id encodings at the 1.0 cut.
///
/// version_guard(
///     roots(
///         ArtifactReferrerKind, ArtifactReferrer, StoredReferrer, FrameEnvironmentId,
///         SubscriptionRevisionId, HostArtifactPin, UploadReferrerId, AttachmentUploadId,
///     ),
///     roots(path = "crates/lash-core-store/src/process_identity.rs", StartKey),
///     roots(path = "crates/lash-core-store/src/session_identity.rs", FrameNodeId),
///     roots(path = "crates/lash-sansio/src/effect_identity.rs", EffectJournalIdentity),
///     items(
///         ALL, as_str, parse, canonical_id, decode, HOST_PIN_PREFIX, HOST_PIN_HEX_LEN,
///         UPLOAD_PREFIX, UPLOAD_HEX_LEN, try_from, decode_journal_identity, json_text, json_parse,
///     ),
///     items(path = "crates/lash-core-store/src/process_identity.rs", parse_rendered),
///     file(
///         path = "crates/lash-sansio/src/identity.rs",
///         cover("string_identity!", SessionId, ProcessId),
///     ),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "coexist"
/// format_outside_manifest = "referrer vocabulary is checked when edge and fence rows are decoded"
pub const ARTIFACT_REFERRER_KINDS_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) declares the vocabulary one ahead
/// and still writes only the current kinds: no new kind is written before
/// finalize (ADR 0115 §5). A label a later build writes reaches N as an
/// unknown kind, which N refuses typed and never counts as absent.
#[cfg(feature = "synthetic-next")]
/// version_surface = "coexist"
/// format_outside_manifest = "referrer vocabulary is checked when edge and fence rows are decoded"
pub const ARTIFACT_REFERRER_KINDS_VERSION: u32 = 2;

/// The label vocabulary version 2 adds: what the stores' laws write as a
/// later build's referrer kind. No build of this window names it, so every
/// decode refuses it as `Incompatible(UnknownVocabulary)`.
pub const SYNTHETIC_NEXT_REFERRER_KIND: &str = "synthetic_next";

/// The byte family a referrer claims.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReferrerStore {
    Artifact,
    Attachment,
}
impl ReferrerStore {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Artifact => "artifact",
            Self::Attachment => "attachment",
        }
    }
}
impl fmt::Display for ReferrerStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The referrer kinds, as the `referrer_kind` column stores them.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactReferrerKind {
    FrameEnvironment,
    ProcessRecord,
    SubscriptionRevision,
    Start,
    StartInput,
    Execution,
    HostPin,
    Session,
    Upload,
}

impl ArtifactReferrerKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 9] = [
        Self::FrameEnvironment,
        Self::ProcessRecord,
        Self::SubscriptionRevision,
        Self::Start,
        Self::StartInput,
        Self::Execution,
        Self::HostPin,
        Self::Session,
        Self::Upload,
    ];

    /// `frame_environment`, `process_record`, `subscription_revision`,
    /// `start`, `start_input`, `execution`, `host_pin`, `session`, `upload`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FrameEnvironment => "frame_environment",
            Self::ProcessRecord => "process_record",
            Self::SubscriptionRevision => "subscription_revision",
            Self::Start => "start",
            Self::StartInput => "start_input",
            Self::Execution => "execution",
            Self::HostPin => "host_pin",
            Self::Session => "session",
            Self::Upload => "upload",
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

    /// Whether every acquisition requires a guard (ADR 0113 §2.4).
    /// Frames permit both ordinary claims and guards for prepared successors.
    #[must_use]
    pub const fn requires_guard(self) -> bool {
        matches!(
            self,
            Self::Execution
                | Self::Start
                | Self::StartInput
                | Self::SubscriptionRevision
                | Self::Upload
        )
    }
    /// Whether this referrer may hold immutable artifacts.
    #[must_use]
    pub const fn holds_artifacts(self) -> bool {
        matches!(
            self,
            Self::FrameEnvironment
                | Self::ProcessRecord
                | Self::SubscriptionRevision
                | Self::Start
                | Self::Execution
                | Self::HostPin
        )
    }

    /// SQL generated from the owning kind predicate.
    #[must_use]
    pub fn predicate_sql(column: &str, accepts: fn(Self) -> bool) -> String {
        let labels = Self::ALL
            .into_iter()
            .filter(|kind| accepts(*kind))
            .map(|kind| format!("'{}'", kind.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{column} IN ({labels})")
    }

    /// Whether this referrer may hold external attachment bytes.
    #[must_use]
    pub const fn holds_attachments(self) -> bool {
        matches!(
            self,
            Self::Session | Self::Upload | Self::Execution | Self::StartInput | Self::ProcessRecord
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
    /// One starter's input staging, independent of earlier uses of the key.
    StartInput {
        start_key: StartKey,
        starter: EffectJournalIdentity,
    },
    Execution(EffectJournalIdentity),
    HostPin(HostArtifactPin),
    Session(SessionId),
    Upload(UploadReferrerId),
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
            Self::StartInput { .. } => ArtifactReferrerKind::StartInput,
            Self::Execution(_) => ArtifactReferrerKind::Execution,
            Self::HostPin(_) => ArtifactReferrerKind::HostPin,
            Self::Session(_) => ArtifactReferrerKind::Session,
            Self::Upload(_) => ArtifactReferrerKind::Upload,
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
            Self::StartInput { start_key, starter } => {
                json_text(&(start_key.as_str(), starter.key()))
            }
            Self::Execution(journal) => journal.key().to_owned(),
            Self::HostPin(pin) => pin.as_str().to_owned(),
            Self::Session(id) => id.to_string(),
            Self::Upload(id) => json_text(&(id.session_id.as_str(), id.upload_id.as_str())),
        }
    }

    /// Typed decode of a stored pair. Refuses an unknown kind, an empty id,
    /// an id that does not decode, and an id whose re-encoding differs from
    /// the stored text. Stores classify a refusal with
    /// [`ArtifactReferrerError::into_store_error`].
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
            ArtifactReferrerKind::StartInput => {
                let (start_key, starter): (String, String) = json_parse(kind, id)?;
                Self::StartInput {
                    start_key: StartKey::parse_rendered(&start_key)
                        .map_err(|error| malformed(kind, error.to_string()))?,
                    starter: decode_journal_identity(&starter)
                        .map_err(|detail| malformed(kind, detail))?,
                }
            }
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
            ArtifactReferrerKind::Session => {
                let session = SessionId::from(id);
                crate::store::validate_session_id(&session)
                    .map_err(|error| malformed(kind, error.to_string()))?;
                Self::Session(session)
            }
            ArtifactReferrerKind::Upload => {
                let (session, upload): (String, String) = json_parse(kind, id)?;
                let session = SessionId::from(session);
                crate::store::validate_session_id(&session)
                    .map_err(|error| malformed(kind, error.to_string()))?;
                Self::Upload(UploadReferrerId::new(
                    session,
                    AttachmentUploadId::try_from(upload)?,
                ))
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

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
struct StoredReferrer<'a> {
    #[schemars(with = "ArtifactReferrerKind")]
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

/// version_surface = "coexist"
/// version_guard(items(HOST_PIN_PREFIX, mint, try_from))
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

/// One session upload staging identity and its parent session.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UploadReferrerId {
    session_id: SessionId,
    upload_id: AttachmentUploadId,
}
impl UploadReferrerId {
    #[must_use]
    pub fn new(session_id: SessionId, upload_id: AttachmentUploadId) -> Self {
        Self {
            session_id,
            upload_id,
        }
    }
    #[must_use]
    pub fn mint(session_id: SessionId) -> Self {
        Self::new(session_id, AttachmentUploadId::mint())
    }
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    #[must_use]
    pub fn upload_id(&self) -> &AttachmentUploadId {
        &self.upload_id
    }
}

/// version_surface = "coexist"
/// version_guard(items(UPLOAD_PREFIX, mint, try_from))
const UPLOAD_PREFIX: &str = "upload:v1:";
const UPLOAD_HEX_LEN: usize = 32;

/// An opaque, releasable host referrer. Only [`mint`](Self::mint) makes a new
/// one. Once released, an upload id is fenced for good: a host that wants to publish
/// again mints a fresh pin.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AttachmentUploadId(String);

impl AttachmentUploadId {
    /// `upload:v1:` followed by 32 lowercase hex digits of a random v4 UUID.
    #[must_use]
    pub fn mint() -> Self {
        Self(format!("{UPLOAD_PREFIX}{}", uuid::Uuid::new_v4().simple()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AttachmentUploadId {
    type Error = ArtifactReferrerError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let kind = ArtifactReferrerKind::Upload;
        if text.is_empty() {
            return Err(ArtifactReferrerError::EmptyId {
                kind: kind.as_str(),
            });
        }
        let hex = text.strip_prefix(UPLOAD_PREFIX).ok_or_else(|| {
            malformed(kind, format!("an upload id starts with `{UPLOAD_PREFIX}`"))
        })?;
        let uuid = (hex.len() == UPLOAD_HEX_LEN
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then(|| uuid::Uuid::parse_str(hex).ok())
        .flatten()
        .ok_or_else(|| {
            malformed(
                kind,
                format!("an upload id ends with {UPLOAD_HEX_LEN} lowercase hex digits"),
            )
        })?;
        if uuid.get_version_num() != 4 {
            return Err(malformed(kind, "an upload id is a v4 UUID"));
        }
        Ok(Self(text))
    }
}

impl From<AttachmentUploadId> for String {
    fn from(pin: AttachmentUploadId) -> Self {
        pin.0
    }
}

impl fmt::Display for AttachmentUploadId {
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

impl ArtifactReferrerError {
    /// How a store reports a stored pair it refuses, for the row
    /// `record_kind` names (ADR 0115 §5). A kind this build has no name for
    /// was written by a newer build: it is `Incompatible(UnknownVocabulary)`,
    /// so no pass counts the row as absent or corrupt. Any other refusal is
    /// `StoredDataCorrupt`.
    #[must_use]
    pub fn into_store_error(self, record_kind: &'static str) -> crate::store::StoreError {
        match self {
            Self::UnknownKind(label) => crate::store::StoreError::Incompatible {
                refusal: crate::compat::CompatRefusal::UnknownVocabulary {
                    surface: "artifact referrer kind".to_owned(),
                    label,
                },
            },
            other => crate::store::StoreError::StoredDataCorrupt {
                record_kind,
                message: other.to_string(),
            },
        }
    }
}

/// A guard and the durable reader whose first acquisition it protects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReferrerGuard {
    Frame {
        frame: FrameEnvironmentId,
        creator: EffectJournalIdentity,
    },
    Journal(EffectJournalIdentity),
    Start {
        start_key: StartKey,
        starter: EffectJournalIdentity,
    },
    StartInput {
        start_key: StartKey,
        starter: EffectJournalIdentity,
    },
    SubscriptionRevision {
        revision: SubscriptionRevisionId,
        creator: EffectJournalIdentity,
    },
    Upload {
        upload: UploadReferrerId,
        expires_at_ms: u64,
    },
    SessionGraphRetired(SessionId),
}

impl ReferrerGuard {
    #[must_use]
    pub fn referrer(&self) -> ArtifactReferrer {
        match self {
            Self::Frame { frame, .. } => ArtifactReferrer::FrameEnvironment(frame.clone()),
            Self::Journal(journal) => ArtifactReferrer::Execution(journal.clone()),
            Self::Start { start_key, .. } => ArtifactReferrer::Start(start_key.clone()),
            Self::StartInput { start_key, starter } => ArtifactReferrer::StartInput {
                start_key: start_key.clone(),
                starter: starter.clone(),
            },
            Self::SubscriptionRevision { revision, .. } => {
                ArtifactReferrer::SubscriptionRevision(revision.clone())
            }
            Self::Upload { upload, .. } => ArtifactReferrer::Upload(upload.clone()),
            Self::SessionGraphRetired(session) => ArtifactReferrer::Session(session.clone()),
        }
    }
}

/// A claim cannot name a guard belonging to another reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferrerClaim(ReferrerClaimBody);

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReferrerClaimBody {
    Unguarded(ArtifactReferrer),
    Guarded(ReferrerGuard),
}

impl ReferrerClaim {
    /// # Errors
    /// Refuses a reader that requires a guard on acquisition.
    pub fn unguarded(referrer: ArtifactReferrer) -> Result<Self, ArtifactReferrerError> {
        if referrer.kind().requires_guard() {
            return Err(malformed(
                referrer.kind(),
                "a guarded referrer is claimed with its guard",
            ));
        }
        Ok(Self(ReferrerClaimBody::Unguarded(referrer)))
    }

    #[must_use]
    pub fn guarded(guard: ReferrerGuard) -> Self {
        Self(ReferrerClaimBody::Guarded(guard))
    }

    #[must_use]
    pub fn referrer(&self) -> ArtifactReferrer {
        match &self.0 {
            ReferrerClaimBody::Unguarded(referrer) => referrer.clone(),
            ReferrerClaimBody::Guarded(guard) => guard.referrer(),
        }
    }

    #[must_use]
    pub fn guard_cleanup(&self) -> Option<ArtifactCleanup> {
        match &self.0 {
            ReferrerClaimBody::Unguarded(_) => None,
            ReferrerClaimBody::Guarded(guard) => Some(ArtifactCleanup::Await(guard.clone())),
        }
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
    /// The immutable process-definition descriptors, keyed by their
    /// `ProcessDefinitionId` (ADR 0113 §3.6). A collectible artifact like the
    /// others: a descriptor lives exactly as long as some referrer holds it.
    ProcessDefinition,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactCleanup {
    Ended {
        referrer: ArtifactReferrer,
        carries: Vec<ArtifactCarry>,
        gate: Option<EffectJournalIdentity>,
    },
    Await(ReferrerGuard),
}

/// The row key supplies the reader; the JSON contains only the plan body.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "plan", rename_all = "snake_case", deny_unknown_fields)]
enum StoredCleanupBody {
    Ended {
        carries: Vec<ArtifactCarry>,
        #[serde(with = "optional_journal_identity")]
        gate: Option<EffectJournalIdentity>,
    },
    AwaitFrame {
        #[serde(with = "journal_identity")]
        creator: EffectJournalIdentity,
    },
    AwaitJournal,
    AwaitUploadExpiry {
        expires_at_ms: u64,
    },
    AwaitSessionGraphRetired,
    AwaitStart {
        #[serde(with = "journal_identity")]
        starter: EffectJournalIdentity,
    },
    AwaitStartInput,
    AwaitSubscriptionRevision {
        #[serde(with = "journal_identity")]
        creator: EffectJournalIdentity,
    },
}

impl ArtifactCleanup {
    #[must_use]
    pub fn ended(
        referrer: ArtifactReferrer,
        carries: Vec<ArtifactCarry>,
        gate: Option<EffectJournalIdentity>,
    ) -> Self {
        Self::Ended {
            referrer,
            carries,
            gate,
        }
    }

    #[must_use]
    pub fn referrer(&self) -> ArtifactReferrer {
        match self {
            Self::Ended { referrer, .. } => referrer.clone(),
            Self::Await(guard) => guard.referrer(),
        }
    }

    #[must_use]
    pub const fn is_ended(&self) -> bool {
        matches!(self, Self::Ended { .. })
    }

    #[must_use]
    pub fn gate(&self) -> Option<&EffectJournalIdentity> {
        match self {
            Self::Ended { gate, .. } => gate.as_ref(),
            Self::Await(_) => None,
        }
    }

    /// # Errors
    /// Returns the JSON encoder's error.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let body = match self {
            Self::Ended { carries, gate, .. } => StoredCleanupBody::Ended {
                carries: carries.clone(),
                gate: gate.clone(),
            },
            Self::Await(guard) => match guard {
                ReferrerGuard::Frame { creator, .. } => StoredCleanupBody::AwaitFrame {
                    creator: creator.clone(),
                },
                ReferrerGuard::Journal(_) => StoredCleanupBody::AwaitJournal,
                ReferrerGuard::Start { starter, .. } => StoredCleanupBody::AwaitStart {
                    starter: starter.clone(),
                },
                ReferrerGuard::StartInput { .. } => StoredCleanupBody::AwaitStartInput,
                ReferrerGuard::SubscriptionRevision { creator, .. } => {
                    StoredCleanupBody::AwaitSubscriptionRevision {
                        creator: creator.clone(),
                    }
                }
                ReferrerGuard::Upload { expires_at_ms, .. } => {
                    StoredCleanupBody::AwaitUploadExpiry {
                        expires_at_ms: *expires_at_ms,
                    }
                }
                ReferrerGuard::SessionGraphRetired(_) => {
                    StoredCleanupBody::AwaitSessionGraphRetired
                }
            },
        };
        serde_json::to_string(&body)
    }

    /// # Errors
    /// An invalid body or a guard incompatible with the row's reader is corrupt.
    pub fn from_json(text: &str, referrer: &ArtifactReferrer) -> Result<Self, crate::StoreError> {
        let corrupt = |message: String| crate::StoreError::StoredDataCorrupt {
            record_kind: "artifact_cleanup_obligation",
            message,
        };
        let body: StoredCleanupBody =
            serde_json::from_str(text).map_err(|error| corrupt(error.to_string()))?;
        let guard = match (body, referrer) {
            (StoredCleanupBody::Ended { carries, gate }, _) => {
                return Ok(Self::Ended {
                    referrer: referrer.clone(),
                    carries,
                    gate,
                });
            }
            (
                StoredCleanupBody::AwaitFrame { creator },
                ArtifactReferrer::FrameEnvironment(frame),
            ) => ReferrerGuard::Frame {
                frame: frame.clone(),
                creator,
            },
            (StoredCleanupBody::AwaitJournal, ArtifactReferrer::Execution(journal)) => {
                ReferrerGuard::Journal(journal.clone())
            }
            (StoredCleanupBody::AwaitStart { starter }, ArtifactReferrer::Start(start_key)) => {
                ReferrerGuard::Start {
                    start_key: start_key.clone(),
                    starter,
                }
            }
            (
                StoredCleanupBody::AwaitStartInput,
                ArtifactReferrer::StartInput { start_key, starter },
            ) => ReferrerGuard::StartInput {
                start_key: start_key.clone(),
                starter: starter.clone(),
            },
            (
                StoredCleanupBody::AwaitSubscriptionRevision { creator },
                ArtifactReferrer::SubscriptionRevision(revision),
            ) => ReferrerGuard::SubscriptionRevision {
                revision: revision.clone(),
                creator,
            },
            (
                StoredCleanupBody::AwaitUploadExpiry { expires_at_ms },
                ArtifactReferrer::Upload(upload),
            ) => ReferrerGuard::Upload {
                upload: upload.clone(),
                expires_at_ms,
            },
            (StoredCleanupBody::AwaitSessionGraphRetired, ArtifactReferrer::Session(session)) => {
                ReferrerGuard::SessionGraphRetired(session.clone())
            }
            (body, _) => {
                return Err(corrupt(format!(
                    "cleanup guard `{}` cannot protect `{referrer}`",
                    body.label()
                )));
            }
        };
        Ok(Self::Await(guard))
    }
}

impl StoredCleanupBody {
    fn label(&self) -> &'static str {
        match self {
            Self::Ended { .. } => "ended",
            Self::AwaitFrame { .. } => "await_frame",
            Self::AwaitJournal => "await_journal",
            Self::AwaitUploadExpiry { .. } => "await_upload_expiry",
            Self::AwaitSessionGraphRetired => "await_session_graph_retired",
            Self::AwaitStart { .. } => "await_start",
            Self::AwaitStartInput => "await_start_input",
            Self::AwaitSubscriptionRevision { .. } => "await_subscription_revision",
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

impl schemars::JsonSchema for ArtifactReferrer {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ArtifactReferrer".into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        StoredReferrer::json_schema(generator)
    }
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
            ArtifactReferrer::StartInput {
                start_key: start_key(),
                starter: journal(),
            },
            ArtifactReferrer::Execution(journal()),
            ArtifactReferrer::HostPin(HostArtifactPin::mint()),
            ArtifactReferrer::Session(SessionId::from("s-1")),
            ArtifactReferrer::Upload(UploadReferrerId::mint(SessionId::from("s-1"))),
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
    fn start_input_claim_is_tied_to_its_start_key_and_starter() {
        let first = ReferrerGuard::StartInput {
            start_key: start_key(),
            starter: journal(),
        };
        let next = ReferrerGuard::StartInput {
            start_key: start_key(),
            starter: ExecutionScope::turn("session", "next-turn")
                .journal_identity()
                .expect("journal"),
        };
        assert_ne!(
            first.referrer().canonical_id(),
            next.referrer().canonical_id()
        );
        assert!(first.referrer().kind().holds_attachments());
        assert!(!first.referrer().kind().holds_artifacts());
        assert_eq!(
            ReferrerClaim::guarded(first.clone()).referrer(),
            first.referrer()
        );
        let json = ArtifactCleanup::Await(first.clone())
            .to_json()
            .expect("body");
        assert_eq!(json, r#"{"plan":"await_start_input"}"#);
        assert!(ReferrerClaim::unguarded(next.referrer()).is_err());
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
    fn an_unknown_kind_is_incompatible_and_a_bad_id_is_corrupt() {
        let unknown = ArtifactReferrer::decode(SYNTHETIC_NEXT_REFERRER_KIND, "x")
            .expect_err("no build of this window names the next kind");
        assert!(matches!(
            unknown.into_store_error("artifact_cleanup_obligation"),
            crate::store::StoreError::Incompatible {
                refusal: crate::compat::CompatRefusal::UnknownVocabulary { ref label, .. }
            } if label == SYNTHETIC_NEXT_REFERRER_KIND
        ));
        let malformed = ArtifactReferrer::decode("host_pin", "").expect_err("an empty id");
        assert!(matches!(
            malformed.into_store_error("artifact_cleanup_obligation"),
            crate::store::StoreError::StoredDataCorrupt {
                record_kind: "artifact_cleanup_obligation",
                ..
            }
        ));
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
        for referrer in every_kind() {
            assert_eq!(
                ReferrerClaim::unguarded(referrer.clone()).is_ok(),
                !referrer.kind().requires_guard()
            );
        }
        let guard = ReferrerGuard::Start {
            start_key: start_key(),
            starter: journal(),
        };
        let claim = ReferrerClaim::guarded(guard.clone());
        assert_eq!(claim.referrer(), guard.referrer());
        assert_eq!(claim.guard_cleanup(), Some(ArtifactCleanup::Await(guard)));
    }

    #[test]
    fn cleanup_decoder_refuses_a_guard_for_another_referrer_kind() {
        let referrer = ArtifactReferrer::HostPin(HostArtifactPin::mint());
        let text = serde_json::json!({
            "referrer": referrer,
            "plan": { "plan": "await_journal" },
            "gate": null,
        })
        .to_string();
        assert!(ArtifactCleanup::from_json(&text, &referrer).is_err());
    }

    #[test]
    fn cleanup_bodies_round_trip_and_check_their_row() {
        let referrers = every_kind();
        let guards = [
            ReferrerGuard::Frame {
                frame: FrameEnvironmentId::new("s".into(), FrameNodeId::new("f").expect("frame")),
                creator: journal(),
            },
            ReferrerGuard::Journal(journal()),
            ReferrerGuard::Start {
                start_key: start_key(),
                starter: journal(),
            },
            ReferrerGuard::StartInput {
                start_key: start_key(),
                starter: journal(),
            },
            ReferrerGuard::SubscriptionRevision {
                revision: SubscriptionRevisionId::new("sub".into(), "inc".into(), 1)
                    .expect("revision"),
                creator: journal(),
            },
            ReferrerGuard::Upload {
                upload: UploadReferrerId::mint("s".into()),
                expires_at_ms: 10,
            },
            ReferrerGuard::SessionGraphRetired("s".into()),
        ];
        for guard in guards {
            let cleanup = ArtifactCleanup::Await(guard);
            let json = cleanup.to_json().expect("encode");
            let value: serde_json::Value = serde_json::from_str(&json).expect("JSON");
            assert!(value.get("referrer").is_none());
            assert!(value.get("gate").is_none());
            assert_eq!(
                ArtifactCleanup::from_json(&json, &cleanup.referrer()).expect("decode"),
                cleanup
            );
            for referrer in &referrers {
                assert_eq!(
                    ArtifactCleanup::from_json(&json, referrer).is_ok(),
                    referrer.kind() == cleanup.referrer().kind()
                );
            }
        }
        let ended = ArtifactCleanup::ended(
            referrers[0].clone(),
            vec![ArtifactCarry {
                artifact: ArtifactName {
                    store: ArtifactStoreId::Engine("lashlang".into()),
                    artifact_ref: "mod-1".into(),
                },
                to: referrers[1].clone(),
            }],
            Some(journal()),
        );
        let json = ended.to_json().expect("encode");
        assert_eq!(
            ArtifactCleanup::from_json(&json, &ended.referrer()).expect("decode"),
            ended
        );
        assert_eq!(
            ArtifactCleanup::from_json(&json, &referrers[1])
                .expect("row owns identity")
                .referrer(),
            referrers[1]
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&json)
                .expect("JSON")
                .get("referrer")
                .is_none()
        );
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
