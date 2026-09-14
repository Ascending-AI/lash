use std::sync::Arc;

use super::control::{AwaitEventResolver, ScopedEffectController};
use crate::RuntimeError;









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

    /// Erase this authority for the store seam.
    ///
    /// [`crate::store::TurnInputStore::turn_cancellation_authority`] hands back
    /// the object-safe [`lash_core_store::turn_control_binding::StoreTurnCancellationAuthority`]
    /// because the concrete authority owns an `AwaitEventResolver`, which is
    /// runtime machinery the store layer cannot depend on.
    pub fn into_store_authority(
        self,
    ) -> Arc<dyn lash_core_store::turn_control_binding::StoreTurnCancellationAuthority> {
        Arc::new(self)
    }

    pub fn resolver(&self) -> Arc<dyn AwaitEventResolver> {
        Arc::clone(&self.resolver)
    }

    /// Finish one exact closure operation previously authorized by the store.
    ///
    /// This grants no store mutation authority: the promise pair may be
    /// settled after owner takeover, while applying input effects and consuming
    /// the authorization still requires the successor's current store fence.
    pub async fn settle_authorized_closure(
        &self,
        authorization: &crate::TurnCancelClosureAuthorization,
    ) -> Result<crate::TurnCancelClosureSettlement, RuntimeError> {
        authorization.validate()?;
        let expected_binding =
            crate::turn_control_binding_id_for_scope(&self.binding_id, authorization.admitted_scope())?;
        if authorization.binding_id() != expected_binding {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                format!(
                    "turn cancellation closure binding `{}` does not match authority `{}`",
                    authorization.binding_id(),
                    expected_binding
                ),
            ));
        }
        let control = match crate::runtime::turn_control::ActiveTurnControl::new(
            self.resolver.as_ref(),
            authorization.address(),
        )
        .await
        {
            Ok(control) => control,
            Err(error) if error.code == crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked => {
                return Err(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::TurnControlUnknownOrRevoked,
                    format!(
                        "turn `{}` in session `{}` was revoked before authorized closure settlement",
                        authorization.turn_id(),
                        authorization.session_id()
                    ),
                ));
            }
            Err(error) => return Err(error),
        };
        control
            .settle_authorized(self.resolver.as_ref(), authorization)
            .await
    }
}

impl lash_core_store::turn_control_binding::StoreTurnCancellationAuthority
    for TurnCancellationAuthority
{
    fn binding_id(&self) -> &str {
        self.binding_id()
    }
}

/// Recover the concrete authority a store handed back through the seam.
///
/// `TurnCancellationAuthority` is the sole implementor, and only this crate
/// constructs one, so a handle that is anything else is a programming error.
pub fn concrete_turn_cancellation_authority(
    handle: &Arc<dyn lash_core_store::turn_control_binding::StoreTurnCancellationAuthority>,
) -> TurnCancellationAuthority {
    let any: &dyn std::any::Any = handle.as_ref();
    any.downcast_ref::<TurnCancellationAuthority>()
        .expect("a store turn-cancellation authority is always a TurnCancellationAuthority")
        .clone()
}

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





#[cfg(test)]
mod tests {
    use lash_core_store::turn_control_binding::{admitted_turn_cancel_scope, turn_control_binding_id_for_scope};

    #[test]
    fn orphan_recovery_uses_persisted_turn_address_for_session_scopes() {
        let address = crate::TurnAddress::new("session", "original-turn");
        let successor_drain = crate::ExecutionScope::queue_drain("session", "successor-drain");

        assert_eq!(
            admitted_turn_cancel_scope(&address, &successor_drain, "test-authority"),
            address.execution_scope()
        );
    }

    #[test]
    fn orphan_recovery_preserves_previously_admitted_physical_scope() {
        let address = crate::TurnAddress::new("session", "turn");
        let process_scope = crate::ExecutionScope::process("original-process");

        assert_eq!(
            admitted_turn_cancel_scope(
                &address,
                &process_scope,
                &turn_control_binding_id_for_scope("test-authority", &process_scope).unwrap()
            ),
            process_scope
        );
    }
}
