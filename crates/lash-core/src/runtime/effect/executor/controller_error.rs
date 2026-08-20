use crate::PluginError;
use crate::runtime::{RuntimeError, RuntimeErrorCode};

use serde::{Deserialize, Serialize};

use super::RuntimeEffectKind;

#[derive(Clone, Debug, thiserror::Error, Serialize, Deserialize)]
#[error("{code}: {message}")]
pub struct RuntimeEffectControllerError {
    pub code: RuntimeErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<crate::runtime::effect::RuntimeEffectReplayMismatchReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<crate::RuntimeErrorCause>,
}

impl RuntimeEffectControllerError {
    /// Constructs a first-party `RuntimeEffectControllerError` from a classified code.
    pub fn new(code: RuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            summary: None,
            cause: None,
        }
    }

    /// Constructs an error minted by a foreign effect-host extension.
    ///
    /// Hosts must namespace these codes and must not mint a built-in
    /// [`RuntimeErrorCode`] spelling. First-party producers use [`Self::new`],
    /// whose typed argument makes an unclassified string a compile error.
    pub fn foreign(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(RuntimeErrorCode::ForeignCode(code.into()), message)
    }

    /// Sets the summary carried by a `RuntimeEffectControllerError` for effect-host implementors
    /// while executing or replaying a runtime effect.
    pub fn with_summary(
        mut self,
        summary: crate::runtime::effect::RuntimeEffectReplayMismatchReport,
    ) -> Self {
        self.summary = Some(summary);
        self
    }

    pub(in crate::runtime::effect) fn wrong_outcome(
        expected: RuntimeEffectKind,
        actual: RuntimeEffectKind,
    ) -> Self {
        Self::new(
            RuntimeErrorCode::RuntimeEffectWrongOutcome,
            format!(
                "expected {} outcome, got {}",
                expected.as_str(),
                actual.as_str()
            ),
        )
    }

    pub(crate) fn into_runtime_error(self) -> RuntimeError {
        let Self {
            code,
            message,
            summary,
            cause,
        } = self;
        let mut runtime = RuntimeError::new(code, message);
        runtime.summary = summary;
        match cause {
            Some(cause) => runtime.with_cause(cause),
            None => runtime,
        }
    }
}

impl From<RuntimeError> for RuntimeEffectControllerError {
    fn from(err: RuntimeError) -> Self {
        Self {
            code: err.code,
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
            err @ PluginError::ProcessNotVisible { .. } => {
                Self::new(RuntimeErrorCode::ProcessNotVisible, err.to_string())
            }
            err @ PluginError::ProcessAlreadyTerminal { .. } => {
                Self::new(RuntimeErrorCode::ProcessAlreadyTerminal, err.to_string())
            }
            err @ PluginError::ProcessNoLongerRetained { .. } => {
                Self::new(RuntimeErrorCode::ProcessNoLongerRetained, err.to_string())
            }
            err => Self::new(RuntimeErrorCode::Plugin, err.to_string()),
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
        let code = match &err {
            crate::StoreError::StoredDataCorrupt { .. }
            | crate::StoreError::MonotonicCounterOverflow { .. } => {
                crate::RuntimeErrorCode::RuntimeStoreCorrupt
            }
            _ => crate::RuntimeErrorCode::RuntimeStore,
        };
        Self {
            code,
            message: err.to_string(),
            summary: None,
            cause,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_target_discriminators_survive_the_effect_controller_boundary() {
        for (error, expected) in [
            (
                PluginError::ProcessNotVisible {
                    process_id: "missing".to_string(),
                },
                RuntimeErrorCode::ProcessNotVisible,
            ),
            (
                PluginError::ProcessAlreadyTerminal {
                    process_id: "done".to_string(),
                    status: crate::ProcessStatus::Completed,
                },
                RuntimeErrorCode::ProcessAlreadyTerminal,
            ),
            (
                PluginError::ProcessNoLongerRetained {
                    terminal_label: "completed".to_string(),
                    pruned_at_ms: 42,
                },
                RuntimeErrorCode::ProcessNoLongerRetained,
            ),
        ] {
            assert_eq!(RuntimeEffectControllerError::from(error).code, expected);
        }
    }

    #[test]
    fn permanent_store_integrity_errors_are_terminal_and_non_retryable() {
        for store_error in [
            crate::StoreError::StoredDataCorrupt {
                record_kind: "RuntimeEffectReplay",
                message: "negative lease_expires_at_ms".to_string(),
            },
            crate::StoreError::MonotonicCounterOverflow {
                counter: "effect_replay_fence",
                current: i64::MAX as u64,
            },
        ] {
            let controller_error = RuntimeEffectControllerError::from(store_error);
            let runtime_error = controller_error.into_runtime_error();
            assert_eq!(
                runtime_error.code,
                crate::RuntimeErrorCode::RuntimeStoreCorrupt
            );
            assert!(!runtime_error.is_retryable());
            assert!(runtime_error.is_terminal());
        }
    }

    #[test]
    fn transient_store_failures_stay_retryable_and_non_terminal() {
        for store_error in [
            crate::StoreError::StorageFailure {
                backend: "sqlite",
                message: "database is locked".to_string(),
            },
            crate::StoreError::Contended,
        ] {
            let controller_error = RuntimeEffectControllerError::from(store_error);
            let runtime_error = controller_error.into_runtime_error();
            assert_eq!(runtime_error.code, crate::RuntimeErrorCode::RuntimeStore);
            assert!(runtime_error.is_retryable());
            assert!(!runtime_error.is_terminal());
        }
    }

    #[test]
    fn foreign_constructor_preserves_extension_code_as_foreign() {
        let runtime_error = RuntimeEffectControllerError::foreign(
            "plugin_defined_abort",
            "extension refused the effect",
        )
        .into_runtime_error();

        assert_eq!(
            runtime_error.code,
            crate::RuntimeErrorCode::ForeignCode("plugin_defined_abort".to_string())
        );
        assert!(!runtime_error.is_retryable());
        assert!(!runtime_error.is_terminal());
    }

    #[test]
    fn replay_mismatch_summary_survives_runtime_error_conversion() {
        let summary = crate::RuntimeEffectReplayMismatchReport {
            divergent_path_count: 2,
            first_divergent_paths: vec![
                "command.duration_ms".to_string(),
                "invocation.replay_key".to_string(),
            ],
        };
        let runtime_error = RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::SqliteEffectReplayHashConflict,
            "recorded runtime effect diverged at command.duration_ms",
        )
        .with_summary(summary.clone())
        .into_runtime_error();

        assert!(runtime_error.code.is_replay_mismatch());
        assert_eq!(runtime_error.summary, Some(summary));
        assert!(runtime_error.to_string().contains("command.duration_ms"));
    }
}
