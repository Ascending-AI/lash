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
use crate::{ProcessId, SessionId, TurnId};

use super::StoreError;
use super::control_intent::ControlIntentId;

/// The obligation state, kind, key, and stall labels written at the 1.0 cut.
#[cfg(not(feature = "synthetic-next"))]
pub const OBLIGATION_LEDGER_VOCABULARY_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) declares the vocabulary one ahead
/// and still writes only N's states, kinds, keys and stall reasons while `F`
/// is N's epoch (ADR 0115 §5). A row a later build writes with a label N
/// does not know stays outstanding under N, stalled `undecodable`.
#[cfg(feature = "synthetic-next")]
pub const OBLIGATION_LEDGER_VOCABULARY_VERSION: u32 = 2;

/// Which ledger an obligation lives on. Its [`label`](Self::label) is the
/// metric label and the drain-status key.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ObligationKind {
    /// An admitted ingress item owes its session a drive.
    Ingress,
    /// A control intent owes its engine half and its follow-on drive.
    ControlIntent,
    /// A terminal root owes its scope close.
    ScopeClose,
    /// A closed scope's plan owes each child its cancel.
    ParentEnd,
    /// A closing session owes its physical delete.
    SessionDelete,
    /// A registered process owes its first engine run.
    ProcessStart,
    /// A terminal process owes its terminal publication.
    ProcessTerminal,
    /// An ended or guarded artifact referrer owes its cleanup (ADR 0113 §2.5).
    ArtifactCleanup,
}

impl ObligationKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 8] = [
        Self::Ingress,
        Self::ControlIntent,
        Self::ScopeClose,
        Self::ParentEnd,
        Self::SessionDelete,
        Self::ProcessStart,
        Self::ProcessTerminal,
        Self::ArtifactCleanup,
    ];

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
            Self::Ingress => "ingress",
            Self::ControlIntent => "control_intent",
            Self::ScopeClose => "scope_close",
            Self::ParentEnd => "parent_end",
            Self::SessionDelete => "session_delete",
            Self::ProcessStart => "process_start",
            Self::ProcessTerminal => "process_terminal",
            Self::ArtifactCleanup => "artifact_cleanup",
        }
    }

    /// The SQL types of this kind's key columns, in the order a claim's and
    /// a stalled listing's projection ends with them.
    #[must_use]
    pub const fn key_column_types(self) -> &'static [KeyColumnType] {
        match self {
            Self::Ingress | Self::ScopeClose | Self::ParentEnd | Self::ArtifactCleanup => {
                &[KeyColumnType::Text, KeyColumnType::Text]
            }
            Self::ControlIntent => &[KeyColumnType::Integer],
            Self::SessionDelete | Self::ProcessStart | Self::ProcessTerminal => {
                &[KeyColumnType::Text]
            }
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
    /// An admission row: a `pending_turn_inputs` input (`ti:` id) or a
    /// `queued_work_batches` batch (`qwb:` id).
    Ingress {
        session_id: SessionId,
        item_id: String,
    },
    /// A `control_intents` row.
    ControlIntent { intent_id: ControlIntentId },
    /// A `session_roots` row.
    ScopeClose { session_id: SessionId, root: TurnId },
    /// A `parent_end_plans` row: the scope's stored kind label and id.
    ParentEnd {
        parent_kind: String,
        parent_id: String,
    },
    /// A `session_meta` row.
    SessionDelete { session_id: SessionId },
    /// A `processes` row.
    ProcessStart { process_id: ProcessId },
    /// A `processes` row.
    ProcessTerminal { process_id: ProcessId },
    /// An `artifact_cleanup_obligations` row: the referrer's stored pair.
    ArtifactCleanup { referrer: ArtifactReferrer },
}

impl ObligationKey {
    /// Decode a row whose kind label came from storage. A foreign kind leaves
    /// the row addressable by its obligation id but cannot be delivered here.
    pub fn decode_label(
        kind_label: &str,
        columns: Vec<KeyColumn>,
    ) -> Result<Self, UndecodableObligation> {
        let kind =
            ObligationKind::from_label(kind_label).map_err(|error| UndecodableObligation {
                detail: error.to_string(),
            })?;
        Self::decode(kind, columns)
    }

