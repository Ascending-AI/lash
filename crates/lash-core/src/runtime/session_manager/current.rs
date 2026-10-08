use super::*;
use crate::TurnId;
use crate::facade_support::RuntimeSessionStateFacadeOps;

impl CurrentOwnerCapability {
    /// Resolve the resident durable-state projection for `session_id`. Only
    /// the current session is resident: there is no runtime registry for
    /// other sessions, and a process runtime has no resident session at all.
    pub(in crate::runtime::session_manager) async fn resident_state_by_id(
        &self,
        session_id: &SessionId,
    ) -> Option<RuntimeSessionState> {
        self.session()
            .filter(|session| session.session_id == *session_id)
            .map(|session| session.snapshot.to_runtime_state())
    }

    /// The current session named `session_id`, or an unknown-session error.
    fn known_session(&self, session_id: &SessionId) -> Result<&CurrentSession, crate::PluginError> {
        self.session()
            .filter(|session| session.session_id == *session_id)
            .ok_or_else(|| crate::PluginError::Session(format!("unknown session `{session_id}`")))
    }

    pub(in crate::runtime::session_manager) async fn turn_scope_by_id(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, crate::PluginError> {
        Ok(self
            .known_session(session_id)?
            .snapshot
            .to_runtime_state()
            .turn_scope(turn_id))
    }

    pub(in crate::runtime) async fn current_snapshot_for_store_write(
        &self,
    ) -> Result<RuntimeSessionState, crate::PluginError> {
        let session = self.require_session("session_store_write")?;
        let mut state = session.snapshot.to_runtime_state();
        if let Some(store) = &session.store {
            crate::store::refresh_session_window(store, &mut state)
                .await
                .map_err(|err| {
                    crate::PluginError::Session(format!(
                        "failed to refresh persisted session state: {err}"
                    ))
                })?;
        }
        Ok(state)
    }

    /// A named session's snapshot: the resident session's own state, or any
    /// other session's durable head read by name. A process runtime has no
    /// resident session, so every snapshot it reads names one explicitly
    /// (a child spawned inside a process reads its originator's).
    pub(in crate::runtime::session_manager) async fn snapshot_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionSnapshot, crate::PluginError> {
        if let Some(state) = self.resident_state_by_id(session_id).await {
            return Ok(state.to_snapshot());
        }
        let unknown = || crate::PluginError::Session(format!("unknown session `{session_id}`"));
        let read_failed = |error: crate::StoreError| {
            crate::PluginError::Session(format!("failed to read session `{session_id}`: {error}"))
        };
        let store =
            crate::runtime::live_session_view(&self.host.core.session_store_factory(), session_id)
                .await
                .map_err(read_failed)?
                .ok_or_else(unknown)?;
        let loaded =
            crate::store::load_session_window_state(&store, crate::store::WindowSelector::Current)
                .await
                .map_err(read_failed)?
                .ok_or_else(unknown)?;
        Ok(loaded.state.to_snapshot())
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
        self.known_session(session_id)?;
        Ok(Arc::new(self.plugins.tool_catalog()?))
    }

    pub(in crate::runtime::session_manager) fn current_tool_registry(
        &self,
    ) -> Result<Arc<crate::ToolRegistry>, crate::PluginError> {
        Ok(self.plugins.tool_registry())
    }

    pub(in crate::runtime::session_manager) async fn snapshot_current(
        &self,
    ) -> Result<SessionSnapshot, crate::PluginError> {
        let session = self.require_session("snapshot_current")?;
        Ok(session.snapshot.to_runtime_state().to_snapshot())
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
        self.known_session(session_id)?;
        Ok(self.current_tool_registry()?.export_state())
    }

    pub(in crate::runtime::session_manager) async fn apply_tool_state(
        &self,
        session_id: &SessionId,
        snapshot: crate::ToolState,
    ) -> Result<u64, crate::PluginError> {
        self.known_session(session_id)?;
        let tool_registry = self.current_tool_registry()?;
        tool_registry
            .apply_state(snapshot)
            .map_err(|err| crate::PluginError::Session(err.to_string()))
    }

    pub(in crate::runtime::session_manager) async fn emit_trace_event(
        &self,
        context: lash_trace::TraceContext,
        event: lash_trace::TraceEvent,
    ) -> Result<(), crate::PluginError> {
        self.emit_trace(context, event);
        Ok(())
    }

    pub(in crate::runtime::session_manager) fn emit_trace(
        &self,
        context: lash_trace::TraceContext,
        event: lash_trace::TraceEvent,
    ) {
        let context = match self.session() {
            Some(session) => context.for_session(session.session_id.clone()),
            None => context,
        };
        // A plugin's own trace event. The plugin contract does not yet hand
        // a plugin the permit of the body it runs in, so the event is taken
        // as the plugin's live work wherever it was made.
        self.host
            .core
            .tracing
            .unreplayed(None)
            .observe(|| (context, event));
    }
}
