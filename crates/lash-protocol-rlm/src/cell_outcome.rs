//! The parked-driver-state spelling of a cell outcome.
//!
//! Recorded `RlmDriverState` payloads carry the outcome as the `error` /
//! `terminal_finish` key pair. This wrapper keeps those exact bytes while the
//! in-memory value is the three-valued [`CellOutcome`]: encoding writes at
//! most one of the two keys, and decoding refuses a payload that sets both —
//! the combination no adjudication can produce.

use std::ops::{Deref, DerefMut};

use lash_core::CellFailure;
use lash_rlm_types::CellOutcome;
use serde_json::Value;

/// A `CellOutcome<CellFailure>` in its parked-state serde spelling.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ParkedCellOutcome(pub CellOutcome<CellFailure>);

impl From<CellOutcome<CellFailure>> for ParkedCellOutcome {
    fn from(outcome: CellOutcome<CellFailure>) -> Self {
        Self(outcome)
    }
}

impl Deref for ParkedCellOutcome {
    type Target = CellOutcome<CellFailure>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ParkedCellOutcome {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl serde::Serialize for ParkedCellOutcome {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Both keys are always written: recorded states carry them as
        // `null` when unset, and the parked bytes must not change.
        #[derive(serde::Serialize)]
        struct Fields<'a> {
            error: Option<&'a CellFailure>,
            terminal_finish: Option<&'a Value>,
        }
        let (error, terminal_finish) = match &self.0 {
            CellOutcome::Running => (None, None),
            CellOutcome::Failed(error) => (Some(error), None),
            CellOutcome::Finished(value) => (None, Some(value)),
        };
        Fields {
            error,
            terminal_finish,
        }
        .serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ParkedCellOutcome {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Fields {
            error: Option<CellFailure>,
            terminal_finish: Option<Value>,
        }
        let fields = Fields::deserialize(deserializer)?;
        match (fields.error, fields.terminal_finish) {
            (Some(_), Some(_)) => Err(serde::de::Error::custom(
                "a cell outcome cannot carry both `error` and `terminal_finish`",
            )),
            (error, terminal) => Ok(Self(CellOutcome::from_parts(error, terminal))),
        }
    }
}
