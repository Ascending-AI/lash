//! The recorded body of a logical root's scope close (FIG-3600 S7, FIG-3607
//! item 7, ADR 0109 §3): it reads the root's terminal evidence and delivers
//! the close the terminal transaction armed — the `ScopeClose` obligation's
//! immediate attempt when the host wires the kind's ledger, else the scope
//! owner itself. It runs only inside the engine's recorded `CloseRootScope`
//! step, and only after the root's final commit wrote that evidence; a
//! replay decodes the evidence it closed and never runs it.
//!
//! A store, ledger or scope owner that did not answer is the attempt's
//! fault, never the step's outcome: the body marks it with derivation retry
//! authority, so an engine runs the close again until it is handed to the
//! obligation ledger (whose retries and stalls it then owns) or the owner
//! acknowledges it.

use std::sync::Arc;

use crate::engine::ScopeCloseSink;
use crate::runtime::drive::deliver_scope_close;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::store::ObligationLedger;
use crate::{
    Clock, RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectOutcome, RuntimeErrorCode, SessionId, SessionStoreFactory, StoreError, TurnId,
};

/// The first execution of one `CloseRootScope` step.
pub(super) struct CloseRootScopeRunner {
    pub(super) store: Arc<dyn crate::store::RuntimePersistence>,
    pub(super) session: SessionId,
    pub(super) root: TurnId,
    pub(super) sink: Arc<dyn ScopeCloseSink>,
    /// The `ScopeClose` kind's ledger when the host wires one (ADR 0109
    /// §3); the close is its obligation's immediate delivery, else the sink
    /// answers directly.
    pub(super) obligations: Option<Arc<dyn ObligationLedger>>,
    /// The catalog the obligation's delivery reads terminal evidence from.
    pub(super) sessions: Arc<dyn SessionStoreFactory>,
    pub(super) clock: Arc<dyn Clock>,
}

fn attempt_fault(context: &str, error: StoreError) -> RuntimeEffectControllerError {
    let mut fault =
        RuntimeEffectControllerError::from(crate::runtime::runtime_error_from_store_commit(error));
    fault.message = format!("{context}: {}", fault.message);
    fault.retryable_uncommitted_derivation()
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for CloseRootScopeRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::CloseRootScope { root } = &envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "root scope close executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if *root != self.root {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "root scope close executor was bound to `{}` but asked to close `{root}`",
                    self.root
                ),
            ));
        }
        let terminal = self
            .store
            .root_terminal(&self.session, &self.root)
            .await
            .map_err(|error| attempt_fault("root terminal read", error))?
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    format!(
                        "root `{}` of session `{}` closes only after its terminal evidence, \
                         and the store holds none",
                        self.root, self.session
                    ),
                )
            })?;
        let relay = super::scope_close::scope_close_relay(
            self.obligations,
            self.sessions,
            Arc::clone(&self.sink),
        );
        deliver_scope_close(
            relay
                .as_ref()
                .map(|relay| relay as &dyn super::relay::ObligationRelay),
            self.sink.as_ref(),
            &terminal,
            self.clock.as_ref(),
        )
        .await
        .map_err(|error| attempt_fault("root scope close", error))?;
        Ok(RuntimeEffectOutcome::CloseRootScope {
            terminal: Box::new(terminal),
        })
    }
}
