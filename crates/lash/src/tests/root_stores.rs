//! The logical-root segment of the facade tests' store doubles (FIG-3600 S7).

use super::*;

/// SnapshotStore keeps a root ledger in memory and writes a root's terminal
/// evidence from its commit, exactly as a SQL backend does in the commit's
/// transaction.
#[async_trait]
impl lash_core::store::RootStore for SnapshotStore {
    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        Ok(self.roots.terminal(session_id, root))
    }

    async fn root_of_input(
        &self,
        session_id: &SessionId,
        input: &lash_core::InputId,
    ) -> std::result::Result<Option<lash_core::TurnId>, lash_core::StoreError> {
        Ok(self.roots.binding(session_id, input))
    }

    async fn root_binding(
        &self,
        session_id: &SessionId,
        input: &lash_core::InputId,
    ) -> std::result::Result<Option<lash_core::TurnId>, lash_core::StoreError> {
        Ok(self.roots.binding(session_id, input))
    }

    async fn bind_root_inputs(
        &self,
        session_id: &SessionId,
        root: &lash_core::TurnId,
        inputs: &[lash_core::InputId],
    ) -> std::result::Result<(), lash_core::StoreError> {
        self.roots.bind(session_id, root, inputs)
    }
}

// The reuse test fails before any turn runs, so this double holds no root.
#[async_trait]
impl lash_core::store::RootStore for BoundSessionStore {
    async fn root_terminal(
        &self,
        _session_id: &SessionId,
        _root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        Ok(None)
    }

    async fn root_of_input(
        &self,
        _session_id: &SessionId,
        _input: &lash_core::InputId,
    ) -> std::result::Result<Option<lash_core::TurnId>, lash_core::StoreError> {
        Ok(None)
    }

    async fn root_binding(
        &self,
        _session_id: &SessionId,
        _input: &lash_core::InputId,
    ) -> std::result::Result<Option<lash_core::TurnId>, lash_core::StoreError> {
        Ok(None)
    }

    async fn bind_root_inputs(
        &self,
        _session_id: &SessionId,
        _root: &lash_core::TurnId,
        _inputs: &[lash_core::InputId],
    ) -> std::result::Result<(), lash_core::StoreError> {
        unreachable!("test should fail before a root claims input on the reused child store")
    }
}
