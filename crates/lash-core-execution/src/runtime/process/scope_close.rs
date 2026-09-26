//! The process registry's scope-close adapter (FIG-3607 item 7, R9, R10): the
//! owner of lifetime scopes the drive reports a closed root or session to.
//!
//! Closing a scope writes its row in the registry's scope-close ledger. The
//! row is what requests cancellation of every process living `Until` the
//! scope, and what refuses a start that names the scope as its starter or its
//! lifetime afterwards (R11). Writing a row that already exists keeps the
//! first, so the drive's at-least-once call is idempotent per root and per
//! session.

use std::sync::Arc;

use crate::engine::ScopeCloseSink;
use crate::store::{ControlIntentId, RootTerminal, StoreError};
use crate::{ProcessRegistry, ScopeId, SessionId, TurnId};

/// Closes lifetime scopes in a process registry's scope-close ledger.
#[derive(Clone)]
pub struct RegistryScopeClose {
    registry: Arc<dyn ProcessRegistry>,
}

impl RegistryScopeClose {
    /// The adapter over `registry`.
    #[must_use]
    pub fn new(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self { registry }
    }

    async fn close(&self, scope: &ScopeId) -> Result<(), StoreError> {
        self.registry
            .record_parent_end(scope)
            .await
            .map_err(|error| StoreError::Backend(format!("close scope `{scope}`: {error}")))
    }
}

#[async_trait::async_trait]
impl ScopeCloseSink for RegistryScopeClose {
    async fn close_root_scope(&self, terminal: &RootTerminal) -> Result<(), StoreError> {
        self.close(&ScopeId::turn(
            terminal.session_id.clone(),
            terminal.root.clone(),
        ))
        .await
    }

    /// The session's roots close before the session itself: a start that
    /// names a root is refused from the moment its root closes, and the
    /// session's own row is the last fact the close writes.
    async fn close_session_scope(
        &self,
        session: &SessionId,
        _intent: ControlIntentId,
        roots: &[TurnId],
    ) -> Result<(), StoreError> {
        for root in roots {
            self.close(&ScopeId::turn(session.clone(), root.clone()))
                .await?;
        }
        self.close(&ScopeId::session(session.clone())).await
    }
}
