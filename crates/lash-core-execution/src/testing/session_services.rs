use super::*;

#[async_trait::async_trait]
impl crate::plugin::SessionReadService for MockSessionManager {
    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.clone())
    }
    async fn snapshot_session(&self, _: &SessionId) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.clone())
    }
    async fn tool_catalog(&self, _: &SessionId) -> Result<Vec<serde_json::Value>, PluginError> {
        Ok(self.tool_catalog.clone())
    }
    async fn tool_state(&self, session_id: &SessionId) -> Result<crate::ToolState, PluginError> {
        crate::plugin::SessionStateService::tool_state(self, session_id).await
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

    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.clone())
    }

    async fn snapshot_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<SessionSnapshot, PluginError> {
        Ok(self.snapshot.clone())
    }
    async fn tool_catalog(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, PluginError> {
        Ok(self.tool_catalog.clone())
    }
    async fn tool_state(&self, _session_id: &SessionId) -> Result<crate::ToolState, PluginError> {
        self.tool_registry
            .as_ref()
            .map(crate::ToolRegistry::export_state)
            .ok_or_else(|| {
                PluginError::Session("tool state is unavailable in this session".to_string())
            })
    }

    async fn apply_tool_state(
        &self,
        _session_id: &SessionId,
        snapshot: crate::ToolState,
    ) -> Result<u64, PluginError> {
        let Some(tool_registry) = self.tool_registry.as_ref() else {
            return Err(PluginError::Session(
                "tool state mutation is unavailable in this session".to_string(),
            ));
        };
        tool_registry
            .apply_state(snapshot)
            .map_err(|err| PluginError::Session(err.to_string()))
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
impl crate::plugin::SessionGraphService for MockSessionManager {}
