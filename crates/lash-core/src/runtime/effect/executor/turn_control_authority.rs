use std::sync::Arc;

use super::control::{AwaitEventResolver, ScopedEffectController};
use crate::RuntimeError;

/// Whether turn-control reads participate in a durable controller journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnControlParticipation {
    Local,
    DurableJournaled,
}

/// Which durable authority owns the reserved turn-control promises.
///
/// Most effect hosts own their cancellation promises together with their
/// journal. The process-local Native host instead delegates only those three
/// reserved waits to the persistent session store when one is available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnControlAuthorityOwner {
    EffectHost,
    SessionStore,
}

/// How turn-control promises are addressed for one turn. Exhaustive: there is
/// no third arrangement, and no field is optional.
pub enum TurnControlAttachment<'a> {
    /// Attach through the same resolver that owns the reserved promises.
    Resolver(&'a dyn AwaitEventResolver),
    /// Attach through an owner-provided durable transport for the same
    /// deployment, such as Restate ingress.
    Dedicated(Arc<dyn crate::TurnAttach>),
}

impl TurnControlAttachment<'_> {
    pub async fn await_terminal(
        &self,
        address: &crate::TurnAddress,
    ) -> Result<crate::TurnTerminal, RuntimeError> {
        match self {
            Self::Resolver(resolver) => {
                crate::runtime::turn_control::await_terminal_from_resolver(*resolver, address).await
            }
            Self::Dedicated(attach) => attach.await_terminal(address).await,
        }
    }
}

pub enum TurnControlBinding<'a> {
    HostOwned {
        binding_id: String,
        resolver: &'a dyn AwaitEventResolver,
        peek: ScopedEffectController<'a>,
        turn_attach: TurnControlAttachment<'a>,
    },
    RunScoped {
        binding_id: String,
        resolver: &'a dyn AwaitEventResolver,
        durable_cancel_after_llm: bool,
        turn_attach: TurnControlAttachment<'a>,
    },
}

impl TurnControlBinding<'_> {
    pub(super) fn host_owned<'a>(
        binding_id: String,
        resolver: &'a dyn AwaitEventResolver,
        peek: ScopedEffectController<'a>,
        turn_attach: Option<Arc<dyn crate::TurnAttach>>,
    ) -> TurnControlBinding<'a> {
        TurnControlBinding::HostOwned {
            binding_id,
            resolver,
            peek,
            turn_attach: turn_attach.map_or(
                TurnControlAttachment::Resolver(resolver),
                TurnControlAttachment::Dedicated,
            ),
        }
    }

    pub(super) fn run_scoped<'a>(
        binding_id: String,
        resolver: &'a dyn AwaitEventResolver,
        durable_cancel_after_llm: bool,
        turn_attach: Option<Arc<dyn crate::TurnAttach>>,
    ) -> TurnControlBinding<'a> {
        TurnControlBinding::RunScoped {
            binding_id,
            resolver,
            durable_cancel_after_llm,
            turn_attach: turn_attach.map_or(
                TurnControlAttachment::Resolver(resolver),
                TurnControlAttachment::Dedicated,
            ),
        }
    }

    pub fn binding_id(&self) -> &str {
        match self {
            Self::HostOwned { binding_id, .. } | Self::RunScoped { binding_id, .. } => binding_id,
        }
    }

    pub fn resolver(&self) -> &dyn AwaitEventResolver {
        match self {
            Self::HostOwned { resolver, .. } | Self::RunScoped { resolver, .. } => *resolver,
        }
    }

    pub fn turn_attach(&self) -> &TurnControlAttachment<'_> {
        match self {
            Self::HostOwned { turn_attach, .. } | Self::RunScoped { turn_attach, .. } => {
                turn_attach
            }
        }
    }
}

/// A reopenable authority for the reserved turn-cancellation promises.
///
/// The resolver is intentionally owned: reopening a session must recover the
/// same database-backed authority without borrowing an effect-host invocation.
#[derive(Clone)]
pub struct TurnCancellationAuthority {
    binding_id: String,
    resolver: Arc<dyn AwaitEventResolver>,
}

impl TurnCancellationAuthority {
    pub fn new(binding_id: impl Into<String>, resolver: Arc<dyn AwaitEventResolver>) -> Self {
        Self {
            binding_id: binding_id.into(),
            resolver,
        }
    }

    pub fn binding_id(&self) -> &str {
        &self.binding_id
    }

    pub fn resolver(&self) -> Arc<dyn AwaitEventResolver> {
        Arc::clone(&self.resolver)
    }
}
