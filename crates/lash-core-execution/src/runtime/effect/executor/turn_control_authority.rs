use crate::ActorContext;

use crate::RuntimeError;

/// A reopenable authority for the reserved turn-cancellation promises.
///
/// The resolver is intentionally owned: reopening a session must recover the
/// same database-backed authority without borrowing an effect-host invocation.
#[derive(Clone)]
pub struct TurnCancellationAuthority {
    binding_id: String,
    resolver: ActorContext,
}
impl TurnCancellationAuthority {
    pub fn new(binding_id: impl Into<String>, resolver: ActorContext) -> Self {
        Self {
            binding_id: binding_id.into(),
            resolver,
        }
    }

    pub fn binding_id(&self) -> &str {
        &self.binding_id
    }

    pub fn resolver(&self) -> ActorContext {
        self.resolver.clone()
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
        let expected_binding = crate::turn_control_binding_id_for_scope(
            &self.binding_id,
            authorization.admitted_scope(),
        )?;
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
            &self.resolver,
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
            .settle_authorized(&self.resolver, authorization, None)
            .await
    }
}

/// How turn-control promises are addressed for one turn: through the
/// resolver that owns the reserved promises. The dedicated transport
/// (Restate ingress) is gone with `turn_attach`, folded to `None` (I0).
pub enum TurnControlAttachment<'a> {
    /// Attach through the same resolver that owns the reserved promises.
    Resolver(&'a ActorContext),
}

impl TurnControlAttachment<'_> {
    pub async fn await_terminal(
        &self,
        address: &crate::TurnAddress,
    ) -> Result<crate::TurnTerminal, RuntimeError> {
        match self {
            Self::Resolver(resolver) => {
                crate::runtime::turn_control::await_terminal_from_resolver(resolver, address).await
            }
        }
    }
}

/// The turn-control promises one run observes: the authority's binding, the
/// run-scoped resolver that owns the reserved promises, and how a caller
/// attaches to the turn's terminal.
///
/// Every host journals its effects, so the resolver is the run's own
/// journaled controller, and a cancellation observed after an LLM call is
/// durable on every host.
pub struct TurnControlBinding<'a> {
    binding_id: String,
    resolver: &'a ActorContext,
    turn_attach: TurnControlAttachment<'a>,
}

impl TurnControlBinding<'_> {
    pub fn binding_id(&self) -> &str {
        &self.binding_id
    }

    pub fn resolver(&self) -> &ActorContext {
        self.resolver
    }

    pub fn turn_attach(&self) -> &TurnControlAttachment<'_> {
        &self.turn_attach
    }
}

#[cfg(test)]
mod tests {
    use lash_core_store::turn_control_binding::{
        admitted_turn_cancel_scope, turn_control_binding_id_for_scope,
    };

    #[test]
    fn orphan_recovery_uses_persisted_turn_address_for_session_scopes() {
        let address = crate::TurnAddress::new("session", "original-turn");
        let successor_drain =
            crate::ExecutionScope::session_operation("session", "successor-drain");

        assert_eq!(
            admitted_turn_cancel_scope(&address, &successor_drain, "test-authority"),
            address.execution_scope()
        );
    }

    #[test]
    fn orphan_recovery_preserves_previously_admitted_physical_scope() {
        let address = crate::TurnAddress::new("session", "turn");
        let process_scope =
            crate::ExecutionScope::process(crate::process_id_for_test("original-process"));

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
