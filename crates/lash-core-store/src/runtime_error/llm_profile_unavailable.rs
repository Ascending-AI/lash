//! The typed fault of a recorded model a worker cannot bind (FIG-4404): its
//! constructor and its accessors, and the one record that carries a typed
//! error through an engine that keeps only text.

use serde::{Deserialize, Serialize};

use super::{RuntimeEffectControllerError, RuntimeError, RuntimeErrorCause, RuntimeErrorCode};
use crate::RuntimeEffectKind;

impl RuntimeErrorCause {
    /// Whether an error carrying this cause is settled, whatever its code.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::PluginOperation { .. } | Self::PluginHooks { .. } => {
                self.plugin_failure_class() == Some(lash_sansio::PluginFailureClass::Terminal)
            }
            Self::VmWorker { outcome } => {
                !outcome.is_retryable() && outcome.deployment_fault().is_none()
            }
            Self::LlmProfileUnavailable { .. }
            | Self::CellSnapshotUndecodable { .. }
            | Self::PluginExecution { .. }
            | Self::PluginStatePublicationFenced { .. } => false,
            Self::AttachmentRetention { failure } => !failure.is_retryable(),
            Self::ToolRunCutRefused { .. }
            | Self::ToolRunIsolationRefused { .. }
            | Self::ToolRunControl { .. }
            | Self::ToolRunAdmissionRefused { .. }
            | Self::MaterialRefused { .. }
            | Self::ProviderFailure { .. }
            | Self::IngressReservedSourceKey { .. }
            | Self::Compat { .. }
            | Self::StoreRefusal { .. }
            | Self::StoredDataCorrupt { .. }
            | Self::ModuleArtifactRefused { .. }
            | Self::MissingRecordedProcessConfig { .. }
            | Self::SessionDeleted { .. }
            | Self::ArtifactReferrerEnded { .. }
            | Self::RunShapeRefused { .. }
            | Self::ConfigRefused { .. }
            | Self::ToolSourcesUnavailable { .. }
            | Self::MaxToolCallsExceeded { .. }
            | Self::PluginStateUnrecorded { .. }
            | Self::PluginStateEffectOwnerMismatch
            | Self::PluginStateFrontier { .. }
            | Self::PluginFormat { .. }
            | Self::ProcessParentEnded { .. }
            | Self::ProcessStartKeyConflict { .. }
            | Self::SchemaRefused { .. }
            | Self::ToolSchemaRefused { .. }
            | Self::ValueMismatch { .. } => true,
        }
    }

    /// The recorded model key `cause` names, if it is an unbound model.
    #[must_use]
    pub fn profile_key(cause: Option<&Self>) -> Option<&crate::LlmProfileKey> {
        match cause {
            Some(Self::LlmProfileUnavailable { profile_key }) => Some(profile_key),
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
    pub fn profile_key(&self) -> Option<&crate::LlmProfileKey> {
        RuntimeErrorCause::profile_key(self.cause.as_ref())
    }

    /// [`RuntimeEffectControllerError::attempt_failure_text`], for a fault
    /// that ends its attempt as a runtime error.
    #[must_use]
    pub fn attempt_failure_text(&self) -> String {
        RuntimeEffectControllerError::from(self.clone()).attempt_failure_text()
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
    pub(super) fn is_unbound_llm_profile_call(&self, kind: RuntimeEffectKind) -> bool {
        self.code == RuntimeErrorCode::LlmProfileUnavailable
            && matches!(kind, RuntimeEffectKind::LlmCall | RuntimeEffectKind::Direct)
    }

    /// A model key this worker's deployment does not serve (FIG-4404): the
    /// one way the fault is built, so it always names the key typed, beside
    /// the message, and is always the attempt's fault. The key was adopted
    /// when it was recorded, so the deployment is at fault, never the
    /// recorded work: the engine ends the attempt, records nothing, and runs
    /// it again until a deployment serves the key.
    #[must_use]
    pub fn llm_profile_unavailable(
        profile_key: &crate::LlmProfileKey,
        message: impl Into<String>,
    ) -> Self {
        let mut error = Self::new(RuntimeErrorCode::LlmProfileUnavailable, message);
        error.cause = Some(RuntimeErrorCause::LlmProfileUnavailable {
            profile_key: Box::new(profile_key.clone()),
        });
        error.retryable_uncommitted_derivation()
    }

    /// The text an engine fails a retried attempt with: this error's display
    /// and, for a fault that carries a typed cause, its record
    /// ([`Self::to_record`]). An engine keeps only the text of a failed
    /// attempt, so the record is how the typed fault reaches the park the
    /// engine's exhausted retries become ([`Self::in_text`]).
    #[must_use]
    pub fn attempt_failure_text(&self) -> String {
        if self.cause.is_some() {
            format!("{self} {}", self.to_record())
        } else {
            self.to_string()
        }
    }

    /// This error as the one record a typed Lash error travels as through an
    /// engine that keeps only text: a handler's terminal error message, or a
    /// failed attempt's failure. [`Self::in_text`] reads it back.
    #[must_use]
    pub fn to_record(&self) -> String {
        serde_json::to_string(&ErrorRecord {
            error: std::borrow::Cow::Borrowed(self),
        })
        .unwrap_or_else(|_| self.to_string())
    }

    /// The error `text` carries as a record ([`Self::to_record`]), found
    /// where it starts: an engine prefixes the text with its own words.
    #[must_use]
    pub fn in_text(text: &str) -> Option<Self> {
        let record = text.get(text.find(ERROR_RECORD_PREFIX)?..)?;
        serde_json::Deserializer::from_str(record)
            .into_iter::<ErrorRecord<'static>>()
            .next()?
            .ok()
            .map(|record| record.error.into_owned())
    }

    /// The recorded model key this error could not bind, when it is the
    /// typed fault of an unbound model (FIG-4404).
    #[must_use]
    pub fn profile_key(&self) -> Option<&crate::LlmProfileKey> {
        RuntimeErrorCause::profile_key(self.cause.as_ref())
    }
}

/// How a record starts: the tag no other text an engine keeps begins with.
const ERROR_RECORD_PREFIX: &str = r#"{"lash.error":"#;

/// The one tagged record of a typed Lash error in text
/// ([`RuntimeEffectControllerError::to_record`]).
#[derive(Serialize, Deserialize)]
struct ErrorRecord<'a> {
    #[serde(rename = "lash.error")]
    error: std::borrow::Cow<'a, RuntimeEffectControllerError>,
}
