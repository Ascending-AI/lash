//! `LocalTestCx` as a [`RuntimeEffectController`]: today's shift code, which
//! issues its effects through a scoped controller, runs under the harness
//! unchanged.
//!
//! Every `execute_effect` is one recorded operation keyed by the envelope's
//! replay key, whose command bytes are the envelope's canonical form — the
//! bytes an engine's replay fence compares — and whose body is the local
//! executor the shift handed over.

use super::cx::LocalTestCx;
use crate::{
    AdmittedScope, AwaitEventResolver, RecordedJournal, RecordedKeyRange, RecordedKeys,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, ScopedEffectController,
};

impl LocalTestCx {
    /// A scoped controller over this context for `admitted`: hand it to work
    /// code that issues effects through a controller.
    pub fn controller(
        &self,
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'_>, RuntimeError> {
        ScopedEffectController::borrowed(self, admitted)
    }
}

impl AwaitEventResolver for LocalTestCx {
    /// A shift-test context mints no durable await-event keys.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for LocalTestCx {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let canonical = envelope.canonical_form()?;
        let key = envelope.invocation.effect_replay_key().to_string();
        let kind = envelope.command.kind().as_str().to_string();
        self.op_with_command_bytes(
            key,
            kind,
            canonical.json().to_string(),
            local_executor.execute(envelope),
        )
        .await
    }

    async fn read_recorded_journal(
        &self,
        range: &RecordedKeyRange,
    ) -> Result<RecordedJournal, RuntimeEffectControllerError> {
        let replay_keys = self.recorded_keys_in(&range.lower, &range.upper);
        Ok(RecordedJournal::Keys(RecordedKeys {
            replay_keys,
            group_keys: Vec::new(),
            closing_outcome: None,
        }))
    }
}
