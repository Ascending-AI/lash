//! The turn's half of the context (ADR 0132 §4). Owned by L3 (FIG-5172).
//!
//! The turn's phase runner lives in `lash-core`'s `runtime::durable::session`
//! (V0, FIG-5170). Nothing here is journaled: each effect runs in place, and
//! what it computes is durable only through the store write its own runner
//! makes or the phase transaction that commits it; a restore recomputes the
//! rest from committed state.

pub use lash_durable::domain::{ModelPin, TurnRow, UnfinishedPhase};

use super::ActorContext;

/// The turn's methods of the context.
impl ActorContext {
    /// The turn's model and preparation effects: `BeforeLlmCall`, `LlmCall`
    /// (a `Repeatable` model call pinned by `model.start`),
    /// `AssistantResponseHooks`, `Direct`, `SyncExecutionEnvironment`,
    /// `Checkpoint`, `ResolveTurnConfig`, `RecordCompactionBase`,
    /// `RenderCompactionPrompt` and `TraceBoundary`, each a
    /// write inside the turn's phase transactions or recomputed from committed
    /// state, so each runs in place. Any other command is refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn turn_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        match &envelope.command {
            crate::RuntimeEffectCommand::BeforeLlmCall { .. }
            | crate::RuntimeEffectCommand::LlmCall { .. }
            | crate::RuntimeEffectCommand::AssistantResponseHooks { .. }
            | crate::RuntimeEffectCommand::Direct { .. }
            | crate::RuntimeEffectCommand::SyncExecutionEnvironment
            | crate::RuntimeEffectCommand::Checkpoint { .. }
            | crate::RuntimeEffectCommand::ResolveTurnConfig { .. }
            | crate::RuntimeEffectCommand::RecordCompactionBase { .. }
            | crate::RuntimeEffectCommand::RenderCompactionPrompt { .. }
            | crate::RuntimeEffectCommand::TraceBoundary { .. } => {
                local.run_in_place(envelope).await
            }
            other => Err(not_in_group(other, "turn")),
        }
    }

    /// The session's own effects: `ResolveConfigTransaction`,
    /// `ReadSessionCommandRun` and `CloseRunScope`. Any other command is
    /// refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn session_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        match &envelope.command {
            crate::RuntimeEffectCommand::ResolveConfigTransaction { .. }
            | crate::RuntimeEffectCommand::ReadSessionCommandRun { .. }
            | crate::RuntimeEffectCommand::CloseRunScope { .. } => {
                local.run_in_place(envelope).await
            }
            other => Err(not_in_group(other, "session")),
        }
    }
}

/// The refusal of a command `group`'s method does not run.
fn not_in_group(
    command: &crate::RuntimeEffectCommand,
    group: &str,
) -> crate::RuntimeEffectControllerError {
    crate::RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
        format!("{:?} is not a {group} effect", command.kind()),
    )
}