    /// The ledger this key addresses.
    #[must_use]
    pub const fn kind(&self) -> ObligationKind {
        match self {
            Self::Ingress { .. } => ObligationKind::Ingress,
            Self::ControlIntent { .. } => ObligationKind::ControlIntent,
            Self::ScopeClose { .. } => ObligationKind::ScopeClose,
            Self::ParentEnd { .. } => ObligationKind::ParentEnd,
            Self::SessionDelete { .. } => ObligationKind::SessionDelete,
            Self::ProcessStart { .. } => ObligationKind::ProcessStart,
            Self::ProcessTerminal { .. } => ObligationKind::ProcessTerminal,
            Self::ArtifactCleanup { .. } => ObligationKind::ArtifactCleanup,
        }
    }
}

impl ObligationKey {
    /// The key's columns, in the ledger's key order.
    #[must_use]
    pub fn columns(&self) -> Vec<KeyColumn> {
        match self {
            Self::Ingress {
                session_id,
                item_id,
            } => vec![
                KeyColumn::Text(session_id.as_str().to_owned()),
                KeyColumn::Text(item_id.clone()),
            ],
            Self::ControlIntent { intent_id } => vec![KeyColumn::Integer(
                i64::try_from(intent_id.sequence()).unwrap_or(i64::MAX),
            )],
            Self::ScopeClose { session_id, root } => vec![
                KeyColumn::Text(session_id.as_str().to_owned()),
                KeyColumn::Text(root.as_str().to_owned()),
            ],
            Self::ParentEnd {
                parent_kind,
                parent_id,
            } => vec![
                KeyColumn::Text(parent_kind.clone()),
                KeyColumn::Text(parent_id.clone()),
            ],
            Self::SessionDelete { session_id } => {
                vec![KeyColumn::Text(session_id.as_str().to_owned())]
            }
            Self::ProcessStart { process_id } | Self::ProcessTerminal { process_id } => {
                vec![KeyColumn::Text(process_id.as_str().to_owned())]
            }
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
            ObligationKind::Ingress => Self::Ingress {
                session_id: SessionId::from(next_text(&mut columns, kind, "session_id")?),
                item_id: next_text(&mut columns, kind, "item_id")?,
            },
            ObligationKind::ScopeClose => Self::ScopeClose {
                session_id: SessionId::from(next_text(&mut columns, kind, "session_id")?),
                root: TurnId::from(next_text(&mut columns, kind, "root")?),
            },
            ObligationKind::ParentEnd => Self::ParentEnd {
                parent_kind: next_text(&mut columns, kind, "parent_kind")?,
                parent_id: next_text(&mut columns, kind, "parent_id")?,
            },
            ObligationKind::SessionDelete => Self::SessionDelete {
                session_id: SessionId::from(next_text(&mut columns, kind, "session_id")?),
            },
            ObligationKind::ArtifactCleanup => {
                let referrer_kind = next_text(&mut columns, kind, "referrer_kind")?;
                let referrer_id = next_text(&mut columns, kind, "referrer_id")?;
                Self::ArtifactCleanup {
                    referrer: ArtifactReferrer::decode(&referrer_kind, &referrer_id).map_err(
                        |error| UndecodableObligation {
                            detail: error
                                .into_store_error("artifact_cleanup_obligation")
                                .to_string(),
                        },
                    )?,
                }
            }
            kind @ (ObligationKind::ProcessStart | ObligationKind::ProcessTerminal) => {
                let process_id = ProcessId::parse(&next_text(&mut columns, kind, "process_id")?)
                    .map_err(|error| UndecodableObligation {
                        detail: error.to_string(),
                    })?;
                match kind {
                    ObligationKind::ProcessStart => Self::ProcessStart { process_id },
                    ObligationKind::ProcessTerminal => Self::ProcessTerminal { process_id },
                    _ => unreachable!("the matched kinds are ProcessStart and ProcessTerminal"),
                }
            }
            ObligationKind::ControlIntent => {
                let sequence = next_integer(&mut columns, kind, "intent_id")?;
                Self::ControlIntent {
                    intent_id: ControlIntentId::from_sequence(u64::try_from(sequence).map_err(
                        |_| UndecodableObligation {
                            detail: format!("control intent id {sequence} is negative"),
                        },
                    )?),
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
        other => Err(UndecodableObligation {
            detail: format!("{kind} obligation key column `{name}` is {other:?}, not text"),
        }),
    }
}

/// The next key column as an integer, or why it is not.
fn next_integer(
    columns: &mut impl Iterator<Item = KeyColumn>,
    kind: ObligationKind,
    name: &str,
) -> Result<i64, UndecodableObligation> {
    match columns.next() {
        Some(KeyColumn::Integer(value)) => Ok(value),
        other => Err(UndecodableObligation {
            detail: format!("{kind} obligation key column `{name}` is {other:?}, not an integer"),
        }),
    }
}

/// The stable id of one armed obligation, minted when the row is armed.
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

    /// A fresh id for an obligation of `kind`.
    #[must_use]
    pub fn mint(kind: ObligationKind) -> Self {
        Self(format!(
            "{}:{}",
            kind.label(),
            uuid::Uuid::new_v4().simple()
        ))
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

/// The obligation created by a process registration. A caller may attempt
/// delivery immediately after its registration transaction commits.
#[must_use]
pub fn process_start_obligation_id(process_id: &ProcessId) -> ObligationId {
    ObligationId::new(format!("process_start:{}", process_id.as_str()))
}

/// The derived id of a terminal root's scope-close obligation (ADR 0109
/// §3): stable per `(session, root)` — the terminal transaction arms it, and
/// the close's own delivery names the same id to claim it.
#[must_use]
pub fn scope_close_obligation_id(session_id: &SessionId, root: &TurnId) -> ObligationId {
    ObligationId::new(format!(
        "{}:{}:{}",
        ObligationKind::ScopeClose.label(),
        session_id.as_str(),
        root.as_str()
    ))
}

/// The token one claim stamps; every settling write compares it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClaimToken(String);

impl ClaimToken {
    /// A fresh token.
    #[must_use]
    pub fn mint() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
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

/// A row this build could not decode into its key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndecodableObligation {
    pub detail: String,
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
    Retry { due_at_ms: u64, error: String },
    /// Stop until re-armed.
    Stall { reason: StallReason, error: String },
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
    pub last_error: Option<String>,
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

    /// Arm `key`'s row outside a producer transaction (the leader's repair
    /// pass): only a row that owes nothing is armed, due at `now_ms`.
    /// `None` if the row is missing or already carries an obligation.
    async fn arm(
        &self,
        key: &ObligationKey,
        now_ms: u64,
    ) -> Result<Option<ObligationId>, StoreError>;

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

    /// Claim `id` for immediate delivery. `None` unless it is `due` (at any
    /// time: a producer's own attempt does not wait for a backoff).
    async fn claim(
        &self,
        id: &ObligationId,
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
    fn every_key_round_trips_through_its_columns() {
        let keys = [
            ObligationKey::Ingress {
                session_id: SessionId::from("s"),
                item_id: "item".to_owned(),
            },
            ObligationKey::ControlIntent {
                intent_id: ControlIntentId::from_sequence(7),
            },
            ObligationKey::SessionDelete {
                session_id: SessionId::from("s"),
            },
            ObligationKey::ArtifactCleanup {
                referrer: ArtifactReferrer::HostPin(
                    crate::artifact_referrer::HostArtifactPin::mint(),
                ),
            },
        ];
        for key in keys {
            assert_eq!(
                ObligationKey::decode(key.kind(), key.columns()),
                Ok(key.clone())
            );
        }
    }

    #[test]
    fn a_column_set_this_build_cannot_name_is_undecodable() {
        for (kind, columns) in [
            (ObligationKind::SessionDelete, vec![KeyColumn::Integer(7)]),
            (
                ObligationKind::Ingress,
                vec![KeyColumn::Text("s".to_owned())],
            ),
            (ObligationKind::ControlIntent, vec![KeyColumn::Integer(-1)]),
            (
                ObligationKind::ControlIntent,
                vec![KeyColumn::Text("7".to_owned())],
            ),
            (
                ObligationKind::ProcessTerminal,
                vec![KeyColumn::Text("not a process id".to_owned())],
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

    #[test]
    fn a_cleanup_key_of_an_unknown_referrer_kind_names_the_typed_refusal() {
        let label = crate::artifact_referrer::SYNTHETIC_NEXT_REFERRER_KIND;
        let undecodable = ObligationKey::decode(
            ObligationKind::ArtifactCleanup,
            vec![
                KeyColumn::Text(label.to_owned()),
                KeyColumn::Text("x".to_owned()),
            ],
        )
        .expect_err("no build of this window names the next kind");
        let refusal = StoreError::Incompatible {
            refusal: crate::compat::CompatRefusal::UnknownVocabulary {
                surface: "artifact referrer kind".to_owned(),
                label: label.to_owned(),
            },
        };
        assert_eq!(undecodable.detail, refusal.to_string());
    }
}
