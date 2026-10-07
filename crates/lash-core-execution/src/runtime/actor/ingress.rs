//! The session-ingress half of the context (ADR 0132 §3, §12; L3s,
//! FIG-5196): the effects a session's producers and its plugin host run
//! outside any turn.
//!
//! None of them is fenced by the session: a producer writes its row and
//! wakes the session actor in one transaction, and the session's activation
//! drains its mail under its own epoch
//! ([`drain_session_mail`](crate::runtime::durable::session_mail)). Nothing
//! is re-run against a recorded history, so each effect runs its body once,
//! here.

use super::ActorContext;

impl ActorContext {
    /// The ingress effects: `AcceptTurnInput` (the producer's write of its
    /// pending input and the session's wake), `TransitionPlugins` and
    /// `PluginCallbacks` (the plugin host's own work). Any other command is
    /// refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn ingress_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        match &envelope.command {
            crate::RuntimeEffectCommand::AcceptTurnInput { .. }
            | crate::RuntimeEffectCommand::TransitionPlugins { .. }
            | crate::RuntimeEffectCommand::PluginCallbacks { .. } => local.execute(envelope).await,
            other => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!("{} is not an ingress effect", other.kind().as_str()),
            )),
        }
    }
}
