use std::sync::Arc;

use crate::plugin::{
    PluginError, SessionHandle, SessionLifecycleService, SessionSnapshot, SessionStateService,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSessionModel {
    pub model: String,
    pub model_variant: crate::ReasoningSelection,
    pub model_capability: crate::provider::ModelCapability,
    /// The session's generation options, so a tool making its own LLM call on
    /// the session's behalf runs under the same sampling intent as the turn
    /// that invoked it rather than at provider defaults.
    ///
    /// Already bounded by `model`'s output-token capacity, because the direct
    /// path a tool calls has no limits of its own to check a cap against. A
    /// tool that substitutes its own model owns that pairing: the cap it gets
    /// here is the one the *session's* model can produce.
    pub generation: crate::GenerationOptions,
}

#[derive(Clone)]
pub struct ToolSessionAdmin<'run> {
    pub(super) session_id: String,
    pub(super) sessions: Arc<dyn SessionStateService>,
    pub(super) session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub(super) effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
}

impl<'run> ToolSessionAdmin<'run> {
    pub async fn model(&self) -> Result<ToolSessionModel, PluginError> {
        let snapshot = self.snapshot_current().await?;
        let generation = snapshot
            .policy
            .model
            .clamped_generation(&snapshot.policy.generation);
        Ok(ToolSessionModel {
            model: snapshot.policy.model.id,
            model_variant: snapshot.policy.model.variant,
            model_capability: snapshot.policy.model.capability,
            generation,
        })
    }

    pub async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        self.snapshot(&self.session_id).await
    }

    pub async fn snapshot(
        &self,
        session_id: impl AsRef<str>,
    ) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(session_id.as_ref()).await
    }

    pub async fn create_session(
        &self,
        request: crate::SessionCreateRequest,
    ) -> Result<SessionHandle, PluginError> {
        self.session_lifecycle.create_session(request).await
    }

    pub async fn close_session(&self, session_id: &str) -> Result<(), PluginError> {
        self.session_lifecycle.close_session(session_id).await
    }

    /// Run one turn on a managed session, scoping its durable effects to
    /// `turn_id`.
    ///
    /// `turn_id` must be unique across every managed turn running in this
    /// process (a process id or another already-unique handle); a duplicate is
    /// rejected with `` turn `<id>` is already running on session `<other>` ``.
    /// A session runs at most one turn at a time, and both registrations are
    /// released even when this future is dropped mid-turn.
    pub async fn start_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        input: crate::TurnInput,
    ) -> Result<crate::AssembledTurn, PluginError> {
        let scope = self.sessions.turn_scope(session_id, turn_id).await?;
        let scoped_effect_controller = self
            .effect_controller
            .scoped_for(scope)
            .map_err(|err| PluginError::Session(err.to_string()))?;
        let request =
            crate::SessionTurnRequest::new(session_id, turn_id, input, scoped_effect_controller)?;
        self.session_lifecycle.start_turn(request).await
    }

    pub async fn tool_catalog(&self) -> Result<Vec<serde_json::Value>, PluginError> {
        self.sessions.tool_catalog(&self.session_id).await
    }

    pub async fn shared_tool_catalog(&self) -> Result<Arc<Vec<serde_json::Value>>, PluginError> {
        self.sessions.shared_tool_catalog(&self.session_id).await
    }

    pub async fn set_tool_membership(
        &self,
        names: &[String],
        present: bool,
    ) -> Result<u64, PluginError> {
        self.sessions
            .set_tool_membership(&self.session_id, names, present)
            .await
    }
}
