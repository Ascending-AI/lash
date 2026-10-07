//! Control intents (FIG-3600 S7): a session's close, persisted as a
//! versioned record in the transaction that applies its store half.
//!
//! A `CloseSession` intent outlives its session: it is the positive deletion
//! tombstone the factory answers a deleted session's runs from. The intent
//! is session mail: its producer wakes the session actor, whose close steps
//! act on it under the session's epoch (ADR 0132 §12).
//!
//! The ledger is [`ControlIntentStore`], carried by the session store
//! factory rather than a session's own store: a `CloseSession` intent is
//! read after its session is gone.

use serde::{Deserialize, Serialize};

use crate::{SessionId, TurnId};

/// The registered durable format of a [`ControlIntent`] record.
/// version_surface = "coexist"
/// version_guard( items(from_stored))
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
    /// Close the session: every listed run ends `Cancelled`.
    CloseSession { runs: Vec<TurnId> },
}

impl ControlIntentKind {
    /// The stored code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::CloseSession { .. } => "close_session",
        }
    }
}

/// Where an intent stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ControlIntentState {
    /// The store half committed; the session actor has not finished it.
    Pending,
    /// The session actor finished it at `at_ms`.
    Acknowledged { at_ms: u64 },
}

impl ControlIntentState {
    /// The stored code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Acknowledged { .. } => "acknowledged",
        }
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
    pub created_at_ms: u64,
}

impl ControlIntent {
    /// The deletion this intent records, when it is a session's
    /// `CloseSession`: the terminal evidence every run of the deleted
    /// session answers.
    #[must_use]
    pub fn session_deleted_terminal(&self, run: &TurnId) -> Option<super::RunTerminal> {
        matches!(self.kind, ControlIntentKind::CloseSession { .. }).then(|| super::RunTerminal {
            session_id: self.session_id.clone(),
            run: run.clone(),
            cause: super::RunTerminalCause::SessionDeleted { intent: self.id },
            head_revision: None,
            at_ms: self.created_at_ms,
        })
    }

    /// Decode the columns a backend stored, refusing a format this build
    /// does not read.
    pub fn from_stored(
        id: u64,
        session_id: SessionId,
        format: u32,
        kind_json: &str,
        state_json: &str,
        created_at_ms: u64,
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
            created_at_ms,
        })
    }

    /// The runs a `CloseSession` intent closed.
    #[must_use]
    pub fn closed_runs(&self) -> &[TurnId] {
        match &self.kind {
            ControlIntentKind::CloseSession { runs } => runs,
        }
    }

    /// The instant-free identity of the intent's decision: its id, session
    /// and kind. A retried writer answers the same decision in another
    /// state.
    #[must_use]
    pub fn same_decision(&self, other: &Self) -> bool {
        self.id == other.id && self.session_id == other.session_id && self.kind == other.kind
    }
}

/// The stored columns of an intent's state: its code and its JSON.
pub fn stored_intent_state(
    state: &ControlIntentState,
) -> Result<(&'static str, String), super::StoreError> {
    let code = state.code();
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

/// The deployment's control-intent ledger (FIG-3600 S7), carried by the
/// session store factory.
///
/// Every method is required: a factory states its answer, and a decorator
/// forwards to the catalog it wraps. A factory with no ledger returns
/// `StoreError::UnsupportedStoreOperation`, which fails a session deletion
/// closed instead of deleting a session whose runs nothing closed.
#[async_trait::async_trait]
pub trait ControlIntentStore: Send + Sync {
    /// Begin closing session `session_id`: the store half of its
    /// `CloseSession` intent, in one transaction. It
    ///
    /// - records the intent on the session (`session_meta.closing_intent`):
    ///   acceptance then refuses the session, and the session mail drain
    ///   admits nothing;
    /// - ends every run without terminal evidence `Cancelled` with cause
    ///   [`SessionDeleted`](super::RunTerminalCause::SessionDeleted) and
    ///   settles its open queued run;
    /// - inserts the `CloseSession { runs }` intent, `Pending`, naming the
    ///   runs it ended, and wakes the session actor.
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

    /// Session `session_id`'s `CloseSession` intent, if its close began: its
    /// deletion tombstone once it is deleted.
    async fn session_close_intent(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<ControlIntent>, super::StoreError>;

    /// Intent `id` as stored, if it exists.
    async fn load_intent(
        &self,
        id: ControlIntentId,
    ) -> Result<Option<ControlIntent>, super::StoreError>;
}

impl crate::store::DurableRecord for ControlIntent {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::control_intent::CONTROL_INTENT_FORMAT);
}

impl crate::store::DurableRecord for ControlIntentId {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::store::obligation::OBLIGATION_LEDGER_VOCABULARY_VERSION);
}
