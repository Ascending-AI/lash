//! Store→engine delivery obligations (ADR 0109 §1).
//!
//! A ledger row that owes the engine an effect carries the obligation
//! columns; the row is the obligation. A producer arms it in the transaction
//! that makes the effect owed, a relay delivers it immediately and retries
//! due rows through a partial index, and a row that cannot be delivered
//! stalls with a typed reason until an operator re-arms it. Nothing here is
//! engine-specific: the engine half is `lash_core`'s relay.

use std::num::NonZeroUsize;

use crate::artifact_referrer::ArtifactReferrer;

use super::StoreError;

/// The obligation state, kind, key, and stall labels written at the 1.0 cut.
///
/// version_guard(
///     items(ALL, from_label, label, key_column_types, decode_label, columns, decode, as_str),
///     items(
///         path = "crates/lash-core-store/src/artifact_referrer.rs", canonical_id, decode, as_str,
///         parse,
///     ),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_outside_manifest = "obligation vocabulary is checked by each ledger's typed row decoder"
pub const OBLIGATION_LEDGER_VOCABULARY_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) declares the vocabulary one ahead
/// and still writes only N's states, kinds, keys and stall reasons while `F`
/// is N's epoch (ADR 0115 §5). A row a later build writes with a label N
/// does not know stays outstanding under N, stalled `undecodable`.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_outside_manifest = "obligation vocabulary is checked by each ledger's typed row decoder"
pub const OBLIGATION_LEDGER_VOCABULARY_VERSION: u32 = 2;

/// Which ledger an obligation lives on. Its [`label`](Self::label) is the
/// metric label and the drain-status key.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ObligationKind {
    /// An ended or guarded artifact referrer owes its cleanup (ADR 0113 §2.5).
    ArtifactCleanup,
}

impl ObligationKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 1] = [Self::ArtifactCleanup];

    /// Decode a stored kind without treating a newer build's label as corrupt.
    pub fn from_label(label: &str) -> Result<Self, StoreError> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.label() == label)
            .ok_or_else(|| unknown_vocabulary("obligation kind", label))
    }

    /// The stable label metrics and drain status report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ArtifactCleanup => "artifact_cleanup",
        }
    }

    /// The SQL types of this kind's key columns, in the order a claim's and
    /// a stalled listing's projection ends with them.
    #[must_use]
    pub const fn key_column_types(self) -> &'static [KeyColumnType] {
        match self {
            Self::ArtifactCleanup => &[KeyColumnType::Text, KeyColumnType::Text],
        }
    }
}

/// The SQL type of one obligation key column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyColumnType {
    Text,
    Integer,
}

/// One obligation key column's value, as a backend binds and reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyColumn {
    Text(String),
    Integer(i64),
}

impl std::fmt::Display for ObligationKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// The row an obligation lives on: one variant per [`ObligationKind`],
/// carrying that ledger's primary key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ObligationKey {
    /// An `artifact_cleanup_obligations` row: the referrer's stored pair.
    ArtifactCleanup { referrer: ArtifactReferrer },
}

impl ObligationKey {
    /// The stable id of this owning row. Each key part is byte-length-prefixed,
    /// so delimiters and Unicode cannot make distinct keys collide.
    #[must_use]
    pub fn id(&self) -> ObligationId {
        let parts = self.columns().into_iter().map(|column| match column {
            KeyColumn::Text(text) => text,
            KeyColumn::Integer(integer) => integer.to_string(),
        });
        let mut id = self.kind().label().to_owned();
        for part in parts {
            id.push(':');
            id.push_str(&part.len().to_string());
            id.push(':');
            id.push_str(&part);
        }
        ObligationId::new(id)
    }

    /// Decode a row whose kind label came from storage. A foreign kind leaves
    /// the row addressable by its obligation id but cannot be delivered here.
    pub fn decode_label(
        kind_label: &str,
        columns: Vec<KeyColumn>,
    ) -> Result<Self, UndecodableObligation> {
        let kind = ObligationKind::from_label(kind_label)
            .map_err(UndecodableObligation::of_store_error)?;
        Self::decode(kind, columns)
    }

    /// The ledger this key addresses.
    #[must_use]
    pub const fn kind(&self) -> ObligationKind {
        match self {
            Self::ArtifactCleanup { .. } => ObligationKind::ArtifactCleanup,
        }
    }
}

