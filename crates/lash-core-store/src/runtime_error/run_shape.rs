//! The typed refusals of a root's shape and of a session's creation config
//! (FIG-4652): their constructors and accessors.

use super::{RuntimeEffectControllerError, RuntimeError, RuntimeErrorCause, RuntimeErrorCode};
use crate::config_transaction::ConfigRefusal;
use crate::run_spec::RunShapeRefusal;

impl RuntimeErrorCause {
    /// The shape refusal `cause` carries, if it is one.
    #[must_use]
    pub fn run_shape_refusal(cause: Option<&Self>) -> Option<&RunShapeRefusal> {
        match cause {
            Some(Self::RunShapeRefused { refusal }) => Some(refusal),
            _ => None,
        }
    }

    /// The creation config refusal `cause` carries, if it is one.
    #[must_use]
    pub fn config_refusal(cause: Option<&Self>) -> Option<&ConfigRefusal> {
        match cause {
            Some(Self::ConfigRefused { refusal }) => Some(refusal),
            _ => None,
        }
    }
}

impl RuntimeEffectControllerError {
    /// A root's refused shape: the root's recorded failure, with its cause
    /// typed. A refused reasoning keeps its own code.
    #[must_use]
    pub fn run_shape_refused(refusal: RunShapeRefusal) -> Self {
        let code = match &refusal {
            RunShapeRefusal::Reasoning { .. } => RuntimeErrorCode::ReasoningRefused,
            RunShapeRefusal::Definition { .. }
            | RunShapeRefusal::Owner { .. }
            | RunShapeRefusal::ReasoningWithoutLlmProfile
            | RunShapeRefusal::ProtocolOptionsWithoutProtocol
            | RunShapeRefusal::Render { .. } => RuntimeErrorCode::RunShapeRefused,
        };
        let mut error = Self::new(code, refusal.to_string());
        error.cause = Some(RuntimeErrorCause::RunShapeRefused {
            refusal: Box::new(refusal),
        });
        error
    }

    /// Why the root's shape was refused. `None` on any other error.
    #[must_use]
    pub fn run_shape_refusal(&self) -> Option<&RunShapeRefusal> {
        RuntimeErrorCause::run_shape_refusal(self.cause.as_ref())
    }
}

impl RuntimeError {
    /// The refusal of the config `session_id`'s creation stated, with its
    /// cause typed.
    #[must_use]
    pub fn session_config_refused(session_id: &crate::SessionId, refusal: ConfigRefusal) -> Self {
        Self::new(
            RuntimeErrorCode::SessionConfigRefused,
            format!("session `{session_id}` config refused at creation: {refusal}"),
        )
        .with_cause(RuntimeErrorCause::ConfigRefused {
            refusal: Box::new(refusal),
        })
    }

    /// Why the root's shape was refused. `None` on any other error.
    #[must_use]
    pub fn run_shape_refusal(&self) -> Option<&RunShapeRefusal> {
        RuntimeErrorCause::run_shape_refusal(self.cause.as_ref())
    }

    /// Why a session's creation config was refused. `None` on any other
    /// error.
    #[must_use]
    pub fn config_refusal(&self) -> Option<&ConfigRefusal> {
        RuntimeErrorCause::config_refusal(self.cause.as_ref())
    }
}
