//! The turn's half of the context (ADR 0132 §4). Owned by L3 (FIG-5172).
//!
//! The turn's phase runner lives in `lash-core`'s `runtime::durable::session`
//! (V0, FIG-5170); the context methods the journal-era turn driver still
//! calls live here until L3 folds that driver into the phase runner.

pub use lash_durable::domain::{ModelPin, TurnPhase, TurnRow, TurnTerminal};

use super::ActorContext;

/// The turn's methods of the context.
impl ActorContext {
    /// The turn's model and preparation effects: `BeforeLlmCall`, `LlmCall`
    /// (a `Repeatable` model call pinned by `model.start`),
    /// `AssistantResponseHooks`, `Direct`, `SyncExecutionEnvironment`,
    /// `Checkpoint`, `ResolveTurnConfig`, `RecordCompactionBase`,
    /// `RenderCompactionPrompt`, `RecoverFollowOn` and `TraceBoundary`, each a
    /// write inside the turn's phase transactions or recomputed from committed
    /// state. Any other command is refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn turn_effect(
        &self,
        _envelope: crate::RuntimeEffectEnvelope,
        _local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        todo!("L3 (FIG-5172): run a turn effect inside its phase transaction")
    }

    /// The session's own effects: `ResolveConfigTransaction`,
    /// `ReadSessionCommandRun`, `CloseRunScope` and `BeginSessionClose`. Any
    /// other command is refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn session_effect(
        &self,
        _envelope: crate::RuntimeEffectEnvelope,
        _local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        todo!("L3 (FIG-5172): run a session effect as a write under the session's epoch")
    }

    /// The identity of the authority that owns this scope's turn control:
    /// its cancel is session mail now.
    #[must_use]
    pub fn turn_control_binding_id(&self) -> String {
        todo!("L3 (FIG-5172): replace the turn-control binding by turn cancel mail")
    }

    /// Bind turn control for this scope.
    ///
    /// # Errors
    ///
    /// The binding's refusal.
    pub async fn turn_control_binding(
        &self,
    ) -> Result<crate::TurnControlBinding<'_>, crate::RuntimeError> {
        todo!("L3 (FIG-5172): replace the turn-control binding by turn cancel mail")
    }

    /// Register a session catalog as a turn-cancel closure participant of
    /// `scope`.
    ///
    /// # Errors
    ///
    /// The participant's refusal.
    pub async fn register_turn_cancel_closure_participant(
        &self,
        _participant_id: &str,
        _scope: &crate::ExecutionScope,
    ) -> Result<(), crate::RuntimeError> {
        todo!("L3 (FIG-5172): delete with the turn-control binding tables once cancel is mail")
    }

    /// Release a session catalog's turn-cancel closure participant of
    /// `scope`.
    ///
    /// # Errors
    ///
    /// The participant's refusal.
    pub async fn release_turn_cancel_closure_participant(
        &self,
        _participant_id: &str,
        _scope: &crate::ExecutionScope,
    ) -> Result<(), crate::RuntimeError> {
        todo!("L3 (FIG-5172): delete with the turn-control binding tables once cancel is mail")
    }
}
