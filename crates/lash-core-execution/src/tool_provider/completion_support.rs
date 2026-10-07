/// Why a recorded attempt body can — or cannot — take a durable completion key.
///
/// The round pins the key's wait when it admits the call, before the body
/// runs, so the leaf context can only report a decision already made. Keeping
/// the two refusals apart is the whole point: one is where the call runs, the
/// other is the provider's own missing declaration, and blaming the runtime
/// for the latter sends the integrator to the wrong file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttemptCompletionSupport {
    /// The key of the completion wait the call's round pinned for a
    /// declared deferrer.
    Available(crate::PinnedKey),
    /// The tool's admitted [`ToolDeclaration`](crate::ToolDeclaration) does not
    /// declare `may_defer`, so no key was reserved for it.
    NotDeclared,
    /// The call runs where no completion wait is pinned for it: outside a
    /// turn's tool round.
    ControllerUnsupported,
}

impl AttemptCompletionSupport {
    /// The reserved key, or the refusal that actually applies, so the
    /// integrator lands in the right file: their own provider declaration, or
    /// the host's controller.
    pub(crate) fn key(&self) -> Result<crate::PinnedKey, crate::RuntimeError> {
        match self {
            Self::Available(key) => Ok(key.clone()),
            Self::NotDeclared => Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::ToolDeferralNotDeclared,
                "this tool did not declare deferred completion: declare `may_defer` in its manifest's ToolDeclaration (ToolDefinition::with_declaration(ToolDeclaration::deferring())), so its round pins a completion wait before the attempt body runs",
            )),
            Self::ControllerUnsupported => Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::AwaitEventUnsupported,
                "completion keys are pinned only for the members of a turn's tool round",
            )),
        }
    }
}
