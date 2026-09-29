/// Why a recorded attempt body can — or cannot — take a durable completion key.
///
/// The coordinator reserves the key before the body runs, so the leaf context
/// can only report a decision already made. Keeping the two refusals apart is
/// the whole point: one is the host's controller, the other is the provider's
/// own missing declaration, and blaming the controller for the latter sends the
/// integrator to the wrong file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttemptCompletionSupport {
    /// The coordinator reserved this key, derived from the call's id, for a
    /// declared deferrer.
    Available(crate::AwaitEventKey),
    /// The provider never declared
    /// [`ToolProvider::attempt_may_defer`](super::ToolProvider::attempt_may_defer)
    /// for this tool, so no key was reserved for it.
    NotDeclared,
    /// The effect controller issues no durable await-event keys.
    ControllerUnsupported,
}

impl AttemptCompletionSupport {
    /// The reserved key, or the refusal that actually applies, so the
    /// integrator lands in the right file: their own provider declaration, or
    /// the host's controller.
    pub(crate) fn key(&self) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        match self {
            Self::Available(key) => Ok(key.clone()),
            Self::NotDeclared => Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::ToolDeferralNotDeclared,
                "this tool did not declare deferred completion: implement ToolProvider::attempt_may_defer (or StaticToolExecute::attempt_may_defer) and return true for it, so the coordinator reserves a completion key before the attempt body runs",
            )),
            Self::ControllerUnsupported => Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::AwaitEventUnsupported,
                "completion keys require an effect controller that issues durable await-event keys",
            )),
        }
    }
}
