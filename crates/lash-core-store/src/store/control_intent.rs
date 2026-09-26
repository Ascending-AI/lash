//! Control intents (FIG-3600 S7, ADR 0104 O4, astra B6): an operator's
//! decision about a logical root or a session, persisted as a versioned
//! record in the transaction that applies its store half.
//!
//! The engine half runs after that transaction, idempotently, and is
//! acknowledged; a failure is retained as a failed intent that reconciliation
//! retries. A `CloseSession` intent outlives its session: it is the positive
//! deletion tombstone the factory answers a deleted session's roots from.
//!
//! The ledger is [`ControlIntentStore`], carried by the session store
//! factory rather than a session's own store: a `CloseSession` intent's engine
//! half is acknowledged, or retried by reconciliation, after its session is
//! gone.

use serde::{Deserialize, Serialize};

use super::{EnginePark, ParkId};
use crate::{SessionId, TurnId};

/// The registered durable format of a [`ControlIntent`] record.
pub const CONTROL_INTENT_FORMAT: u32 = 1;

/// A control intent's id: the store's intent clock sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ControlIntentId(u64);

impl ControlIntentId {
    /// The id the store's intent clock allocated as `sequence`.
    #[must_use]
    pub const fn from_sequence(sequence: u64) -> Self {
        Self(sequence)
    }

    /// The clock sequence this id names.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for ControlIntentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// What an intent decides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlIntentKind {
    /// Resume the parked root's execution under the same fence.
    Redrive { root: TurnId, park: ParkId },
    /// End the parked root `Cancelled`.
    Cancel { root: TurnId, park: ParkId },
    /// End the parked root and drive its held inputs under `new_root`.
    Fork {
        root: TurnId,
        park: ParkId,
        new_root: Option<TurnId>,
    },
    /// Close the session: every listed root ends `Cancelled`.
    CloseSession { roots: Vec<TurnId> },
}

impl ControlIntentKind {
    /// The stored code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Redrive { .. } => "redrive",
            Self::Cancel { .. } => "cancel",
            Self::Fork { .. } => "fork",
            Self::CloseSession { .. } => "close_session",
        }
    }
}

/// Where an intent's engine half stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ControlIntentState {
    /// The store half committed; the engine half is not acknowledged.
    Pending,
    Acknowledged {
        at_ms: u64,
    },
    /// A redrive overtaken by a cancel, fork or close before it applied.
    Superseded {
        by: ControlIntentId,
    },
    Failed {
        last_error: String,
        retryable: bool,
    },
}

impl ControlIntentState {
    /// The stored code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Acknowledged { .. } => "acknowledged",
            Self::Superseded { .. } => "superseded",
            Self::Failed { .. } => "failed",
        }
    }

    /// Whether reconciliation still owes this intent its engine half.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        matches!(
            self,
            Self::Pending
                | Self::Failed {
                    retryable: true,
                    ..
                }
        )
    }
}

/// One control intent, as the store holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlIntent {
    pub id: ControlIntentId,
    pub session_id: SessionId,
    /// [`CONTROL_INTENT_FORMAT`] of the writer.
    pub format: u32,
    pub kind: ControlIntentKind,
    pub state: ControlIntentState,
    pub attempts: u32,
    pub created_at_ms: u64,
    /// The engine's handle on the root's stopped execution, copied from its
    /// park by a verb whose store half deletes the park (a cancel or a
    /// fork), so the engine half can still find the execution to release.
    pub engine: Option<EnginePark>,
}

impl ControlIntent {
    /// The deletion this intent records, when it is a session's
    /// `CloseSession`: the terminal evidence every root of the deleted
    /// session answers.
    #[must_use]
    pub fn session_deleted_terminal(&self, root: &TurnId) -> Option<super::RootTerminal> {
        matches!(self.kind, ControlIntentKind::CloseSession { .. }).then(|| super::RootTerminal {
            session_id: self.session_id.clone(),
            root: root.clone(),
            kind: super::RootTerminalKind::Cancelled,
            cause: super::RootTerminalCause::SessionDeleted { intent: self.id },
            head_revision: None,
            at_ms: self.created_at_ms,
        })
    }