impl ObligationKey {
    /// The key's columns, in the ledger's key order.
    #[must_use]
    pub fn columns(&self) -> Vec<KeyColumn> {
        match self {
            Self::ArtifactCleanup { referrer } => vec![
                KeyColumn::Text(referrer.kind().as_str().to_owned()),
                KeyColumn::Text(referrer.canonical_id()),
            ],
        }
    }

    /// The key of `kind` read back from `columns`, in the ledger's key order.
    ///
    /// # Errors
    ///
    /// A column set this build cannot name a row by — the wrong arity or
    /// type, or a process id no registrar minted — is undecodable: the relay
    /// stalls the row rather than failing its page.
    pub fn decode(
        kind: ObligationKind,
        columns: Vec<KeyColumn>,
    ) -> Result<Self, UndecodableObligation> {
        let mut columns = columns.into_iter();
        Ok(match kind {
            ObligationKind::ArtifactCleanup => {
                let referrer_kind = next_text(&mut columns, kind, "referrer_kind")?;
                let referrer_id = next_text(&mut columns, kind, "referrer_id")?;
                Self::ArtifactCleanup {
                    referrer: ArtifactReferrer::decode(&referrer_kind, &referrer_id).map_err(
                        |error| {
                            UndecodableObligation::of_store_error(
                                error.into_store_error("artifact_cleanup_obligation"),
                            )
                        },
                    )?,
                }
            }
        })
    }
}

/// The next key column as text, or why it is not.
fn next_text(
    columns: &mut impl Iterator<Item = KeyColumn>,
    kind: ObligationKind,
    name: &str,
) -> Result<String, UndecodableObligation> {
    match columns.next() {
        Some(KeyColumn::Text(value)) => Ok(value),
        other => Err(UndecodableObligation::malformed(format!(
            "{kind} obligation key column `{name}` is {other:?}, not text"
        ))),
    }
}

/// The stable id of one obligation, derived from its owning typed key.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ObligationId(String);

impl ObligationId {
    /// The id stored as `text`.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The id as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ObligationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The token one claim stamps; every settling write compares it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClaimToken(String);

impl ClaimToken {
    /// A fresh token: a claimant that cannot come back to its claim (a
    /// relay pass, a producer's in-process attempt).
    #[must_use]
    pub fn mint() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    /// The token `claimant` claims obligation `id` under: the same whenever
    /// the same claimant derives it, so a claimant that reruns after an
    /// interruption (a journaled step whose answer was lost) finds its own
    /// claim again through [`ObligationLedger::claim`]. `claimant` names one
    /// claimant for the life of the obligation — a journaled step's stable
    /// identity — and never a pass or a process, so a claim another relay
    /// retook, which carries that relay's minted token, is never its own.
    #[must_use]
    pub fn derive(claimant: &str, id: &ObligationId) -> Self {
        Self(format!("{claimant}@{id}"))
    }

    /// The token stored as `text`.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The token as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where an armed obligation stands (`obligation_state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObligationState {
    /// Waiting for its next attempt at `obligation_due_at_ms`.
    Due,
    /// A relay holds it until `obligation_due_at_ms`.
    Claimed,
    /// The engine accepted it.
    Delivered,
    /// It will not be retried until re-armed.
    Stalled,
}

/// Where an armed obligation stands, and how many claims it has taken since
/// it was armed or re-armed (`obligation_attempts`): a kind whose consumer
/// settles it (ingress) asks the engine under the attempt of its current
/// claim, so a reader follows that ask by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObligationStanding {
    pub state: ObligationState,
    pub attempts: u32,
}

impl ObligationState {
    /// Every state, in declaration order.
    pub const ALL: [Self; 4] = [Self::Due, Self::Claimed, Self::Delivered, Self::Stalled];

    /// The stored label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Due => "due",
            Self::Claimed => "claimed",
            Self::Delivered => "delivered",
            Self::Stalled => "stalled",
        }
    }

    /// The state stored as `label`.
    ///
    /// # Errors
    ///
    /// A label this build does not know is refused as incompatible.
    pub fn from_label(label: &str) -> Result<Self, StoreError> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == label)
            .ok_or_else(|| unknown_vocabulary("obligation state", label))
    }
}

