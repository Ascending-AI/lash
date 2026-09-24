//! `LocalTestCx` as a [`RuntimeEffectController`]: today's drive code, which
//! issues its effects through a scoped controller, runs under the harness
//! unchanged.
//!
//! Every `execute_effect` is one recorded operation keyed by the envelope's
//! replay key, whose command bytes are the envelope's canonical form — the
//! bytes an engine's replay fence compares — and whose body is the local
//! executor the drive handed over.

use tokio_util::sync::CancellationToken;

use super::cx::LocalTestCx;
use crate::{
    AdmittedScope, AwaitEventResolver, EffectGroupHandle, EffectJournaling, GroupSettlement,
    LoserPolicy, RecordedJournal, RecordedKeyRange, RecordedKeys, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectGroup,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, ScopedEffectController,
    effect_groups_unsupported,
};

const HARNESS: &str = "the local determinism test context";

impl LocalTestCx {
    /// A scoped controller over this context for `admitted`: hand it to drive
    /// code that issues effects through a controller.
    pub fn controller(
        &self,
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'_>, RuntimeError> {
        ScopedEffectController::borrowed(self, admitted)
    }
}

impl AwaitEventResolver for LocalTestCx {}

#[async_trait::async_trait]
impl RuntimeEffectController for LocalTestCx {
    fn effect_journaling(&self) -> EffectJournaling {
        EffectJournaling::Journaled
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let canonical = envelope.canonical_form()?;
        let key = envelope.invocation.replay_key().to_string();
        let kind = envelope.command.kind().as_str().to_string();
        self.op_with_command_bytes(
            key,
            kind,
            canonical.json().to_string(),
            local_executor.execute(envelope),
        )
        .await
    }

    async fn open_effect_group(
        &self,
        _group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        Err(effect_groups_unsupported(HARNESS))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut EffectGroupHandle,
        _cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        Err(effect_groups_unsupported(HARNESS))
    }

    async fn close_effect_group(
        &self,
        _handle: EffectGroupHandle,
        _disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        Err(effect_groups_unsupported(HARNESS))
    }

    async fn read_recorded_journal(
        &self,
        range: &RecordedKeyRange,
    ) -> Result<RecordedJournal, RuntimeEffectControllerError> {
        let (replay_keys, settled_keys) = self.recorded_keys_in(&range.lower, &range.upper);
        Ok(RecordedJournal::Keys(RecordedKeys {
            replay_keys,
            settled_keys,
            group_keys: Vec::new(),
            closing_outcome: None,
        }))
    }
}
