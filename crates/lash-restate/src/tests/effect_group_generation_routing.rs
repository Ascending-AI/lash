//! L4 (FIG-3795 §5.2): an effect group's children stay on the build that
//! opened the group, on the multi-deployment server double.
//!
//! Build N opens a group while build N+1 is already the newest deployment.
//! The open records the dispatch route `EffectGroupDispatch_g<G_N>`, so the
//! dispatcher and every child it sends run on N — the build whose journal
//! shape the group's children were planned under — and none on N+1. A crash
//! of the dispatcher after a child send replays onto the same lane: each
//! child runs once, under the one name its idempotency key is scoped by. A
//! retried child call with the same replay key attaches to that child. Once
//! N's deployment is removed, a group N's host opens is refused typed before
//! any group state exists, and nothing is dispatched anywhere.

use super::*;
use lash_core::{GroupExecutors, GroupWakePolicy, LoserPolicy, RuntimeEffectGroup};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{
    AttemptDispatch, CrashPoint, CrashRule, DeploymentHooks, RestateTestServer, ServerConfig,
};

use crate::effect_group::EffectGroupChildRequest;

const DISPATCH: &str = "EffectGroupDispatch";

/// Which build ran which child effect, by replay key.
pub(super) type RunLog = Arc<Mutex<Vec<(&'static str, String)>>>;

/// A build's resolver: runs every language-runtime child and logs the run
/// under the build's name.
struct BuildExecutors {
    build: &'static str,
    log: RunLog,
}

impl GroupExecutors for BuildExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let RuntimeEffectCommand::LanguageRuntimeValue { operation } = &envelope.command else {
            return None;
        };
        let operation = operation.clone();
        let replay_key = envelope.invocation.replay_key().to_owned();
        let (build, log) = (self.build, Arc::clone(&self.log));
        Some(RuntimeEffectLocalExecutor::testing(move |_| async move {
            log.lock_recover().push((build, replay_key));
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!({ "child": operation }),
            })
        }))
    }
}

/// The builds run no process here.
struct IdleRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for IdleRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &crate::SegmentStarted,
        _process_id: ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        Err(PluginError::Session(
            "no process runs in the L4 law".to_string(),
        ))
    }
}

fn generation(build: &'static str) -> lash_core::engine::BuildGeneration {
    lash_core::engine::BuildGeneration::for_test(build)
}

/// One build of every lash service over the shared stores, with its own
/// effect host (the group resolver runs children on it).
pub(super) async fn build_endpoint(
    connection: &RestateConnection,
    stores: &lash_sqlite_store::SqliteStoreSet,
    build: &'static str,
    log: &RunLog,
) -> (Arc<RestateEffectHost>, Endpoint) {
    build_endpoint_reading(connection, stores, build, log, crate::RESTATE_WIRE).await
}

/// [`build_endpoint`] for a build that reads the wire versions `reads`, as a
/// build of another release does.
pub(super) async fn build_endpoint_reading(
    connection: &RestateConnection,
    stores: &lash_sqlite_store::SqliteStoreSet,
    build: &'static str,
    log: &RunLog,
    reads: crate::VersionRange,
) -> (Arc<RestateEffectHost>, Endpoint) {
    let host = Arc::new(RestateEffectHost::new_for_build(
        connection.clone(),
        test_restate_authority_id(),
        Some(generation(build)),
        crate::RestateNamespace::default(),
    ));
    host.register_group_executors(Arc::new(BuildExecutors {
        build,
        log: Arc::clone(log),
    }))
    .expect("register the build's group resolver");
    let registry = stores.process_registry();
    let ingress = RestateIngressClient::new(connection.clone());
    let endpoint = crate::services::bind_lash_services_reading(
        Endpoint::builder(),
        crate::services::LashServiceParts {
            effect_host: &host,
            ingress: ingress.clone(),
            sessions: stores.session_store_factory() as Arc<dyn lash_core::DeploymentStore>,
            process_workflow: LashProcessWorkflowImpl::new(
                Arc::new(IdleRunner),
                Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
                registry as Arc<dyn lash_core::ProcessContinuationStore>,
                ingress,
                test_restate_authority_id(),
                generation(build),
                &crate::services::DEFAULT_NAMESPACE,
            ),
            session_driver: crate::RestateSessionDriverSlot::new(),
            build_generation: generation(build),
            namespace: crate::RestateNamespace::default(),
            fleet: crate::object_state::FleetView::default(),
        },
        reads,
    )
    .build();
    (host, endpoint)
}

