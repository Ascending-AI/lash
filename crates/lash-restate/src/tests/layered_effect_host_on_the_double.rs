//! `LayeredEffectHost` over the deployment host the in-process server double
//! serves (FIG-3580, ported from `lash-sqlite-store` under FIG-3668): a layer
//! observes the seam, and the endpoint's group state arbitrates every group.
//!
//! The journaled-effect leg of the old file stays on SQLite for B4 — the
//! deployment host refuses `execute_effect` outside a handler — and the
//! unwired-host laws stay there too: a Restate host resolves its group
//! children at the endpoint, so it has no unwired form.

use std::sync::{Arc, Mutex};

use lash_core::testing::{EffectLayer, LayeredEffectHost};
use lash_core::{
    AdmittedScope, EffectAddress, EffectGroupHandle, EffectHost, ExecutionScope, GroupExecutors,
    GroupSettlement, GroupWakePolicy, LoserPolicy, RuntimeAttribution, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectGroup, RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeErrorCode,
};
use lash_sansio::sync::MutexExt;

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use crate::RestateEffectHost;

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

/// A recording layer over the deployment host opens a group; the endpoint's
/// group state, not the layer, decides every later answer about it. A second,
/// unlayered host over the same endpoint reopens the group the layered
/// controller recorded and is refused a narrower shape under its key, and the
/// settlements the layered controller reads are the ones the group state
/// ranked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layered_group_is_arbitrated_by_the_backend_journal() {
    let harness = LiveConformanceHarness::start_on(HarnessServer::in_process()).await;
    let connection = harness.connection();
    harness.install_executors(Arc::new(EchoChildren));
    let layer = Arc::new(RecordingLayer::default());
    let layered = LayeredEffectHost::new(
        Arc::new(RestateEffectHost::new_for_test(connection.clone())) as Arc<dyn EffectHost>,
        Arc::clone(&layer) as Arc<dyn EffectLayer>,
    );
    let scope = ExecutionScope::runtime_operation("layered-group");
    let admitted = || AdmittedScope::runtime_operation("layered-group");

    let view = layered.scoped(admitted()).expect("the layered host scopes");
    let mut handle = view
        .controller()
        .open_effect_group(group(&scope, "layered", 2))
        .await
        .expect("the layered controller opens the group on the endpoint");

    // Another host over the same endpoint never saw the layer: what it
    // answers comes from the state the layered open recorded.
    let unlayered: Arc<dyn EffectHost> =
        Arc::new(RestateEffectHost::new_for_test(connection.clone()));
    let unlayered_view = unlayered.scoped(admitted()).expect("the raw host scopes");
    let refusal = unlayered_view
        .controller()
        .open_effect_group(group(&scope, "layered", 1))
        .await
        .expect_err("the group state holds a two-child group under this key");
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
            .await_next_settlement(
                &mut handle,
                lash_core::TurnCancelWait::unobserved(lash_core::CancellationToken::new()),
            )
            .await
            .expect("the group state serves each rank");
        consumed.push(settlement.position);
        let recorded = unlayered_view
            .controller()
            .read_group_settlement("layered", consumed.len() as u64)
            .await
            .expect("the raw host reads the rank")
            .expect("the rank the layered caller consumed is recorded");
        assert_eq!(
            recorded.sequence, settlement.sequence,
            "both hosts read one group's ranks"
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

/// The shared effect-group host contract, answered through a recording
/// layer: every host the suite asks for is a layered view of the endpoint's
/// deployment host.
mod layered_effect_group_host_laws {
    use super::*;

    /// The `(guard, make)` tuple the group-host suites destructure: a fresh
    /// layered host per call, its resolver installed at the endpoint the way
    /// [`LiveConformanceHarness::group_host_factory`] installs it.
    type LayeredHostFactory =
        Box<dyn Fn(Option<Arc<dyn GroupExecutors>>) -> Arc<dyn EffectHost> + Send + Sync>;

    fn fixture(
        harness: Arc<LiveConformanceHarness>,
    ) -> (Arc<LiveConformanceHarness>, LayeredHostFactory) {
        let layer = Arc::new(RecordingLayer::default());
        let connection = harness.connection();
        let install = Arc::clone(&harness);
        (
            harness,
            Box::new(move |executors: Option<Arc<dyn GroupExecutors>>| {
                let Some(executors) = executors else {
                    panic!(
                        "the Restate legs register no unwired-host group laws; no law asks \
                         them for an unwired host"
                    )
                };
                install.install_executors(executors);
                Arc::new(LayeredEffectHost::new(
                    Arc::new(RestateEffectHost::new_for_test(connection.clone()))
                        as Arc<dyn EffectHost>,
                    Arc::clone(&layer) as Arc<dyn EffectLayer>,
                )) as Arc<dyn EffectHost>
            }),
        )
    }

    lash_conformance::effect_group_host_tests!({
        let harness = Arc::new(LiveConformanceHarness::start_on(HarnessServer::in_process()).await);
        fixture(harness)
    });

    // A close racing its own children's settlements seats one terminal per
    // child, through the same layered host.
    lash_conformance::effect_group_close_race_tests!({
        let harness = Arc::new(LiveConformanceHarness::start_on(HarnessServer::in_process()).await);
        fixture(harness)
    });
}
