//! Control intents (FIG-3600 S7, ADR 0104 O4, astra B6): an operator's
//! decision about a logical root or a session, persisted as a versioned
//! record in the transaction that applies its store half.
//!
//! The engine half runs after that transaction, idempotently, and is
//! acknowledged; a failure is retained as a failed intent that reconciliation
//! retries. A `CloseSession` intent outlives its session: it is the positive
//! deletion tombstone the factory answers a deleted session's roots from.
//!
//! This module holds the record's vocabulary. The verbs that open and apply
//! intents land with the park verbs and session close.

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
