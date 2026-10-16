use super::*;

impl crate::core_internal::RuntimeService for MockSessionManager {}

#[async_trait::async_trait]
impl crate::plugin::SessionReadService for MockSessionManager {
    async fn committed_turn(
        &self,
        _session_id: &crate::SessionId,
        _turn_id: &crate::TurnId,
    ) -> Result<
        Option<lash_core_store::transcript::CommittedTurnWithToolCalls>,
        crate::plugin::CommittedTurnReadError,
    > {
        Err(crate::PluginError::Session("operation is absent from this fixture".into()).into())
    }

    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.clone())
    }
    async fn snapshot_session(&self, _: &SessionId) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.clone())
    }
    async fn tool_catalog(&self, _: &SessionId) -> Result<Vec<serde_json::Value>, PluginError> {
        Ok(self.tool_catalog.clone())
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionStateService for MockSessionManager {
    async fn turn_scope(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, PluginError> {
        Ok(crate::ExecutionScope::turn(session_id, turn_id))
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionLifecycleService for MockSessionManager {
    async fn create_session(
        &self,
        request: SessionCreateRequest,
    ) -> Result<SessionHandle, PluginError> {
        self.created.lock_recover().push(request.clone());
        Ok(SessionHandle {
            session_id: request
                .session_id
                .clone()
                .unwrap_or_else(|| SessionId::from("child")),
            parent_session_id: request.relation.parent_session_id().cloned(),
            policy: request.policy.unwrap_or_else(mock_session_policy),
            observed_processes: Vec::new(),
        })
    }
}

#[async_trait::async_trait]
impl crate::plugin::SessionGraphService for MockSessionManager {
    async fn append_session_nodes(
        &self,
        _session_id: &crate::SessionId,
        _request: crate::AppendSessionNodesRequest,
    ) -> Result<crate::AppendSessionNodesOutcome, crate::PluginError> {
        Err(crate::PluginError::Session(
            "operation is absent from this fixture".into(),
        ))
    }
}
