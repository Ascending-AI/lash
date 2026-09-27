//! `LayeredEffectHost` over a SQLite memory backend (FIG-3580): a layer
//! observes the seam, and the backend's journal arbitrates every group.

use std::sync::{Arc, Mutex};

use lash_core_execution::testing::{EffectLayer, LayeredEffectHost};
use lash_core_execution::{
    AdmittedScope, EffectAddress, EffectGroupHandle, EffectHost, ExecutionScope, GroupExecutors,
    GroupSettlement, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectGroup,
    RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use lash_sansio::sync::MutexExt;
use lash_sqlite_store::SqliteBackend;

/// Records the name of every seam operation it forwards, and nothing else.
#[derive(Default)]
struct RecordingLayer {
    seen: Mutex<Vec<String>>,
}

impl RecordingLayer {
    fn seen(&self) -> Vec<String> {
        self.seen.lock_recover().clone()
    }

    fn record(&self, operation: impl Into<String>) {
        self.seen.lock_recover().push(operation.into());
    }
}

#[async_trait::async_trait]
impl EffectLayer for RecordingLayer {
    async fn execute_effect(
        &self,
        inner: &dyn RuntimeEffectController,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.record(format!("execute:{}", envelope.invocation.replay_key()));
        inner.execute_effect(envelope, local_executor).await
    }

    async fn open_effect_group(
        &self,
        inner: &dyn RuntimeEffectController,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        self.record(format!("open:{}", group.group_key()));
        inner.open_effect_group(group).await
    }

    async fn await_next_settlement(
        &self,
        inner: &dyn RuntimeEffectController,
        handle: &mut EffectGroupHandle,
        cancel: lash_core::TurnCancelWait,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        let settlement = inner.await_next_settlement(handle, cancel).await?;
        self.record(format!("settle:{}", settlement.position));
        Ok(settlement)
    }
}

/// An effect run through a layered controller is journaled by the backend:
/// the layer records it, and a raw host over the same backend replays the
/// recorded outcome instead of running its own executor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layered_effect_is_journaled_by_the_backend() {
    let backend = SqliteBackend::memory()
        .await
        .expect("open a memory backend");
    let layer = Arc::new(RecordingLayer::default());
    let layered = LayeredEffectHost::new(
        backend.effect_host(),
        Arc::clone(&layer) as Arc<dyn EffectLayer>,
    );
    let scope = ExecutionScope::runtime_operation("layered-static");
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(scope.clone(), "layered-static:value").expect("address"),
            RuntimeAttribution::none(),
            "value",
        ),
        RuntimeEffectCommand::LanguageRuntimeValue {
            operation: "layered-static:value".to_string(),
        },
    );
    let run = |value: i64| {
        RuntimeEffectLocalExecutor::testing(move |_| async move {
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!(value),
            })
        })
    };
    let first = layered
        .scoped_static(AdmittedScope::runtime_operation("layered-static"))
        .expect("scope binds")
        .expect("the SQLite host lends owned controllers")
        .execute_effect(envelope.clone(), run(1))
        .await
        .expect("the first run executes");
    // The replay comes from the backend's journal: a raw host that never
    // saw the layer answers with the recorded outcome, not the new executor's.
    let replayed = backend
        .effect_host()
        .scoped(AdmittedScope::runtime_operation("layered-static"))
        .expect("scope binds")
        .execute_effect(envelope, run(2))
        .await
        .expect("the replay reads the journal");
    let value = |outcome: RuntimeEffectOutcome| match outcome {
        RuntimeEffectOutcome::LanguageRuntimeValue { value } => value,
        other => panic!("expected a language-runtime value, got {other:?}"),
    };
    assert_eq!(value(first), serde_json::json!(1));
    assert_eq!(
        value(replayed),
        serde_json::json!(1),
        "the raw host replays the outcome the layered run journaled"
    );
    assert_eq!(
        layer.seen(),
        vec!["execute:layered-static:value".to_string()]
    );
}

fn sync_await<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    })
    .join()
    .expect("runtime thread")
}

/// The shared effect-group host contract's unwired leg, answered through a
/// recording layer: the Restate double resolves group children at the
/// endpoint, so it has no unwired-host form for these laws and the layered
/// SQLite host keeps them until the SQL effect host leaves (B4).
mod layered_effect_group_host_laws {
    use super::*;

    lash_conformance::effect_group_unwired_host_tests!({
        let backend = SqliteBackend::memory()
            .await
            .expect("open a memory backend");
        let layer = Arc::new(RecordingLayer::default());
        let hosts = backend.clone();
        (
            backend,
            move |executors: Option<Arc<dyn GroupExecutors>>| {
                let backend = hosts.clone();
                let host = sync_await(async move {
                    backend
                        .reopen()
                        .await
                        .expect("reopen the backend")
                        .effect_host()
                });
                if let Some(executors) = executors {
                    host.register_group_executors(executors)
                        .expect("a freshly opened host has no resolver yet");
                }
                Arc::new(LayeredEffectHost::new(
                    host,
                    Arc::clone(&layer) as Arc<dyn EffectLayer>,
                )) as Arc<dyn EffectHost>
            },
        )
    });
}
