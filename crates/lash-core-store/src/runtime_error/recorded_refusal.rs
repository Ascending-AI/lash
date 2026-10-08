//! The one stored form of a refusal (FIG-5391).

use super::{RuntimeError, RuntimeErrorCause, RuntimeErrorCode};

/// A refusal as a durable record keeps it: a run's refused terminal and a
/// settled command's refusal or failure. It is the persisted projection of
/// a [`RuntimeError`]: its code, its message and its typed cause, so a
/// reader rebuilds the error with [`RuntimeError::from`] and answers the
/// same typed accessors (`ended_referrer()`, `store_refusal()`,
/// `session_state_version_refusal()`) on every path. A replay-mismatch
/// summary and a direct turn's acceptance belong to the live error, not to
/// the record.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct RecordedRefusal {
    #[schemars(with = "String")]
    pub code: RuntimeErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<RuntimeErrorCause>,
}

impl RecordedRefusal {
    /// The size of the record's stored JSON.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len())
    }
}

impl From<&RuntimeError> for RecordedRefusal {
    fn from(error: &RuntimeError) -> Self {
        Self {
            code: error.code.clone(),
            message: error.message.clone(),
            cause: error.cause.clone(),
        }
    }
}

impl From<RuntimeError> for RecordedRefusal {
    fn from(error: RuntimeError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            cause: error.cause,
        }
    }
}

impl From<RecordedRefusal> for RuntimeError {
    fn from(refusal: RecordedRefusal) -> Self {
        let error = Self::new(refusal.code, refusal.message);
        match refusal.cause {
            Some(cause) => error.with_cause(cause),
            None => error,
        }
    }
}
