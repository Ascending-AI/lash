use super::*;

impl CurrentSessionCapability {
    pub(in crate::runtime::session_manager) async fn turn_scope_by_id(
        &self,
        managed: &ManagedSessionCapability,
        session_id: &str,
        turn_id: &str,
    ) -> Result<crate::ExecutionScope, crate::PluginError> {
        if session_id == self.session_id {
            return Ok(self.snapshot.to_runtime_state().turn_scope(turn_id));
        }
        let runtime = {
            let registry = managed.registry.lock().await;
            registry.get(session_id).cloned()
        }
        .ok_or_else(|| crate::PluginError::Session(format!("unknown session `{session_id}`")))?;
        Ok(runtime.observe().persisted_state.turn_scope(turn_id))
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
        managed: &ManagedSessionCapability,
        session_id: &str,
    ) -> Result<SessionSnapshot, crate::PluginError> {
        if session_id == self.session_id {
            let state = self.snapshot.to_runtime_state();
            return Ok(state.to_snapshot());
        }
        let runtime = {
            let registry = managed.registry.lock().await;
            registry.get(session_id).cloned()
        }
        .ok_or_else(|| crate::PluginError::Session(format!("unknown session `{session_id}`")))?;
        Ok(runtime.observe().persisted_state.to_snapshot())
    }

    pub(in crate::runtime::session_manager) async fn tool_catalog_by_id(
        &self,
        managed: &ManagedSessionCapability,
        session_id: &str,
    ) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        Ok(self
            .shared_tool_catalog_by_id(managed, session_id)
            .await?
            .as_ref()
            .clone())
    }

    pub(in crate::runtime::session_manager) async fn shared_tool_catalog_by_id(
        &self,
        managed: &ManagedSessionCapability,
        session_id: &str,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        if session_id == self.session_id {
            if let Some(runtime) = managed.registry.lock().await.get(session_id).cloned() {
                let runtime = runtime.runtime.lock().await;
                return runtime.active_tool_catalog_shared();
            }
            return Ok(Arc::new(self.plugins.tool_catalog(session_id)?));
        }
        let runtime = {
            let registry = managed.registry.lock().await;
            registry.get(session_id).cloned()
        }
        .ok_or_else(|| crate::PluginError::Session(format!("unknown session `{session_id}`")))?;
        let observation = runtime.observe();
        if let Some(err) = observation.tool_catalog_error.as_ref() {
            return Err(crate::PluginError::Session(err.clone()));
        }
        Ok(Arc::clone(&observation.tool_catalog))
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
        managed: &ManagedSessionCapability,
        session_id: &str,
    ) -> Result<SessionSnapshot, crate::PluginError> {
        self.snapshot_by_id(managed, session_id).await
    }

    pub(in crate::runtime::session_manager) async fn tool_catalog(
        &self,
        managed: &ManagedSessionCapability,
        session_id: &str,
    ) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        self.tool_catalog_by_id(managed, session_id).await
    }

    pub(in crate::runtime::session_manager) async fn shared_tool_catalog(
        &self,
        managed: &ManagedSessionCapability,
        session_id: &str,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        self.shared_tool_catalog_by_id(managed, session_id).await
    }

    pub(in crate::runtime::session_manager) async fn tool_state(
        &self,
        managed: &ManagedSessionCapability,
        session_id: &str,
    ) -> Result<crate::ToolState, crate::PluginError> {
        if session_id == self.session_id {
            if let Some(runtime) = managed.registry.lock().await.get(session_id).cloned() {
                return runtime.observe().tool_state.clone().ok_or_else(|| {
                    crate::PluginError::Session("runtime session not available".to_string())
                });
            }
            return Ok(self.current_tool_registry()?.export_state());
        }

        let runtime = {
            let registry = managed.registry.lock().await;
            registry.get(session_id).cloned()
        }
        .ok_or_else(|| crate::PluginError::Session(format!("unknown session `{session_id}`")))?;
        runtime
            .observe()
            .tool_state
            .clone()
            .ok_or_else(|| crate::PluginError::Session("runtime session not available".to_string()))
    }

    pub(in crate::runtime::session_manager) async fn apply_tool_state(
        &self,
        managed: &ManagedSessionCapability,
        session_id: &str,
        snapshot: crate::ToolState,
    ) -> Result<u64, crate::PluginError> {
        if session_id == self.session_id {
            if let Some(runtime) = managed.registry.lock().await.get(session_id).cloned() {
                let mut writer = runtime.runtime.lock().await;
                let generation = writer
                    .apply_tool_state(snapshot)
                    .await
                    .map_err(|err| crate::PluginError::Session(err.to_string()))?;
                runtime.publish_from(&writer);
                return Ok(generation);
            }
            let tool_registry = self.current_tool_registry()?;
            return tool_registry
                .apply_state(snapshot)
                .map_err(|err| crate::PluginError::Session(err.to_string()));
        }

        let runtime = {
            let registry = managed.registry.lock().await;
            registry.get(session_id).cloned()
        }
        .ok_or_else(|| crate::PluginError::Session(format!("unknown session `{session_id}`")))?;
        let mut writer = runtime.runtime.lock().await;
        let generation = writer
            .apply_tool_state(snapshot)
            .await
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        runtime.publish_from(&writer);
        Ok(generation)
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