    /// Decode the columns a backend stored, refusing a format this build
    /// does not read.
    #[allow(clippy::too_many_arguments)]
    pub fn from_stored(
        id: u64,
        session_id: SessionId,
        format: u32,
        kind_json: &str,
        state_json: &str,
        attempts: u32,
        created_at_ms: u64,
        engine: Option<String>,
    ) -> Result<Self, super::StoreError> {
        if format != CONTROL_INTENT_FORMAT {
            return Err(super::StoreError::UnsupportedRecordSchemaVersion {
                record_kind: "ControlIntent",
                actual: format,
                expected: CONTROL_INTENT_FORMAT,
            });
        }
        let corrupt = |message: String| super::StoreError::StoredDataCorrupt {
            record_kind: "ControlIntent",
            message,
        };
        Ok(Self {
            id: ControlIntentId::from_sequence(id),
            session_id,
            format,
            kind: serde_json::from_str(kind_json)
                .map_err(|error| corrupt(format!("control intent kind: {error}")))?,
            state: serde_json::from_str(state_json)
                .map_err(|error| corrupt(format!("control intent state: {error}")))?,
            attempts,
            created_at_ms,
            engine: engine.map(EnginePark::new),
        })
    }
}

impl ControlIntent {
    /// The roots a `CloseSession` intent closed; empty for every other kind.
    #[must_use]
    pub fn closed_roots(&self) -> &[TurnId] {
        match &self.kind {
            ControlIntentKind::CloseSession { roots } => roots,
            _ => &[],
        }
    }

    /// The instant-free identity of the intent's decision: its id, session
    /// and kind. A retried writer answers the same decision at another
    /// attempt count or state.
    #[must_use]
    pub fn same_decision(&self, other: &Self) -> bool {
        self.id == other.id && self.session_id == other.session_id && self.kind == other.kind
    }
}

/// What [`ControlIntentStore::claim_intent_application`] answers: whether the
/// engine half should run now. The state is re-read in the store's
/// transaction, so an intent a later one superseded never reaches its engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntentApplication {
    /// The intent is open: run its engine half. The answer carries the
    /// intent with its attempt counted.
    Apply(ControlIntent),
    /// A later intent superseded it before it applied: run nothing.
    Superseded(ControlIntent),
    /// Its engine half is acknowledged, or failed for good: run nothing.
    Done(ControlIntent),
}

impl IntentApplication {
    /// The intent, whatever the answer.
    #[must_use]
    pub fn intent(&self) -> &ControlIntent {
        match self {
            Self::Apply(intent) | Self::Superseded(intent) | Self::Done(intent) => intent,
        }
    }
}

/// Decide a claim of an intent's application against its stored state:
/// the new state and attempt count to write, if any, and the answer.
#[must_use]
pub fn decide_intent_application(stored: ControlIntent) -> IntentApplication {
    match stored.state {
        ControlIntentState::Pending
        | ControlIntentState::Failed {
            retryable: true, ..
        } => {
            let mut claimed = stored;
            claimed.attempts = claimed.attempts.saturating_add(1);
            IntentApplication::Apply(claimed)
        }
        ControlIntentState::Superseded { .. } => IntentApplication::Superseded(stored),
        ControlIntentState::Acknowledged { .. }
        | ControlIntentState::Failed {
            retryable: false, ..
        } => IntentApplication::Done(stored),
    }
}

/// The state an acknowledgement writes over `stored`: `None` when the intent
/// is not open (already acknowledged, superseded or failed for good), so a
/// retried acknowledgement writes nothing.
#[must_use]
pub fn decide_intent_acknowledgement(
    stored: &ControlIntentState,
    at_ms: u64,
) -> Option<ControlIntentState> {
    stored
        .is_open()
        .then_some(ControlIntentState::Acknowledged { at_ms })
}

/// The state a failure writes over `stored`: `None` when the intent is no
/// longer open, so a late failure never reopens an acknowledged or
/// superseded intent.
#[must_use]
pub fn decide_intent_failure(
    stored: &ControlIntentState,
    error: &str,
    retryable: bool,
) -> Option<ControlIntentState> {
    stored.is_open().then(|| ControlIntentState::Failed {
        last_error: error.to_string(),
        retryable,
    })
}

/// The stored columns of an intent's state: its code (the open-intent index
/// keys on it) and its JSON.
pub fn stored_intent_state(
    state: &ControlIntentState,
) -> Result<(&'static str, String), super::StoreError> {
    let code = match state {
        ControlIntentState::Failed {
            retryable: true, ..
        } => "failed_retryable",
        other => other.code(),
    };
    let json =
        serde_json::to_string(state).map_err(|error| super::StoreError::RecordEncodingFailed {
            record_kind: "ControlIntent".to_string(),
            message: error.to_string(),
        })?;
    Ok((code, json))
}

/// The stored JSON of an intent's kind.
pub fn stored_intent_kind(kind: &ControlIntentKind) -> Result<String, super::StoreError> {
    serde_json::to_string(kind).map_err(|error| super::StoreError::RecordEncodingFailed {
        record_kind: "ControlIntent".to_string(),
        message: error.to_string(),
    })
}

