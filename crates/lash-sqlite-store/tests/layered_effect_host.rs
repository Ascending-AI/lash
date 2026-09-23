//! `LayeredEffectHost` over a SQLite memory deployment (FIG-3580): a layer
//! observes the seam, and the deployment's journal arbitrates every group.

use std::sync::{Arc, Mutex};

use lash_core_execution::testing::{EffectLayer, LayeredEffectHost};
use lash_core_execution::{
    AdmittedScope, EffectAddress, EffectGroupHandle, EffectHost, ExecutionScope, GroupExecutors,
    GroupSettlement, GroupWakePolicy, LoserPolicy, RuntimeAttribution, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectGroup, RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeErrorCode,
};
use lash_sansio::sync::MutexExt;
use lash_sqlite_store::SqliteDeployment;

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
        cancel: lash_core_execution::CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        let settlement = inner.await_next_settlement(handle, cancel).await?;
        self.record(format!("settle:{}", settlement.position));
        Ok(settlement)
    }
}

/// Runs every child as a language-runtime value naming its own replay key.
struct EchoChildren;

impl GroupExecutors for EchoChildren {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let key = envelope.invocation.replay_key().to_string();
        Some(RuntimeEffectLocalExecutor::testing(move |_| async move {
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!(key),
            })
        }))
    }
}

fn group(scope: &ExecutionScope, key: &str, children: usize) -> RuntimeEffectGroup {
    let invocation = |replay_key: String, label: &str| {
        RuntimeEffectInvocation::new(
            EffectAddress::new(scope.clone(), replay_key).expect("a valid effect address"),
            RuntimeAttribution::none(),
            label,
        )
    };
    RuntimeEffectGroup::try_new(
        invocation(format!("{key}:group"), "effect-group"),
        key.to_string(),
        (0..children)
            .map(|child| {
                RuntimeEffectEnvelope::new(
                    invocation(format!("{key}:child:{child}"), "child"),
                    RuntimeEffectCommand::LanguageRuntimeValue {
                        operation: format!("{key}:child:{child}"),
                    },
                )
            })
            .collect(),
        GroupWakePolicy::All,
        LoserPolicy::RunToCompletion,
    )
    .expect("a well-formed group")
}

/// A recording layer over a memory deployment opens a group; the deployment's
/// journal, not the layer, decides every later answer about it. A second,
/// unlayered host over the same deployment reopens the group the layered
/// controller recorded and is refused a narrower shape under its key, and the
/// settlements the layered controller reads are the ones the journal ranked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layered_group_is_arbitrated_by_the_deployment_journal() {
    let deployment = SqliteDeployment::memory()
        .await
        .expect("open a memory deployment");
    deployment
        .effect_host()
        .register_group_executors(Arc::new(EchoChildren))
        .expect("the fresh host takes the resolver");
    let layer = Arc::new(RecordingLayer::default());
    let layered = LayeredEffectHost::new(
        deployment.effect_host(),
        Arc::clone(&layer) as Arc<dyn EffectLayer>,
    );
    let scope = ExecutionScope::runtime_operation("layered-group");
    let admitted = || AdmittedScope::runtime_operation("layered-group");

    let view = layered.scoped(admitted()).expect("the layered host scopes");
    let mut handle = view
        .controller()
        .open_effect_group(group(&scope, "layered", 2))
        .await
        .expect("the layered controller opens the group on the journal");

    // Another host over the same databases never saw the layer: what it
    // answers comes from the rows the layered open wrote.
    let unlayered = deployment
        .reopen()
        .await
        .expect("reopen the deployment")
        .effect_host();
    unlayered
        .register_group_executors(Arc::new(EchoChildren))
        .expect("the reopened host takes the resolver");
    let unlayered_view = unlayered.scoped(admitted()).expect("the raw host scopes");
    let refusal = unlayered_view
        .controller()
        .open_effect_group(group(&scope, "layered", 1))
        .await
        .expect_err("the journal holds a two-child group under this key");
    assert_eq!(refusal.code, RuntimeErrorCode::RuntimeEffectGroupShape);
    let reopened = unlayered_view
        .controller()
        .open_effect_group(group(&scope, "layered", 2))
        .await
        .expect("the same shape reopens the recorded group");
    assert_eq!(reopened.group_key(), handle.group_key());

    let mut consumed = Vec::new();
    while !handle.is_exhausted() {
        let settlement = view
            .controller()
            .await_next_settlement(&mut handle, lash_core_execution::CancellationToken::new())
            .await
            .expect("the journal serves each rank");
        consumed.push(settlement.position);
        let recorded = unlayered_view
            .controller()
            .read_group_settlement("layered", consumed.len() as u64)
            .await
            .expect("the raw host reads the rank")
            .expect("the rank the layered caller consumed is recorded");
        assert_eq!(
            recorded.sequence, settlement.sequence,
            "both hosts read one journal's ranks"
        );
    }
    view.controller()
        .close_effect_group(handle, LoserPolicy::RunToCompletion)
        .await
        .expect("close the group");

    let mut positions = consumed.clone();
    positions.sort_unstable();
    assert_eq!(positions, vec![0, 1]);
    let mut expected = vec!["open:layered".to_string()];
    expected.extend(consumed.iter().map(|position| format!("settle:{position}")));
    assert_eq!(
        layer.seen(),
        expected,
        "the layer saw the one open and each settlement it forwarded, and \
         nothing the unlayered host did"
    );
}

/// An effect run through a layered controller is journaled by the deployment:
/// the layer records it, and a raw host over the same deployment replays the
/// recorded outcome instead of running its own executor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layered_effect_is_journaled_by_the_deployment() {
    let deployment = SqliteDeployment::memory()
        .await
        .expect("open a memory deployment");
    let layer = Arc::new(RecordingLayer::default());
    let layered = LayeredEffectHost::new(
        deployment.effect_host(),
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
    // The replay comes from the deployment's journal: a raw host that never
    // saw the layer answers with the recorded outcome, not the new executor's.
    let replayed = deployment
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

/// The whole shared effect-group host contract, answered through a recording
/// layer: every host the suite asks for is a layered view of one memory
/// deployment's journal.
mod layered_effect_group_host_laws {
    use super::*;

    lash_conformance::effect_group_host_tests!({
        let deployment = SqliteDeployment::memory()
            .await
            .expect("open a memory deployment");
        let layer = Arc::new(RecordingLayer::default());
        let hosts = deployment.clone();
        (
            deployment,
            move |executors: Option<Arc<dyn GroupExecutors>>| {
                let deployment = hosts.clone();
                let host = sync_await(async move {
                    deployment
                        .reopen()
                        .await
                        .expect("reopen the deployment")
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