/// Why an obligation stalled (`obligation_stall_reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallReason {
    /// Its retryable failures reached the kind's attempt ceiling.
    AttemptsExhausted,
    /// The engine refused it for good.
    Refused,
    /// This build cannot decode the row.
    Undecodable,
}

impl StallReason {
    /// Every reason, in declaration order.
    pub const ALL: [Self; 3] = [Self::AttemptsExhausted, Self::Refused, Self::Undecodable];

    /// The stored label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AttemptsExhausted => "attempts_exhausted",
            Self::Refused => "refused",
            Self::Undecodable => "undecodable",
        }
    }

    /// The reason stored as `label`.
    ///
    /// # Errors
    ///
    /// A label this build does not know is refused as incompatible.
    pub fn from_label(label: &str) -> Result<Self, StoreError> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == label)
            .ok_or_else(|| unknown_vocabulary("obligation stall reason", label))
    }
}

fn unknown_vocabulary(surface: &str, label: &str) -> StoreError {
    StoreError::Incompatible {
        refusal: crate::compat::CompatRefusal::UnknownVocabulary {
            surface: surface.to_owned(),
            label: label.to_owned(),
        },
    }
}

/// Why a delivery attempt did not deliver, as its obligation row and the
/// intent it refused retain it: the typed code beside the message, so a
/// reader never recovers the cause from the text.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeliveryError {
    pub code: crate::RuntimeErrorCode,
    pub message: String,
}

impl DeliveryError {
    /// The cause `code`, worded `message`.
    #[must_use]
    pub fn new(code: crate::RuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The same cause, its message led by what the attempt was doing.
    #[must_use]
    pub fn in_context(mut self, context: impl std::fmt::Display) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl From<StoreError> for DeliveryError {
    fn from(error: StoreError) -> Self {
        Self {
            code: error.runtime_code(),
            message: error.to_string(),
        }
    }
}

/// A row this build could not decode into its key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndecodableObligation {
    /// The store's code for why: an unknown vocabulary is the store's
    /// compatibility refusal, a malformed key its corruption.
    pub code: crate::RuntimeErrorCode,
    pub detail: String,
}

impl UndecodableObligation {
    /// A key column set no build of this vocabulary writes.
    #[must_use]
    pub fn malformed(detail: String) -> Self {
        Self {
            code: crate::RuntimeErrorCode::RuntimeStoreCorrupt,
            detail,
        }
    }

    /// The store error that refused the row's label or key.
    #[must_use]
    pub fn of_store_error(error: StoreError) -> Self {
        let DeliveryError { code, message } = DeliveryError::from(error);
        Self {
            code,
            detail: message,
        }
    }

    /// The stall this row settles with.
    #[must_use]
    pub fn into_delivery_error(self) -> DeliveryError {
        DeliveryError::new(self.code, self.detail)
    }
}

/// One claimed obligation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedObligation {
    pub id: ObligationId,
    pub token: ClaimToken,
    /// Claims taken since the row was armed or re-armed, this one included.
    pub attempts: u32,
    /// The row it lives on, or why this build cannot name it.
    pub key: Result<ObligationKey, UndecodableObligation>,
}

/// How a claim settles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObligationSettlement {
    /// The engine accepted it.
    Delivered,
    /// Try again at `due_at_ms`.
    Retry {
        due_at_ms: u64,
        error: DeliveryError,
    },
    /// Stop until re-armed.
    Stall {
        reason: StallReason,
        error: DeliveryError,
    },
    /// Not owed yet: back to `due` at `due_at_ms` with attempts reset to 0.
    Defer { due_at_ms: u64 },
}

/// Whether a settling write applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettleOutcome {
    Applied,
    /// The claim token no longer matches: another relay retook the row, or
    /// the delivery's own transaction settled it.
    ClaimLost,
}

/// A stalled obligation, as an operator lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StalledObligation {
    pub kind: ObligationKind,
    pub id: ObligationId,
    /// The row it lives on, or why this build cannot name it.
    pub key: Result<ObligationKey, UndecodableObligation>,
    pub reason: StallReason,
    pub attempts: u32,
    pub last_error: Option<DeliveryError>,
    pub stalled_at_ms: u64,
}

