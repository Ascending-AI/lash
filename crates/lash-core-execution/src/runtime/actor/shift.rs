//! The session-ingress half of the context: what the shift journaled
//! (ADR 0132 §3, §12). Owned by L3s (FIG-5196).

use super::ActorContext;

impl ActorContext {
    /// The shift's and ingress's effects: `TransitionPlugins`, `AdmitShift`,
    /// `DrawRunStart`, `AcceptTurnInput`, `ObserveDrainMark` and
    /// `PluginCallbacks`. Admission is the actor claim: each becomes a write
    /// under the session's epoch in `drain_session_mail`, or is deleted with
    /// the shift fence (`DrawRunStart`, `ObserveDrainMark`). Any other
    /// command is refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn shift_effect(
        &self,
        _envelope: crate::RuntimeEffectEnvelope,
        _local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        todo!(
            "L3s (FIG-5196): apply a shift effect under the session's epoch, or delete it with the shift fence"
        )
    }
}
