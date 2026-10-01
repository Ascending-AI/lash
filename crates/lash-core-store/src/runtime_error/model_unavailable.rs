//! The typed fault of a recorded model a worker cannot bind (FIG-4404): its
//! constructor, its accessors, and the record that carries it through an
//! engine that keeps only a failed attempt's text.

use serde::{Deserialize, Serialize};

use super::{RuntimeEffectControllerError, RuntimeError, RuntimeErrorCause, RuntimeErrorCode};
use crate::RuntimeEffectKind;

impl RuntimeErrorCause {
    /// Whether an error carrying this cause is settled, whatever its code.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::ModelUnavailable { .. })
    }

    /// The recorded model key `cause` names, if it is an unbound model.
    #[must_use]
    pub fn model_key(cause: Option<&Self>) -> Option<&crate::ModelKey> {
        match cause {
            Some(Self::ModelUnavailable { model_key }) => Some(model_key),
            _ => None,
        }
    }
}

impl RuntimeError {
    pub(super) fn has_terminal_cause(&self) -> bool {
        self.cause
            .as_ref()
            .is_some_and(RuntimeErrorCause::is_terminal)
    }

    /// The recorded model key this error could not bind, when it is the
    /// typed fault of an unbound model (FIG-4404).
    #[must_use]
    pub fn model_key(&self) -> Option<&crate::ModelKey> {
        RuntimeErrorCause::model_key(self.cause.as_ref())
    }

    /// [`RuntimeEffectControllerError::attempt_failure_text`], for a fault
    /// that ends its attempt as a runtime error.
    #[must_use]
    pub fn attempt_failure_text(&self) -> String {
        AttemptFault::failure_text(self, self.model_key())
    }
}

impl RuntimeEffectControllerError {
    pub(super) fn has_terminal_cause(&self) -> bool {
        self.cause
            .as_ref()
            .is_some_and(RuntimeErrorCause::is_terminal)
    }

    /// Whether this is an unbound model met by one of the two effects whose
    /// body binds it: the one fault of theirs that is never a recorded result.
    pub(super) fn is_unbound_model_call(&self, kind: RuntimeEffectKind) -> bool {
        self.code == RuntimeErrorCode::ModelUnavailable
            && matches!(kind, RuntimeEffectKind::LlmCall | RuntimeEffectKind::Direct)
    }

    /// A model key this worker's deployment does not serve (FIG-4404): the
    /// one way the fault is built, so it always names the key typed, beside
    /// the message, and is always the attempt's fault. The key was adopted
    /// when it was recorded, so the deployment is at fault, never the
    /// recorded work: the engine ends the attempt, records nothing, and runs
    /// it again until a deployment serves the key.
    #[must_use]
    pub fn model_unavailable(model_key: &crate::ModelKey, message: impl Into<String>) -> Self {
        let mut error = Self::new(RuntimeErrorCode::ModelUnavailable, message);
        error.cause = Some(RuntimeErrorCause::ModelUnavailable {
            model_key: Box::new(model_key.clone()),
        });
        error.retryable_uncommitted_derivation()
    }

    /// The text an engine fails a retried attempt with: this error's display
    /// and, for a fault that carries typed facts, their record
    /// ([`AttemptFault`]). An engine keeps only the text of a failed attempt,
    /// so the record is how the typed fault reaches the park the engine's
    /// exhausted retries become ([`AttemptFault::in_failure`]).
    #[must_use]
    pub fn attempt_failure_text(&self) -> String {
        AttemptFault::failure_text(self, self.model_key())
    }

    /// The recorded model key this error could not bind, when it is the
    /// typed fault of an unbound model (FIG-4404).
    #[must_use]
    pub fn model_key(&self) -> Option<&crate::ModelKey> {
        RuntimeErrorCause::model_key(self.cause.as_ref())
    }
}

/// The typed facts of an attempt's fault, as the JSON record an engine's
/// failure text carries beside the message
/// ([`RuntimeEffectControllerError::attempt_failure_text`]). A park written
/// from the engine's exhausted retries decodes it, so the fault stays typed
/// across the engine instead of being read back out of prose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fault")]
pub(crate) enum AttemptFault {
    /// A recorded model the worker could not bind (FIG-4404).
    #[serde(rename = "lash.model_unavailable")]
    ModelUnavailable { model_key: crate::ModelKey },
}

impl AttemptFault {
    /// `error`'s display and, when it could not bind `model_key`, the
    /// record of that key.
    fn failure_text(error: &dyn std::fmt::Display, model_key: Option<&crate::ModelKey>) -> String {
        let record = model_key.and_then(|model_key| {
            serde_json::to_string(&Self::ModelUnavailable {
                model_key: model_key.clone(),
            })
            .ok()
        });
        match record {
            Some(record) => format!("{error} {record}"),
            None => error.to_string(),
        }
    }

    /// The record `failure` carries, found where it starts: an engine
    /// prefixes the text with its own words.
    pub(crate) fn in_failure(failure: &str) -> Option<Self> {
        let record = failure.get(failure.find(r#"{"fault":"lash."#)?..)?;
        serde_json::Deserializer::from_str(record)
            .into_iter::<Self>()
            .next()?
            .ok()
    }
}
