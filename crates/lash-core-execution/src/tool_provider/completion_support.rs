/// Why a recorded attempt body can — or cannot — take a durable completion key.
///
/// The coordinator reserves the key before the body runs, so the leaf context
/// can only report a decision already made. Keeping the two refusals apart is
/// the whole point: one is the host's controller, the other is the provider's
/// own missing declaration, and blaming the controller for the latter sends the
/// integrator to the wrong file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptCompletionSupport {
    /// The coordinator reserved a key for a declared deferrer.
    Available,
    /// The provider never declared
    /// [`ToolProvider::attempt_may_defer`](super::ToolProvider::attempt_may_defer)
    /// for this tool, so no key was reserved for it.
    NotDeclared,
    /// The effect controller cannot route await events across process loss.
    ControllerUnsupported,
}

impl AttemptCompletionSupport {
    /// Refuse with the reason that actually applies, so the integrator lands in
    /// the right file: their own provider declaration, or the host's controller.
    pub(crate) fn ensure_available(self) -> Result<(), crate::RuntimeError> {
        match self {
            Self::Available => Ok(()),
            Self::NotDeclared => Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::ToolDeferralNotDeclared,
                "this tool did not declare deferred completion: implement ToolProvider::attempt_may_defer (or StaticToolExecute::attempt_may_defer) and return true for it, so the coordinator reserves a completion key before the attempt body runs",
            )),
            Self::ControllerUnsupported => Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::ToolCompletionKeyProcessLifetime,
                "completion keys require an effect controller with process-loss-safe await-event routing",
            )),
        }
    }
}
