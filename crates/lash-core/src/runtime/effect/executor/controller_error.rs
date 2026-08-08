use crate::PluginError;
use crate::runtime::{RuntimeError, RuntimeErrorCode};

use serde::{Deserialize, Serialize};

use super::RuntimeEffectKind;

#[derive(Clone, Debug, thiserror::Error, Serialize, Deserialize)]
#[error("{code}: {message}")]
pub struct RuntimeEffectControllerError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<crate::runtime::effect::RuntimeEffectReplayMismatchSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<crate::RuntimeErrorCause>,
}

impl RuntimeEffectControllerError {
    /// Constructs a `RuntimeEffectControllerError` for effect-host implementors while executing or
    /// replaying a runtime effect.
    ///
    /// This is an intentionally open extension point. Hosts should namespace
    /// their codes and must not mint a built-in [`RuntimeErrorCode`] string;
    /// built-in strings are canonicalized, and unknown codes cross the runtime
    /// boundary as [`RuntimeErrorCode::ForeignCode`].
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            summary: None,
            cause: None,
        }
    }

    /// Sets the summary carried by a `RuntimeEffectControllerError` for effect-host implementors
    /// while executing or replaying a runtime effect.
    pub fn with_summary(
        mut self,
        summary: crate::runtime::effect::RuntimeEffectReplayMismatchSummary,
    ) -> Self {
        self.summary = Some(summary);
        self
    }

    pub(in crate::runtime::effect) fn wrong_outcome(
        expected: RuntimeEffectKind,
        actual: RuntimeEffectKind,
    ) -> Self {
        Self::new(
            "runtime_effect_wrong_outcome",
            format!(
                "expected {} outcome, got {}",
                expected.as_str(),
                actual.as_str()
            ),
        )
    }

    pub(crate) fn into_runtime_error(self) -> RuntimeError {
        let runtime = RuntimeError::new(RuntimeErrorCode::from_wire_code(&self.code), self.message);
        match self.cause {
            Some(cause) => runtime.with_cause(cause),
            None => runtime,
        }
    }
}

impl From<RuntimeError> for RuntimeEffectControllerError {
    fn from(err: RuntimeError) -> Self {
        Self {
            code: err.code.as_str().to_string(),
            message: err.message,
            summary: None,
            cause: err.cause,
        }
    }
}

impl From<PluginError> for RuntimeEffectControllerError {
    fn from(err: PluginError) -> Self {
        match err {
            PluginError::RuntimeEffectController(err) => err,
            err => Self::new("plugin", err.to_string()),
        }
    }
}

impl From<crate::StoreError> for RuntimeEffectControllerError {
    fn from(err: crate::StoreError) -> Self {
        let cause = match &err {
            crate::StoreError::SessionDeleted { session_id } => {
                Some(crate::RuntimeErrorCause::SessionDeleted {
                    session_id: session_id.clone(),
                })
            }
            _ => None,
        };
        Self {
            code: "runtime_store".to_string(),
            message: err.to_string(),
            summary: None,
            cause,
        }
    }
}
