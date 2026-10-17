use crate::tool_intent::{StartProcessIntent, StartProcessIntentWire};
/// The one process start a pending call declares, and whose terminal resolves
/// the call.
///
/// A pending attempt carries no [`ToolIntents`](crate::tool_intent::ToolIntents), so a
/// deferred call cannot declare a start the ordinary way. This is the one
/// start it may declare instead. It is sealed: the only constructor validates
/// the start against the attempt that declares it, and it holds exactly one
/// [`StartProcessIntent`](crate::tool_intent::StartProcessIntent). Its intent identity is
/// the declaring attempt's identity for index 0, so its start key is
/// [`StartKeyDerivation::for_tool_intent`](lash_core_store::process_identity::StartKeyDerivation::for_tool_intent) of that
/// identity and every redrive reaches the same child.
///
/// Only the attempt whose `Pending` the runtime records ever launches: a
/// retried or superseded attempt's declaration is discarded with it.
///
/// It decodes, because it is durable, and so a decoded declaration is bound
/// to the admitted call again before it launches: a start naming another
/// session, or an identity that is not its declaring attempt's for index 0,
/// is refused with nothing registered.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(from = "DeclaredStartWire")]
pub struct DeclaredStart {
    // Both boxed: a pending completion rides every recorded attempt launch.
    start: Box<crate::tool_intent::StartProcessIntent>,
    identity: Box<crate::ToolIntentIdentity>,
}

/// What a [`DeclaredStart`] decodes from: its start through the
/// crate-private stored form.
#[derive(serde::Deserialize)]
struct DeclaredStartWire {
    start: Box<StartProcessIntentWire>,
    identity: Box<crate::ToolIntentIdentity>,
}

impl From<DeclaredStartWire> for DeclaredStart {
    fn from(wire: DeclaredStartWire) -> Self {
        Self {
            start: wire.start.into(),
            identity: wire.identity,
        }
    }
}

impl PartialEq for DeclaredStart {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
            && serde_json::to_value(&self.start).ok() == serde_json::to_value(&other.start).ok()
    }
}

impl DeclaredStart {
    /// The declared start.
    pub fn start(&self) -> &crate::tool_intent::StartProcessIntent {
        &self.start
    }

    /// The declaring attempt's intent identity for the start.
    pub fn identity(&self) -> &crate::ToolIntentIdentity {
        &self.identity
    }
}

/// Why an admitted attempt refused its declared start.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeclaredStartRefused {
    #[error("a declared start must name the declaring owner")]
    ForeignOwner,
    #[error("a declared start needs a completion key")]
    CompletionUnavailable,
}

/// Refuses a declaration that does not belong to the call that admitted
/// it (ADR 0116 §3.1).
///
/// A declared start is durable, so it decodes without its constructor,
/// and a decoded one names whatever owner and identity its bytes say.
/// `declaring` is the identity the runtime derived for index 0 from the
/// admitted call itself: its owner, execution scope, `ToolCallId` and
/// the attempt emission that minted the declaration. The start must name
/// that owner, and its identity must be exactly that one, or the launch
/// would register a child under another call's key, another owner's, or
/// one its own fields do not derive.
#[doc(hidden)]
pub fn bound_declared_start(
    start: &DeclaredStart,
    declaring: &crate::ToolIntentIdentity,
) -> Result<(), crate::tool_intent_outcome::ToolIntentRefusalReason> {
    if start.start.owner != declaring.owner {
        return Err(
            crate::tool_intent_outcome::ToolIntentRefusalReason::OwnerMismatch {
                expected: declaring.owner.clone(),
                recorded: start.start.owner.clone(),
            },
        );
    }
    if *start.identity != *declaring {
        return Err(
            crate::tool_intent_outcome::ToolIntentRefusalReason::DeclaredStartIdentityMismatch {
                expected: Box::new(declaring.clone()),
                recorded: start.identity.clone(),
            },
        );
    }
    Ok(())
}

/// The start request realization presents: the declaration under its
/// derived key.
#[doc(hidden)]
pub fn declared_start_request(start: &DeclaredStart) -> crate::process::ProcessStartRequest {
    start.start.into_request(&start.identity)
}

/// Construct the declaration already admitted by its execution-owned attempt.
#[doc(hidden)]
pub fn declared_start(
    start: StartProcessIntent,
    identity: crate::ToolIntentIdentity,
) -> DeclaredStart {
    DeclaredStart {
        start: Box::new(start),
        identity: Box::new(identity),
    }
}
