use super::*;
use crate::TurnId;
use crate::facade_support::RuntimeSessionStateFacadeOps;

impl CurrentSessionCapability {
    /// Resolve the durable-state projection for `session_id`. Only the current
    /// session resolves: there is no runtime registry for other sessions, so a
    /// foreign id is simply unknown to these services.
    pub(in crate::runtime::session_manager) async fn resident_state_by_id(
        &self,
        session_id: &SessionId,
    ) -> Option<RuntimeSessionState> {
        (session_id == self.session_id).then(|| self.snapshot.to_runtime_state())
    }

    pub(in crate::runtime::session_manager) async fn turn_scope_by_id(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, crate::PluginError> {
        if session_id != self.session_id {
            return Err(crate::PluginError::Session(format!(
                "unknown session `{session_id}`"
            )));
        }
        Ok(self.snapshot.to_runtime_state().turn_scope(turn_id))
    }

    pub(in crate::runtime) async fn current_snapshot_for_store_write(
        &self,
    ) -> Result<RuntimeSessionState, crate::PluginError> {
        let mut state = self.snapshot.to_runtime_state();
        if let Some(store) = &self.store {
            crate::store::refresh_persisted_session_state(store.as_ref(), &mut state)
                .await
                .map_err(|err| {
                    crate::PluginError::Session(format!(
                        "failed to refresh persisted session state: {err}"
                    ))
                })?;
        }
        Ok(state)
    }

    pub(in crate::runtime::session_manager) async fn snapshot_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionSnapshot, crate::PluginError> {
        self.resident_state_by_id(session_id)
            .await
            .map(|state| state.to_snapshot())
            .ok_or_else(|| crate::PluginError::Session(format!("unknown session `{session_id}`")))
    }

    pub(in crate::runtime::session_manager) async fn tool_catalog_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        Ok(self
            .shared_tool_catalog_by_id(session_id)
            .await?
            .as_ref()
            .clone())
    }

    pub(in crate::runtime::session_manager) async fn shared_tool_catalog_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        if session_id != self.session_id {
            return Err(crate::PluginError::Session(format!(
                "unknown session `{session_id}`"
            )));
        }
        Ok(Arc::new(self.plugins.tool_catalog(session_id)?))
    }

    pub(in crate::runtime::session_manager) fn current_tool_registry(
        &self,
    ) -> Result<Arc<crate::ToolRegistry>, crate::PluginError> {
        Ok(self.plugins.tool_registry())
    }

    pub(in crate::runtime::session_manager) async fn snapshot_current(
        &self,
    ) -> Result<SessionSnapshot, crate::PluginError> {
        let state = self.snapshot.to_runtime_state();
        Ok(state.to_snapshot())
    }

    pub(in crate::runtime::session_manager) async fn snapshot_session(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionSnapshot, crate::PluginError> {
        self.snapshot_by_id(session_id).await
    }

    pub(in crate::runtime::session_manager) async fn tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        self.tool_catalog_by_id(session_id).await
    }

    pub(in crate::runtime::session_manager) async fn shared_tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        self.shared_tool_catalog_by_id(session_id).await
    }

    pub(in crate::runtime::session_manager) async fn tool_state(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::ToolState, crate::PluginError> {
        if session_id != self.session_id {
            return Err(crate::PluginError::Session(format!(
                "unknown session `{session_id}`"
            )));
        }
        Ok(self.current_tool_registry()?.export_state())
    }

    pub(in crate::runtime::session_manager) async fn apply_tool_state(
        &self,
        session_id: &SessionId,
        snapshot: crate::ToolState,
    ) -> Result<u64, crate::PluginError> {
        if session_id != self.session_id {
            return Err(crate::PluginError::Session(format!(
                "unknown session `{session_id}`"
            )));
        }
        let tool_registry = self.current_tool_registry()?;
        tool_registry
            .apply_state(snapshot)
            .map_err(|err| crate::PluginError::Session(err.to_string()))
    }

    /// Capture the spawn-time [`crate::SessionPluginInit`] for a peer fork of
    /// the current session. This is the only read of the source session a
    /// fork performs — the payload it returns is what the journaled creation
    /// request carries, so materialization never touches the live session
    /// again.
    pub(in crate::runtime::session_manager) async fn plugin_init_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::SessionPluginInit, crate::PluginError> {
        if session_id != self.session_id {
            return Err(crate::PluginError::Session(format!(
                "unknown session `{session_id}`"
            )));
        }
        self.plugins.capture_fork_init()
    }

    pub(in crate::runtime::session_manager) async fn emit_trace_event(
        &self,
        context: lash_trace::TraceContext,
        event: lash_trace::TraceEvent,
    ) -> Result<(), crate::PluginError> {
        crate::trace::emit_trace(
            &self.host.core.tracing.trace_sink,
            &self.host.core.tracing.trace_context,
            context.for_session(self.session_id.clone()),
            event,
            self.host.core.clock.as_ref(),
        );
        Ok(())
    }
}