fn recording(
    build: &'static str,
    served: &Arc<Mutex<Vec<(&'static str, AttemptDispatch)>>>,
) -> DeploymentHooks {
    let served = Arc::clone(served);
    DeploymentHooks {
        served: Some(Arc::new(move |dispatch: &AttemptDispatch| {
            served.lock_recover().push((build, dispatch.clone()));
        })),
        refuse: None,
    }
}

/// A group of `children` language-runtime children under `key`.
fn group(key: &str, children: usize) -> RuntimeEffectGroup {
    let scope = ExecutionScope::runtime_operation(key);
    let child = |position: usize| {
        RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(scope.clone(), format!("{key}:child:{position}"))
                    .expect("valid child address"),
                RuntimeAttribution::none(),
                "effect",
            ),
            RuntimeEffectCommand::LanguageRuntimeValue {
                operation: format!("child-{position}"),
            },
        )
    };
    RuntimeEffectGroup::try_new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(scope.clone(), format!("{key}:group")).expect("valid group address"),
            RuntimeAttribution::none(),
            "group",
        ),
        key.to_owned(),
        (0..children).map(child).collect(),
        GroupWakePolicy::All,
        LoserPolicy::RunToCompletion,
    )
    .expect("the group assembles")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l4_a_groups_children_run_on_the_build_that_opened_it() {
    const CHILDREN: usize = 3;
    let server = RestateTestServer::new(ServerConfig::default().with_seed(0x3795_d004))
        .expect("start the server double");
    let connection = RestateConnection::with_transport(server.ingress_url(), server.transport());
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open the shared store set");
    let served: Arc<Mutex<Vec<(&'static str, AttemptDispatch)>>> = Arc::default();
    let log = RunLog::default();
    let (host_n, endpoint_n) = build_endpoint(&connection, &stores, "N", &log).await;
    let (_host_next, endpoint_next) = build_endpoint(&connection, &stores, "N+1", &log).await;
    let deployment_n = server
        .register_with(endpoint_n, "build-N", recording("N", &served))
        .await
        .expect("register build N");
    server
        .register_with(endpoint_next, "build-N+1", recording("N+1", &served))
        .await
        .expect("register build N+1");
    let lane = crate::services::DEFAULT_NAMESPACE
        .generation(crate::LashService::EffectGroupDispatch, generation("N"))
        .name()
        .into_owned();

    // N's dispatcher crashes once after its first child send is issued and
    // replays onto its own lane.
    let key = "fig-3795-l4-pinned";
    server.crash_on(
        CrashRule::new(CrashPoint::BeforeFrame {
            ty: MessageType::CallCommand,
        })
        .service(lane.clone())
        .handler("run")
        .key(key)
        .within_attempts(1),
    );
    let scoped = host_n
        .scoped(lash_core::AdmittedScope::runtime_operation(key))
        .expect("the opener's controller");
    let mut handle = scoped
        .controller()
        .open_effect_group(group(key, CHILDREN))
        .await
        .expect("build N opens its group while N+1 is newest");
    let mut outcomes = Vec::new();
    for _ in 0..CHILDREN {
        let settled = tokio::time::timeout(
            Duration::from_secs(60),
            scoped.controller().await_next_settlement(
                &mut handle,
                lash_core::TurnCancelWait::unobserved(tokio_util::sync::CancellationToken::new()),
            ),
        )
        .await
        .expect("a child settles within the budget")
        .expect("the settlement is served");
        outcomes.push(settled.position);
        assert!(settled.outcome.is_ok(), "each child succeeds: {settled:?}");
    }
    server.settle().await;
    outcomes.sort_unstable();
    assert_eq!(
        outcomes,
        (0..CHILDREN).collect::<Vec<_>>(),
        "the parent consumes exactly one result per position"
    );
    assert_eq!(server.stats().crashes, 1, "the dispatcher crashed once");

    let dispatches: Vec<_> = served
        .lock_recover()
        .iter()
        .filter(|(_, dispatch)| dispatch.service.starts_with(DISPATCH))
        .cloned()
        .collect();
    assert!(
        dispatches
            .iter()
            .all(|(build, dispatch)| *build == "N" && dispatch.service == lane),
        "the dispatcher and every child ran on N's lane: {dispatches:#?}"
    );
    let child_invocations: Vec<_> = server
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with(DISPATCH) && view.target.ends_with("/child"))
        .collect();
    assert_eq!(
        child_invocations.len(),
        CHILDREN,
        "one child invocation per position, the replayed send included: {child_invocations:#?}"
    );
    assert!(
        child_invocations.iter().all(|view| {
            view.target == format!("{lane}/{key}/child")
                && view.pinned_deployment_id == deployment_n.as_str()
        }),
        "every child is pinned to N under N's lane: {child_invocations:#?}"
    );
    let mut runs = log.lock_recover().clone();
    runs.sort();
    let expected: Vec<(&'static str, String)> = (0..CHILDREN)
        .map(|position| ("N", format!("{key}:child:{position}")))
        .collect();
    assert_eq!(runs, expected, "each child's effect ran exactly once, on N");

    // A retried child call with the same replay key attaches to the child
    // that lane started: no second run, on this name or any other.
    let (group_shape, _) = crate::effect_group::EffectGroupShape::from_group(
        &group(key, CHILDREN),
        &lash_core::AdmittedScope::runtime_operation(key),
    )
    .expect("the group's shape");
    let replay_key = format!("{key}:child:0");
    let envelope = group(key, CHILDREN).children()[0].clone();
    let retried: () = RestateIngressClient::new(connection.clone())
        .call_lash_workflow_idempotent(
            &lane,
            key,
            "child",
            &EffectGroupChildRequest {
                group_key: key.to_owned(),
                shape: group_shape,
                position: 0,
                envelope,
            },
            &replay_key,
        )
        .await
        .expect("the retried child attaches to the one that ran");
    let () = retried;
    server.settle().await;
    assert_eq!(
        server
            .invocations()
            .into_iter()
            .filter(|view| view.target.ends_with("/child"))
            .count(),
        CHILDREN,
        "the retry started no second child"
    );
    assert_eq!(
        log.lock_recover()
            .iter()
            .filter(|(_, run)| *run == replay_key)
            .count(),
        1,
        "the retried child did not run again"
    );

    // With N's deployment gone, a group N's host opens is refused typed
    // before any state is created, and nothing is dispatched.
    server
        .remove_deployment(&deployment_n, true)
        .expect("remove build N");
    let before = served.lock_recover().len();
    let gone = "fig-3795-l4-removed";
    let refused = host_n
        .scoped(lash_core::AdmittedScope::runtime_operation(gone))
        .expect("the opener's controller")
        .controller()
        .open_effect_group(group(gone, CHILDREN))
        .await
        .expect_err("a group on a removed build's lane is refused");
    assert!(
        refused.to_string().contains(&lane),
        "the refusal names the lane nothing serves: {refused}"
    );
    server.settle().await;
    let after: Vec<_> = served.lock_recover()[before..].to_vec();
    assert!(
        after
            .iter()
            .all(|(_, dispatch)| !dispatch.service.starts_with(DISPATCH)),
        "zero dispatch after the removal: {after:#?}"
    );
    assert!(
        log.lock_recover()
            .iter()
            .all(|(_, run)| !run.starts_with(gone)),
        "no child of the refused group ran"
    );
    assert!(
        server
            .invocations()
            .iter()
            .all(|view| view.target != format!("EffectGroupIndex/{gone}/open")),
        "the refused group was never opened: no group state exists for it"
    );
}