/// The deployment's control-intent ledger (FIG-3600 S7, ADR 0104 O4, astra
/// B6), carried by the session store factory.
///
/// Every method is required: a factory states its answer, and a decorator
/// forwards to the catalog it wraps. A factory with no ledger returns
/// `StoreError::UnsupportedStoreOperation`, which fails a session deletion
/// closed instead of deleting a session whose roots nothing closed.
#[async_trait::async_trait]
pub trait ControlIntentStore: Send + Sync {
    /// Begin closing session `session_id`: the store half of its
    /// `CloseSession` intent, in one transaction. It
    ///
    /// - records the intent on the session (`session_meta.closing_intent`):
    ///   acceptance then refuses the session, and admission answers idle;
    /// - raises the session's drive epoch, so every fence an earlier
    ///   admission sealed is stale;
    /// - ends every root without terminal evidence `Cancelled` with cause
    ///   [`SessionDeleted`](super::RootTerminalCause::SessionDeleted), deletes
    ///   the session's park (feed `Cancelled{SessionDeleted}`) and settles its
    ///   open queued run;
    /// - supersedes every open intent of the session;
    /// - inserts the `CloseSession { roots }` intent, `Pending`, naming the
    ///   roots it ended.
    ///
    /// Idempotent: a retry finds the session's intent and answers it, also
    /// after the session is deleted (the intent is its tombstone). `None`
    /// when the session has no durable record and was never closed: there
    /// is nothing to close, and nothing is written.
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> Result<Option<ControlIntent>, super::StoreError>;

    /// Claim the application of intent `id`'s engine half: re-read its state
    /// in a transaction and count the attempt when it is open. An unknown id
    /// is `StoreError::ControlIntentUnknown`.
    async fn claim_intent_application(
        &self,
        id: ControlIntentId,
    ) -> Result<IntentApplication, super::StoreError>;

    /// Acknowledge intent `id`'s engine half. A no-op once it is not open.
    async fn acknowledge_intent(
        &self,
        id: ControlIntentId,
        at_ms: u64,
    ) -> Result<(), super::StoreError>;

    /// Retain the failure of intent `id`'s engine half: a retryable failure
    /// leaves it open for reconciliation, a permanent one is visible for an
    /// operator. A no-op once it is not open. Answers the stored intent.
    async fn record_intent_failure(
        &self,
        id: ControlIntentId,
        error: &str,
        retryable: bool,
        at_ms: u64,
    ) -> Result<ControlIntent, super::StoreError>;

    /// Intent `id` as stored, if it exists.
    async fn load_intent(
        &self,
        id: ControlIntentId,
    ) -> Result<Option<ControlIntent>, super::StoreError>;
}

/// A control-intent ledger held in memory, for store doubles that keep no
/// SQL rows. It decides every write with the same deciders a SQL backend
/// runs inside its transaction.
#[derive(Debug, Default)]
pub struct InMemoryControlIntents {
    state: std::sync::Mutex<std::collections::BTreeMap<ControlIntentId, ControlIntent>>,
}

