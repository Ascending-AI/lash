//! A host layer sees a Restate group's timer and durable-wait children
//! (FIG-3780).
//!
//! A tool child's controller was already routed through the host's stack
//! (FIG-3547). A `Sleep` or `AwaitEvent` child has no tool request naming an
//! admission, so its controller is admitted from the opener the group's
//! shape records and routed the same way. This law opens such a group under a
//! process opener on the server double and requires both children to cross a
//! layer, each on a controller admitted under the opener's incarnation: the
//! opener survived the round trip through the group index and the dispatch.

use std::sync::{Arc, Mutex};

use lash_core::{
    AdmittedScope, EffectAddress, GroupExecutors, GroupWakePolicy, LoserPolicy, ProcessIncarnation,
    ProcessRef, Resolution, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectGroup,
    RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    ScopedEffectController,
};
use tokio_util::sync::CancellationToken;

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

/// What crossed the layer: each child's command kind with the process
/// incarnation its routed controller was admitted under.
type Crossings = Arc<Mutex<Vec<(&'static str, Option<ProcessRef>)>>>;

fn kind(command: &RuntimeEffectCommand) -> &'static str {
    match command {
        RuntimeEffectCommand::Sleep { .. } => "sleep",
        RuntimeEffectCommand::AwaitEvent { .. } => "await_event",
        _ => "other",
    }
}

/// Records every effect it passes through, stamped with the admission of the
/// controller it was routed onto.
struct CrossingLayer {
    crossings: Crossings,
    admitted: Option<ProcessRef>,
}

#[async_trait::async_trait]
impl lash_core::testing::EffectLayer for CrossingLayer {
    async fn execute_effect(
        &self,
        inner: &dyn RuntimeEffectController,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.crossings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((kind(&envelope.command), self.admitted.clone()));
        inner.execute_effect(envelope, local_executor).await
    }
}

/// Runs timer and durable-wait children, and routes every controller the
/// endpoint mints for them through a [`CrossingLayer`], as a layered host's
/// resolver does.
struct LayeredWaitChildren {
    crossings: Crossings,
}

impl GroupExecutors for LayeredWaitChildren {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        match envelope.command {
            RuntimeEffectCommand::Sleep { .. } => {
                Some(RuntimeEffectLocalExecutor::sleep(CancellationToken::new()))
            }
            RuntimeEffectCommand::AwaitEvent { .. } => Some(
                RuntimeEffectLocalExecutor::await_event(CancellationToken::new(), None),
            ),
            _ => None,
        }
    }

    fn route_handler_child_controller<'run>(
        &self,
        controller: ScopedEffectController<'run>,
    ) -> Result<ScopedEffectController<'run>, lash_core::RuntimeError> {
        let layer = CrossingLayer {
            crossings: Arc::clone(&self.crossings),
            admitted: controller.admitted_process().cloned(),
        };
        lash_core::testing::LayeredEffectHost::layer_scoped(controller, Arc::new(layer))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_layer_sees_a_groups_timer_and_wait_children_under_its_opener() {
    let harness = LiveConformanceHarness::start_on(HarnessServer::in_process()).await;
    let crossings: Crossings = Arc::default();
    harness.install_executors(Arc::new(LayeredWaitChildren {
        crossings: Arc::clone(&crossings),
    }));
    let host = harness.endpoint_host();
    let nonce = harness.run_nonce();
    let opener = ProcessRef::new(
        format!("layered-waits-{nonce}"),
        ProcessIncarnation::from_registration_sequence(7),
    );
    let admitted = AdmittedScope::process(opener.clone());
    let scope = admitted.scope().clone();
    let scoped = host
        .scoped(admitted)
        .expect("the process opener's controller");
    let key = format!("layered-waits-{nonce}");
    let wait_key = scoped
        .controller()
        .await_event_key(
            &scope,
            lash_core::AwaitEventWaitIdentity::tool_completion("layered-wait"),
        )
        .await
        .expect("mint the wait child's key");
    let child = |position: usize, command: RuntimeEffectCommand| {
        RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(scope.clone(), format!("{key}:child:{position}"))
                    .expect("valid child address"),
                RuntimeAttribution::none(),
                "effect",
            ),
            command,
        )
    };
    let group = RuntimeEffectGroup::try_new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(scope.clone(), format!("{key}:group")).expect("valid group address"),
            RuntimeAttribution::none(),
            "group",
        ),
        key.clone(),
        vec![
            child(
                0,
                RuntimeEffectCommand::AwaitEvent {
                    key: wait_key.clone(),
                },
            ),
            child(
                1,
                RuntimeEffectCommand::Sleep {
                    spec: lash_core::SleepSpec::For { duration_ms: 1 },
                },
            ),
        ],
        GroupWakePolicy::All,
        LoserPolicy::RunToCompletion,
    )
    .expect("the two-child group assembles");
    let mut handle = scoped
        .controller()
        .open_effect_group(group)
        .await
        .expect("the process opener opens its group");
    host.resolve_await_event(&wait_key, Resolution::Ok(serde_json::json!("done")))
        .await
        .expect("resolve the wait child");
    for _ in 0..2 {
        let settled = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            scoped.controller().await_next_settlement(
                &mut handle,
                lash_core::TurnCancelWait::unobserved(CancellationToken::new()),
            ),
        )
        .await
        .expect("a child settles within the budget")
        .expect("the settlement is served");
        assert!(
            settled.outcome.is_ok(),
            "each child settles with its own outcome: {settled:?}"
        );
    }

    let mut crossed = crossings
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    crossed.sort_by_key(|(kind, _)| *kind);
    crossed.dedup();
    assert_eq!(
        crossed,
        vec![
            ("await_event", Some(opener.clone())),
            ("sleep", Some(opener)),
        ],
        "both wait children crossed the layer, each on a controller admitted under the \
         group's recorded opener"
    );
}
