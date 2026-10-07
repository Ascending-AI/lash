//! A session's durable fault (ADR 0109 §9): corrupt stored data the engine
//! met where no sender is left to answer.
//!
//! A run's answer is published before its scope closes and before its
//! shift's next admission reads the session. Stored data that either one
//! finds corrupt cannot fail that run, whose answer stands, and no retry
//! repairs it. The fault is recorded on the session's `session_meta` row
//! instead, with the typed code and cause the read failed with. While it
//! stands the session admits nothing and every shift is refused with it.
//! Only an operator clears it.

use serde::{Deserialize, Serialize};

use super::StoreError;
use crate::{RuntimeError, RuntimeErrorCause, RuntimeErrorCode, SessionId, TurnId};

/// What met the fault.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum SessionFaultOrigin {
    /// The owed scope close of `run`, whose `ScopeClose` obligation stalled
    /// as refused with the same code.
    ScopeClose { run: TurnId },
    /// A shift's admission.
    DriveAdmission,
}

/// The failure a session fault retains: a runtime error's typed code, its
/// message and its cause.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFaultRecord {
    #[serde(flatten)]
    pub origin: SessionFaultOrigin,
    pub code: RuntimeErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<RuntimeErrorCause>,
}

impl SessionFaultRecord {
    /// The fault `origin` met as `error`.
    #[must_use]
    pub fn new(origin: SessionFaultOrigin, error: &RuntimeError) -> Self {
        Self {
            origin,
            code: error.code.clone(),
            message: error.message.clone(),
            cause: error.cause.clone(),
        }
    }

    /// The error every shift of the faulted session is refused with: the
    /// recorded code, message and cause.
    #[must_use]
    pub fn runtime_error(&self) -> RuntimeError {
        let error = RuntimeError::new(self.code.clone(), self.message.clone());
        match &self.cause {
            Some(cause) => error.with_cause(cause.clone()),
            None => error,
        }
    }
}

/// A session's standing fault, as an operator lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFault {
    pub session_id: SessionId,
    #[serde(flatten)]
    pub record: SessionFaultRecord,
    /// Host-clock epoch milliseconds of the recording.
    pub recorded_at_ms: u64,
}

impl SessionFaultRecord {
    /// The `fault_json` column's text.
    ///
    /// # Errors
    ///
    /// The record did not encode.
    pub fn to_stored(&self) -> Result<String, super::StoreError> {
        serde_json::to_string(self).map_err(|error| super::StoreError::RecordEncodingFailed {
            record_kind: "SessionFault".to_string(),
            message: error.to_string(),
        })
    }
}

impl SessionFault {
    /// Decode `session_id`'s `fault_json` and `fault_at_ms` columns.
    ///
    /// # Errors
    ///
    /// A column no build writes is corrupt.
    pub fn from_stored(
        session_id: SessionId,
        fault_json: &str,
        fault_at_ms: i64,
    ) -> Result<Self, super::StoreError> {
        let corrupt = |message: String| super::StoreError::StoredDataCorrupt {
            record_kind: "SessionFault",
            message,
        };
        Ok(Self {
            session_id,
            record: serde_json::from_str(fault_json).map_err(|error| corrupt(error.to_string()))?,
            recorded_at_ms: u64::try_from(fault_at_ms).map_err(|_| {
                corrupt(format!(
                    "fault_at_ms must be non-negative, got {fault_at_ms}"
                ))
            })?,
        })
    }

    /// [`Self::from_stored`] for the two nullable columns of a shift-epoch
    /// read: both set, or neither.
    ///
    /// # Errors
    ///
    /// One column without the other, or a column no build writes, is corrupt.
    pub fn from_stored_columns(
        session_id: &SessionId,
        fault_json: Option<String>,
        fault_at_ms: Option<i64>,
    ) -> Result<Option<Self>, super::StoreError> {
        match (fault_json, fault_at_ms) {
            (None, None) => Ok(None),
            (Some(json), Some(at_ms)) => {
                Self::from_stored(session_id.clone(), &json, at_ms).map(Some)
            }
            _ => Err(super::StoreError::StoredDataCorrupt {
                record_kind: "SessionFault",
                message: "fault_json and fault_at_ms are set together".to_string(),
            }),
        }
    }
}

/// A session's standing fault in its store (ADR 0109 §9): recorded once,
/// read, listed and cleared by an operator.
#[async_trait::async_trait]
pub trait SessionFaultStore: Send + Sync {
    /// Record `record` as `session_id`'s fault at `at_ms` (ADR 0109 §9) and
    /// answer the fault that now stands: a session already faulted keeps its
    /// first. `None` when the session has no `session_meta` row.
    async fn record_session_fault(
        &self,
        session_id: &SessionId,
        record: &SessionFaultRecord,
        at_ms: u64,
    ) -> Result<Option<SessionFault>, StoreError>;

    /// `session_id`'s standing fault, read by itself.
    async fn session_fault(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionFault>, StoreError>;

    /// The standing session faults after session `after`, in session-id
    /// order, at most `limit`.
    async fn list_session_faults(
        &self,
        after: Option<&SessionId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<SessionFault>, StoreError>;

    /// Clear `session_id`'s fault, an operator's verb: the session admits
    /// again. `false` when it had none.
    async fn clear_session_fault(&self, session_id: &SessionId) -> Result<bool, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_fault_keeps_its_typed_code_cause_and_origin() {
        let error = crate::StoreError::StoredDataCorrupt {
            record_kind: "SessionMeta",
            message: "unreadable".to_string(),
        }
        .runtime_error();
        let record = SessionFaultRecord::new(
            SessionFaultOrigin::ScopeClose {
                run: TurnId::from("run-1"),
            },
            &error,
        );
        let stored = record.to_stored().expect("encode the fault");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&stored).expect("stored JSON"),
            serde_json::json!({
                "origin": "scope_close",
                "run": "run-1",
                "code": "runtime_store_corrupt",
                "message": "stored SessionMeta data is corrupt: unreadable",
                "cause": {
                    "kind": "stored_data_corrupt",
                    "record_kind": "SessionMeta",
                    "message": "unreadable",
                },
            })
        );
        let session = SessionId::from("s");
        let fault = SessionFault::from_stored(session.clone(), &stored, 7).expect("decode");
        assert_eq!(
            fault,
            SessionFault {
                session_id: session.clone(),
                record: record.clone(),
                recorded_at_ms: 7,
            }
        );
        let refusal = fault.record.runtime_error();
        assert_eq!((&refusal.code, &refusal.cause), (&error.code, &error.cause));
        assert!(refusal.is_terminal() && !refusal.is_retryable());

        assert_eq!(
            SessionFault::from_stored_columns(&session, None, None).expect("no fault"),
            None
        );
        for (json, at_ms) in [(Some(stored.clone()), None), (None, Some(7))] {
            assert!(matches!(
                SessionFault::from_stored_columns(&session, json, at_ms),
                Err(crate::StoreError::StoredDataCorrupt { .. })
            ));
        }
        assert!(matches!(
            SessionFault::from_stored(session, "{}", 7),
            Err(crate::StoreError::StoredDataCorrupt { .. })
        ));
    }
}
