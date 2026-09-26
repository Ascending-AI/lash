//! The recorded body of a logical root's scope close (FIG-3600 S7, FIG-3607
//! item 7): it reads the root's terminal evidence and hands it to the host's
//! scope owner. It runs only inside the engine's recorded `CloseRootScope`
//! step, and only after the root's final commit wrote that evidence; a replay
//! decodes the evidence it closed and never runs it.
//!
//! A store or scope owner that did not answer is the attempt's fault, never
//! the step's outcome: the body marks it with derivation retry authority, so
//! an engine runs the close again until the owner acknowledges it.

use std::sync::Arc;

use crate::engine::ScopeCloseSink;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::{
    RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectOutcome, RuntimeErrorCode, SessionId, StoreError, TurnId,
};

/// The first execution of one `CloseRootScope` step.
pub(super) struct CloseRootScopeRunner {
    pub(super) store: Arc<dyn crate::store::RuntimePersistence>,
    pub(super) session: SessionId,
    pub(super) root: TurnId,
    pub(super) sink: Arc<dyn ScopeCloseSink>,
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
        self.sink
            .close_root_scope(&terminal)
            .await
            .map_err(|error| attempt_fault("root scope close", error))?;
        Ok(RuntimeEffectOutcome::CloseRootScope {
            terminal: Box::new(terminal),
        })
    }
}
