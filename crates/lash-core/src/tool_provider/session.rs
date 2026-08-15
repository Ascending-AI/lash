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
    pub(super) parent_invocation: Option<crate::RuntimeInvocation>,
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
        if self.parent_invocation.as_ref().is_some_and(|invocation| {
            invocation.effect_kind() == Some(crate::RuntimeEffectKind::ToolAttempt)
        }) && self.effect_controller.controller().journal_addressing()
            == crate::EffectJournalAddressing::OrdinalAddressed
        {
            return Err(PluginError::Session(
                "ToolContext::sessions().start_turn() is unavailable inside an atomic tool attempt on ordinal-addressed journal tiers; return a typed tool intent for supported follow-on work or start the nested turn from a process step"
                    .to_string(),
            ));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    struct ControllerOwnedReplay;

    impl crate::AwaitEventResolver for ControllerOwnedReplay {
        fn replay_ownership(&self) -> crate::EffectReplayOwnership {
            crate::EffectReplayOwnership::Controller
        }

        fn journal_addressing(&self) -> crate::EffectJournalAddressing {
            crate::EffectJournalAddressing::OrdinalAddressed
        }
    }

    #[async_trait::async_trait]
    impl crate::RuntimeEffectController for ControllerOwnedReplay {
        async fn execute_effect(
            &self,
            _envelope: crate::RuntimeEffectEnvelope,
            _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
        ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
            panic!("nested turn guard must reject before effect execution")
        }
    }

    #[tokio::test]
    async fn start_turn_is_rejected_inside_controller_owned_atomic_tool_attempt() {
        let manager = Arc::new(crate::testing::MockSessionManager::default());
        let sessions: Arc<dyn SessionStateService> = manager.clone();
        let lifecycle: Arc<dyn SessionLifecycleService> = manager;
        let admin = ToolSessionAdmin {
            session_id: "session".to_string(),
            sessions,
            session_lifecycle: lifecycle,
            effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
                ControllerOwnedReplay,
            )),
            parent_invocation: Some(crate::RuntimeInvocation::effect(
                crate::RuntimeScope::new("session"),
                "parent-tool-attempt",
                crate::RuntimeEffectKind::ToolAttempt,
                "parent-tool-attempt",
            )),
        };

        let error = admin
            .start_turn(
                "child-session",
                "child-turn",
                crate::TurnInput::text("nested"),
            )
            .await
            .expect_err("nested turn must be rejected");

        assert_eq!(
            error.to_string(),
            "plugin session error: ToolContext::sessions().start_turn() is unavailable inside an atomic tool attempt on ordinal-addressed journal tiers; return a typed tool intent for supported follow-on work or start the nested turn from a process step"
        );
    }
}
