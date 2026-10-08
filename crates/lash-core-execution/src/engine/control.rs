//! The engine control vocabulary: run references and refusals.

use crate::store::StoreError;
use crate::{SessionId, TurnId};

/// One logical run, as an engine's control verbs address it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RunRef {
    pub session: SessionId,
    pub run: TurnId,
}

/// Whether the identical ask may succeed when it is made again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefusalClass {
    /// A fault of this attempt: the obligation is retried after a backoff.
    Retryable,
    /// The engine's answer to the ask: making it again is refused the same
    /// way.
    Permanent,
}

/// Why an engine could not carry out a control verb or accept a shift: its
/// retry class beside the typed code of its cause. A retryable refusal is
/// retained on the `ControlIntent` and retried after a backoff; a permanent
/// one refuses the intent, visible to an operator under its code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct EngineRefusal {
    pub disposition: RefusalClass,
    pub code: crate::RuntimeErrorCode,
    pub message: String,
}

impl EngineRefusal {
    /// A fault of this attempt under `code`.
    #[must_use]
    pub fn retryable(code: crate::RuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            disposition: RefusalClass::Retryable,
            code,
            message: message.into(),
        }
    }

    /// The engine's answer for good under `code`.
    #[must_use]
    pub fn permanent(code: crate::RuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            disposition: RefusalClass::Permanent,
            code,
            message: message.into(),
        }
    }

    /// Whether the ask is worth another attempt.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.disposition == RefusalClass::Retryable
    }

    /// The cause as an obligation row and a refused intent retain it.
    #[must_use]
    pub fn into_delivery_error(self) -> crate::store::DeliveryError {
        crate::store::DeliveryError::new(self.code, self.message)
    }
}

/// A store's answer keeps the retry class of its canonical runtime code.
impl From<StoreError> for EngineRefusal {
    fn from(error: StoreError) -> Self {
        let disposition = if error.runtime_code().is_retryable() {
            RefusalClass::Retryable
        } else {
            RefusalClass::Permanent
        };
        let crate::store::DeliveryError { code, message } = error.into();
        Self {
            disposition,
            code,
            message,
        }
    }
}

/// A plugin's answer keeps its class. A redrive needs fresh authority, so
/// repeating the identical control request cannot repair it.
impl From<crate::PluginError> for EngineRefusal {
    fn from(error: crate::PluginError) -> Self {
        let disposition = match error.class() {
            crate::PluginErrorClass::Retryable => RefusalClass::Retryable,
            crate::PluginErrorClass::Redrivable | crate::PluginErrorClass::Terminal => {
                RefusalClass::Permanent
            }
        };
        let message = error.to_string();
        Self {
            disposition,
            code: crate::RuntimeEffectControllerError::from(error).code,
            message,
        }
    }
}

#[cfg(test)]
mod refusal_classification_tests {
    use super::*;

    #[test]
    fn engine_refusal_retries_only_retryable_plugin_operations() {
        for (error, plugin_error) in StoreError::samples_for_testing()
            .into_iter()
            .zip(StoreError::samples_for_testing())
        {
            let code = error.runtime_code();
            let plugin = crate::PluginError::from(plugin_error);
            let plugin_code = crate::RuntimeEffectControllerError::from(plugin.clone()).code;
            let through_plugin = EngineRefusal::from(plugin);
            let refusal = EngineRefusal::from(error);
            assert_eq!(refusal.code, code);
            assert_eq!(refusal.is_retryable(), code.is_retryable(), "{refusal:?}");
            assert_eq!(refusal.disposition, through_plugin.disposition);
            assert_eq!(through_plugin.code, plugin_code);
        }
        for plugin in [
            crate::PluginError::from(StoreError::Contended),
            crate::PluginError::from(StoreError::StoredDataCorrupt {
                record_kind: "process record",
                message: "invalid JSON".into(),
            }),
            crate::PluginError::SessionExecutionLeaseLost {
                session_id: SessionId::fixture("lost-authority"),
            },
            crate::PluginError::ProcessExecutionSuperseded {
                process_id: crate::process_id_for_test("superseded-process"),
            },
        ] {
            let expected = plugin.class() == crate::PluginErrorClass::Retryable;
            let code = crate::RuntimeEffectControllerError::from(plugin.clone()).code;
            let refusal = EngineRefusal::from(plugin);
            assert_eq!(refusal.code, code);
            assert_eq!(refusal.is_retryable(), expected, "{refusal:?}");
        }
    }
}