/// One kind's obligation ledger: the store half of the relay (ADR 0109 §1.3).
///
/// Every write is a compare-and-set on the row's state and, while claimed,
/// its claim token, so two relays that overlap (two leaders, or two
/// deployments on PostgreSQL) never both settle one claim.
#[async_trait::async_trait]
pub trait ObligationLedger: Send + Sync {
    /// The kind this ledger holds.
    fn kind(&self) -> ObligationKind;

    /// Claim at most `limit` rows due at `now_ms` (a lapsed claim included),
    /// oldest due first, each held for `claim_ttl_ms`. A row whose key does
    /// not decode is returned claimed with `key: Err`, never failing the
    /// page.
    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError>;

    /// Claim `id` under the caller's `token` for a delivery it runs itself,
    /// held for `claim_ttl_ms`: a `due` row at any time (a producer's own
    /// attempt does not wait for a backoff), or a claim `token` already
    /// holds. The second is a claimant re-deriving its own claim after an
    /// interruption took the answer of the claim that stamped it (ADR 0109
    /// §1.3): the claim's expiry is refreshed and its attempt count kept.
    /// `None` otherwise: the row is delivered, stalled, or claimed under
    /// another token, so a claim some other relay retook stays its own.
    async fn claim(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError>;

    /// Settle `id`'s claim under `token`.
    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError>;

    /// Put stalled `id` back to `due` at `now_ms` with its attempts reset.
    /// `false` if it is not stalled.
    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError>;

    /// Stalled obligations after `after`, in id order.
    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError>;

    /// How many obligations are stalled.
    async fn count_stalled(&self) -> Result<u64, StoreError>;

    /// Where `id` stands, or `None` if no row carries it.
    async fn standing(&self, id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError>;

    /// The state of `id`, or `None` if no row carries it.
    async fn state(&self, id: &ObligationId) -> Result<Option<ObligationState>, StoreError> {
        Ok(self.standing(id).await?.map(|standing| standing.state))
    }
}

impl crate::store::DurableRecord for ObligationKind {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::obligation::OBLIGATION_LEDGER_VOCABULARY_VERSION);
}

impl crate::store::DurableRecord for KeyColumnType {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::obligation::OBLIGATION_LEDGER_VOCABULARY_VERSION);
}

impl crate::store::DurableRecord for ObligationKey {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::obligation::OBLIGATION_LEDGER_VOCABULARY_VERSION);
}

impl crate::store::DurableRecord for ObligationState {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::obligation::OBLIGATION_LEDGER_VOCABULARY_VERSION);
}

impl crate::store::DurableRecord for StallReason {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::obligation::OBLIGATION_LEDGER_VOCABULARY_VERSION);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_obligation_labels_are_typed_and_leave_the_key_undecodable() {
        for error in [
            ObligationKind::from_label("synthetic_next").map(|_| ()),
            ObligationState::from_label("synthetic_next").map(|_| ()),
            StallReason::from_label("synthetic_next").map(|_| ()),
        ] {
            assert!(matches!(
                error,
                Err(StoreError::Incompatible {
                    refusal: crate::compat::CompatRefusal::UnknownVocabulary { ref label, .. }
                }) if label == "synthetic_next"
            ));
        }
        assert!(ObligationKey::decode_label("synthetic_next", vec![]).is_err());
    }

    #[test]
    fn a_column_set_this_build_cannot_name_is_undecodable() {
        for (kind, columns) in [
            (
                ObligationKind::ArtifactCleanup,
                vec![KeyColumn::Integer(7), KeyColumn::Text("x".to_owned())],
            ),
            (
                ObligationKind::ArtifactCleanup,
                vec![KeyColumn::Text("session".to_owned())],
            ),
            (
                ObligationKind::ArtifactCleanup,
                vec![
                    KeyColumn::Text("owner".to_owned()),
                    KeyColumn::Text("x".to_owned()),
                ],
            ),
            (
                ObligationKind::ArtifactCleanup,
                vec![
                    KeyColumn::Text("host_pin".to_owned()),
                    KeyColumn::Text(String::new()),
                ],
            ),
        ] {
            assert!(
                ObligationKey::decode(kind, columns.clone()).is_err(),
                "{kind} decoded {columns:?}"
            );
        }
    }
}