impl InMemoryControlIntents {
    fn state(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<ControlIntentId, ControlIntent>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// [`ControlIntentStore::begin_session_close`] over this ledger: the
    /// session's kept `CloseSession` intent, else a new one closing `roots`
    /// when the session `exists`, else `None`.
    pub fn begin_session_close(
        &self,
        session_id: &SessionId,
        exists: bool,
        roots: Vec<TurnId>,
        at_ms: u64,
    ) -> Option<ControlIntent> {
        let mut state = self.state();
        if let Some(intent) = state.values().find(|intent| {
            intent.session_id == *session_id
                && matches!(intent.kind, ControlIntentKind::CloseSession { .. })
        }) {
            return Some(intent.clone());
        }
        if !exists {
            return None;
        }
        let id = ControlIntentId::from_sequence(
            state
                .keys()
                .next_back()
                .map_or(1, |last| last.sequence().saturating_add(1)),
        );
        let intent = ControlIntent {
            id,
            session_id: session_id.clone(),
            format: CONTROL_INTENT_FORMAT,
            kind: ControlIntentKind::CloseSession { roots },
            state: ControlIntentState::Pending,
            attempts: 0,
            created_at_ms: at_ms,
            engine: None,
        };
        state.insert(id, intent.clone());
        Some(intent)
    }

    fn rewrite<T>(
        &self,
        id: ControlIntentId,
        write: impl FnOnce(&mut ControlIntent) -> T,
    ) -> Result<T, super::StoreError> {
        let mut state = self.state();
        let intent = state
            .get_mut(&id)
            .ok_or(super::StoreError::ControlIntentUnknown { intent: id })?;
        Ok(write(intent))
    }

    /// [`ControlIntentStore::claim_intent_application`] over this ledger.
    pub fn claim(&self, id: ControlIntentId) -> Result<IntentApplication, super::StoreError> {
        self.rewrite(id, |intent| {
            let application = decide_intent_application(intent.clone());
            if let IntentApplication::Apply(applied) = &application {
                *intent = applied.clone();
            }
            application
        })
    }

    /// [`ControlIntentStore::acknowledge_intent`] over this ledger.
    pub fn acknowledge(&self, id: ControlIntentId, at_ms: u64) -> Result<(), super::StoreError> {
        self.rewrite(id, |intent| {
            if let Some(state) = decide_intent_acknowledgement(&intent.state, at_ms) {
                intent.state = state;
            }
        })
    }

    /// [`ControlIntentStore::record_intent_failure`] over this ledger.
    pub fn fail(
        &self,
        id: ControlIntentId,
        error: &str,
        retryable: bool,
    ) -> Result<ControlIntent, super::StoreError> {
        self.rewrite(id, |intent| {
            if let Some(state) = decide_intent_failure(&intent.state, error, retryable) {
                intent.state = state;
            }
            intent.clone()
        })
    }

    /// [`ControlIntentStore::load_intent`] over this ledger.
    #[must_use]
    pub fn load(&self, id: ControlIntentId) -> Option<ControlIntent> {
        self.state().get(&id).cloned()
    }

    /// `SessionStoreFactory::list_open_control_intents` over this ledger.
    #[must_use]
    pub fn open_after(&self, after: Option<ControlIntentId>, limit: usize) -> Vec<ControlIntent> {
        self.state()
            .values()
            .filter(|intent| after.is_none_or(|after| intent.id > after) && intent.state.is_open())
            .take(limit)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(state: ControlIntentState) -> ControlIntent {
        ControlIntent {
            id: ControlIntentId::from_sequence(4),
            session_id: SessionId::from("s"),
            format: CONTROL_INTENT_FORMAT,
            kind: ControlIntentKind::CloseSession {
                roots: vec![TurnId::from("r")],
            },
            state,
            attempts: 0,
            created_at_ms: 1,
            engine: None,
        }
    }

    #[test]
    fn an_open_intent_is_applied_with_its_attempt_counted_and_a_closed_one_is_not() {
        let IntentApplication::Apply(applied) =
            decide_intent_application(intent(ControlIntentState::Pending))
        else {
            panic!("a pending intent applies");
        };
        assert_eq!(applied.attempts, 1);
        assert!(matches!(
            decide_intent_application(intent(ControlIntentState::Failed {
                last_error: "x".into(),
                retryable: true
            })),
            IntentApplication::Apply(_)
        ));
        assert!(matches!(
            decide_intent_application(intent(ControlIntentState::Superseded {
                by: ControlIntentId::from_sequence(9)
            })),
            IntentApplication::Superseded(_)
        ));
        assert!(matches!(
            decide_intent_application(intent(ControlIntentState::Acknowledged { at_ms: 2 })),
            IntentApplication::Done(_)
        ));
        assert!(matches!(
            decide_intent_application(intent(ControlIntentState::Failed {
                last_error: "x".into(),
                retryable: false
            })),
            IntentApplication::Done(_)
        ));
    }

    #[test]
    fn a_late_acknowledgement_or_failure_never_reopens_a_closed_intent() {
        let acknowledged = ControlIntentState::Acknowledged { at_ms: 2 };
        assert_eq!(decide_intent_acknowledgement(&acknowledged, 5), None);
        assert_eq!(decide_intent_failure(&acknowledged, "late", true), None);
        assert_eq!(
            decide_intent_failure(&ControlIntentState::Pending, "engine down", true),
            Some(ControlIntentState::Failed {
                last_error: "engine down".into(),
                retryable: true
            })
        );
        assert_eq!(
            stored_intent_state(&ControlIntentState::Failed {
                last_error: "x".into(),
                retryable: true
            })
            .expect("encode")
            .0,
            "failed_retryable"
        );
    }

    #[test]
    fn stored_columns_round_trip() {
        let stored = intent(ControlIntentState::Pending);
        let (_, state_json) = stored_intent_state(&stored.state).expect("state");
        let kind_json = stored_intent_kind(&stored.kind).expect("kind");
        let decoded = ControlIntent::from_stored(
            4,
            SessionId::from("s"),
            CONTROL_INTENT_FORMAT,
            &kind_json,
            &state_json,
            0,
            1,
            None,
        )
        .expect("decode");
        assert_eq!(decoded, stored);
        assert_eq!(decoded.closed_roots(), &[TurnId::from("r")]);
    }
}
