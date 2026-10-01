//! The typed causes a runtime error carries beside its code.

use super::{IngressReservedSourceKeyRefusal, RuntimeEffectControllerError, RuntimeErrorCode};
use crate::SessionId;

/// Typed cause retained when a controller-owned runtime effect must abort
/// through the generic runtime error boundary.
///
/// An attachment retention cause keeps the attachment store's retry class.
/// Other causes are terminal except for an unavailable recorded model.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RuntimeErrorCause {
    /// The attachment-store family and structured source of a required retention failure.
    AttachmentRetention {
        failure: Box<super::AttachmentRetentionFailure>,
    },
    IngressReservedSourceKey {
        #[serde(flatten)]
        refusal: Box<IngressReservedSourceKeyRefusal>,
    },
    StoreRefusal {
        refusal: Box<crate::store::StoreRefusal>,
    },
    /// A durable record cannot be decoded. Retrying cannot repair its bytes.
    StoredDataCorrupt {
        #[serde(flatten)]
        corruption: Box<StoredDataCorruption>,
    },
    SessionDeleted {
        session_id: SessionId,
    },
    /// An artifact publish or acquire named a referrer that has a fence
    /// (ADR 0113 §2.7): the typed half of
    /// [`RuntimeErrorCode::ArtifactReferrerEnded`].
    ArtifactReferrerEnded {
        referrer: Box<crate::artifact_referrer::ArtifactReferrer>,
    },
    /// The typed half of [`RuntimeErrorCode::RuntimeEffectGroupChildUnroutable`].
    EffectGroupChildUnroutable {
        missing: GroupChildCapability,
    },
    /// The recorded model key this worker could not bind (FIG-4404).
    /// A deployment serving the key repairs this retryable cause.
    ModelUnavailable {
        model_key: Box<crate::ModelKey>,
    },
    /// A tool call passed the session's recorded `max_tool_calls`
    /// (FIG-4546): the typed half of
    /// [`RuntimeErrorCode::MaxToolCallsExceeded`].
    MaxToolCallsExceeded {
        exceeded: Box<crate::ToolCallLimitExceeded>,
    },
}

/// The record kind and diagnostic retained when durable data cannot be decoded.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredDataCorruption {
    pub record_kind: String,
    pub message: String,
}

impl RuntimeErrorCause {
    /// The fenced referrer `cause` names, if it is an ended-referrer refusal.
    #[must_use]
    pub fn ended_referrer(
        cause: Option<&Self>,
    ) -> Option<&crate::artifact_referrer::ArtifactReferrer> {
        match cause {
            Some(Self::ArtifactReferrerEnded { referrer }) => Some(referrer),
            _ => None,
        }
    }
}

/// A capability a deployment needs to execute an effect-group child, and
/// whose absence is a fact of the deployment's wiring rather than of one
/// attempt (FIG-4550).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GroupChildCapability {
    /// The deployment's builder of a tool child's dispatch context when the
    /// child's opener is not live where it runs.
    ToolChildContextSource,
    /// The resolver that maps a recorded group child to the code that runs
    /// it: the host serves the lane with none registered.
    GroupExecutors,
}

impl GroupChildCapability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolChildContextSource => "tool_child_context_source",
            Self::GroupExecutors => "group_executors",
        }
    }
}

impl std::fmt::Display for GroupChildCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl RuntimeEffectControllerError {
    /// The typed refusal of an effect-group child that the deployment serving
    /// its lane can never execute, naming the capability it lacks (FIG-4550).
    pub fn group_child_unroutable(
        missing: GroupChildCapability,
        message: impl Into<String>,
    ) -> Self {
        let mut error = Self::new(RuntimeErrorCode::RuntimeEffectGroupChildUnroutable, message);
        error.cause = Some(RuntimeErrorCause::EffectGroupChildUnroutable { missing });
        error
    }
}
