use super::*;
use lash_core::facade_support::RuntimeSessionStateFacadeOps;
use lash_core::testing::TestTurnExecution as _;

const SEED: u64 = 0x5_e217;

// Exercise the trait-default turn-control binding over a real journaling host:
// this host forwards its ports to `0` but keeps the trait's own
// `turn_control_binding`, so the binding comes from the default's journaled
// arm rather than from the backend host's override.
struct DefaultBindingHost(Arc<dyn lash_core::EffectHost>);

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for DefaultBindingHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.0.await_event_authority_binding_id()
    }

    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.0
            .await_event_resolver()
            .await_event_key(scope, wait)
            .await
    }
    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.0
            .await_event_resolver()
            .resolve_await_event(key, resolution)
            .await
    }
    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.0.await_event_resolver().peek_await_event(key).await
    }
    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: CancellationToken,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.0
            .await_event_resolver()
            .await_await_event(key, cancel)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::EffectHost for DefaultBindingHost {
    async fn journal_replay(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> Result<lash_core::JournalReplay, lash_core::RuntimeError> {
        self.0.journal_replay(journal).await
    }

    fn turn_control_binding_id(&self) -> String {
        self.0.turn_control_binding_id()
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    fn scoped<'run>(
        &'run self,
        scope: lash_core::AdmittedScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.0.scoped(scope)
    }

    fn scoped_static(
        &self,
        scope: lash_core::AdmittedScope,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        self.0.scoped_static(scope)
    }
}
