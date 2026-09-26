//! Shared durable-wait journals across a build roll (FIG-3795 §5.2: L10,
//! L3) on the multi-deployment server double.
//!
//! The durable-wait workflow and the process attach are shared services:
//! bound under their stable names only, their journal shapes frozen, so an
//! invocation suspended on build N may resume on build N+1. L10 proves it —
//! the precondition for the drain moving them off a build it retires. L3,
//! the wait signal across the drain's hand-off of a waiting segment, needs
//! the drain's `HandOver` arm (FIG-3799) and is written here ignored until
//! that arm lands.

use super::effect_group_generation_routing::{RunLog, build_endpoint};
use super::*;
use lash_restate_test::{
    DeploymentHooks, Refusal, RestateTestServer, ResumeDeployment, ServerConfig,
};

use crate::durable_wait::RestateDurableWaitAwaitRequest;
use crate::process::RestateProcessCompleteRequest;
use crate::process_attach::RestateProcessAttachRequest;

const WAIT_WORKFLOW: &str = "LashDurableWaitWorkflow";

/// Builds N and N+1 over one store set, N registered; N+1 built and
/// registered by [`Roll::register_next`].
struct Roll {
    server: RestateTestServer,
    ingress: RestateIngressClient,
    host_n: Arc<RestateEffectHost>,
    host_next: Arc<RestateEffectHost>,
    stores: lash_sqlite_store::SqliteStoreSet,
    deployment_n: lash_restate_test::DeploymentId,
    endpoint_next: Option<Endpoint>,
    /// Build N refuses every dispatch once it is retiring, so an invocation
    /// suspended on it pauses when it wakes, for the drain to move.
    retiring: Arc<AtomicBool>,
}

impl Roll {
    async fn start(seed: u64) -> Self {
        // Every await suspends, so each shared invocation below is suspended
        // on N when N+1 arrives, and wakes by a fresh dispatch; a refused
        // wake pauses after two attempts.
        let mut config = ServerConfig::default().with_seed(seed).always_replay(true);
        config.retry.max_attempts = Some(2);
        let server = RestateTestServer::new(config).expect("start the server double");
        let connection =
            RestateConnection::with_transport(server.ingress_url(), server.transport());
        let stores = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open the shared store set");
        let log = RunLog::default();
        let (host_n, endpoint_n) = build_endpoint(&connection, &stores, "N", &log).await;
        let (host_next, endpoint_next) = build_endpoint(&connection, &stores, "N+1", &log).await;
        let retiring = Arc::new(AtomicBool::new(false));
        let refusing = Arc::clone(&retiring);
        let deployment_n = server
            .register_with(
                endpoint_n,
                "build-N",
                DeploymentHooks {
                    served: None,
                    refuse: Some(Arc::new(move |_| {
                        refusing
                            .load(Ordering::SeqCst)
                            .then_some(Refusal::Retryable)
                    })),
                },
            )
            .await
            .expect("register build N");
        Self {
            server,
            ingress: RestateIngressClient::new(connection),
            host_n,
            host_next,
            stores,
            deployment_n,
            endpoint_next: Some(endpoint_next),
            retiring,
        }
    }

    async fn register_next(&mut self) -> lash_restate_test::DeploymentId {
        self.server
            .register_with(
                self.endpoint_next.take().expect("build N+1 registers once"),
                "build-N+1",
                Default::default(),
            )
            .await
            .expect("register build N+1")
    }

    fn view(&self, target: &str) -> lash_restate_test::InvocationView {
        self.server
            .invocations()
            .into_iter()
            .find(|view| view.target == target)
            .unwrap_or_else(|| panic!("an invocation of `{target}`"))
    }

    async fn wait_for(&self, target: &str, status: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if self
                .server
                .invocations()
                .iter()
                .any(|view| view.target == target && view.status == status)
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "`{target}` never reached `{status}`: {:#?}",
                self.server.invocations()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Retire build N: from now on it refuses every dispatch.
    fn retire_n(&self) {
        self.retiring.store(true, Ordering::SeqCst);
    }

    /// The drain's move: resume every invocation paused on N on the newest
    /// deployment serving its name, until `done`.
    async fn drain_n_until(&self, done: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !done() {
            for view in self.server.invocations() {
                if view.pinned_deployment_id == self.deployment_n.as_str()
                    && view.status == "paused"
                {
                    assert_eq!(
                        self.server.resume_on(&view.id, &ResumeDeployment::Latest),
                        Some(Ok(())),
                        "resume `{}` on the newest build",
                        view.target
                    );
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the drain never finished: {:#?}",
                self.server.invocations()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn remove_n(&self) {
        self.server
            .remove_deployment(&self.deployment_n, false)
            .expect("nothing open is pinned to N any more");
    }

    fn wait_key(&self, label: &str) -> AwaitEventKey {
        test_restate_await_event_key(
            &ExecutionScope::runtime_operation(label),
            lash_core::AwaitEventWaitIdentity::tool_completion(label),
        )
        .expect("a wait key")
    }

    fn await_resolution(
        &self,
        key: &AwaitEventKey,
    ) -> tokio::task::JoinHandle<Result<Resolution, String>> {
        let ingress = self.ingress.clone();
        let request = RestateDurableWaitAwaitRequest {
            key: key.clone(),
            deadline: None,
        };
        let workflow_key = crate::RestateDurableWaitAddress::for_key(key).workflow_key;
        tokio::spawn(async move {
            ingress
                .call_workflow_json::<_, Resolution>(
                    WAIT_WORKFLOW,
                    &workflow_key,
                    "await_resolution",
                    &request,
                )
                .await
                .map_err(|error| error.to_string())
        })
    }
}

async fn joined<T>(task: tokio::task::JoinHandle<Result<T, String>>, what: &str) -> T {
    tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .unwrap_or_else(|_| panic!("{what} never returned"))
        .expect("the task")
        .unwrap_or_else(|error| panic!("{what} failed: {error}"))
}

/// L10: an `await_resolution` and a `LashProcessAttach` suspended on N
/// resume on N+1 by resume-on-deployment, N is removed, and each completes
/// as it would have on N: the wait returns its one resolution, the attach
/// resolves its wait with the process terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l10_shared_wait_journals_suspended_on_n_resume_on_n_plus_1() {
    let mut roll = Roll::start(0x3795_d010).await;

    // A wait on N.
    let key = roll.wait_key("fig-3795-l10-wait");
    let workflow_key = crate::RestateDurableWaitAddress::for_key(&key).workflow_key;
    let waiter = roll.await_resolution(&key);
    let wait_target = format!("{WAIT_WORKFLOW}/{workflow_key}/await_resolution");
    roll.wait_for(&wait_target, "suspended").await;

    // An attach on N, for a process whose terminal is still to come.
    let registry: Arc<dyn ProcessRegistry> = roll.stores.process_registry();
    let process_id = registry
        .register_process(rerunnable_registration())
        .await
        .expect("register the attached process")
        .id;
    let attach_key = roll.wait_key("fig-3795-l10-attach");
    let attach_workflow = crate::process_attach::process_attach_workflow_key(&attach_key);
    let attached = roll.await_resolution(&attach_key);
    roll.ingress
        .send_workflow_json(
            "LashProcessAttach",
            &attach_workflow,
            "run",
            &RestateProcessAttachRequest {
                process_id: process_id.clone(),
                key: attach_key.clone(),
            },
        )
        .await
        .expect("arm the attach");
    let attach_target = format!("LashProcessAttach/{attach_workflow}/run");
    roll.wait_for(&attach_target, "suspended").await;
    roll.server.settle().await;
    for target in [&wait_target, &attach_target] {
        assert_eq!(
            roll.view(target).pinned_deployment_id,
            roll.deployment_n.as_str(),
            "`{target}` started on N"
        );
    }

    let next = roll.register_next().await;
    roll.retire_n();

    // Both complete on N+1 as they would have on N.
    let resolution = Resolution::Ok(serde_json::json!({ "resolved": "after the roll" }));
    assert_eq!(
        roll.host_next
            .resolve_await_event(&key, resolution.clone())
            .await
            .expect("resolve the wait suspended on N"),
        ResolveOutcome::Accepted
    );
    roll.drain_n_until(|| waiter.is_finished()).await;
    assert_eq!(joined(waiter, "the moved wait").await, resolution);

    let terminal = process_success(serde_json::json!({ "process": "ended" }));
    roll.ingress
        .call_workflow_json::<_, ()>(
            "LashProcessWorkflow",
            process_id.as_str(),
            "complete_terminal",
            &RestateProcessCompleteRequest {
                process_id: process_id.clone(),
                output: terminal.clone(),
            },
        )
        .await
        .expect("publish the process terminal");
    roll.drain_n_until(|| attached.is_finished()).await;
    let Resolution::Ok(value) = joined(attached, "the attach's wait").await else {
        panic!("the attach resolves its wait with the terminal");
    };
    assert_eq!(
        serde_json::from_value::<ProcessAwaitOutput>(value).expect("a process terminal"),
        terminal,
        "the moved attach resolved its wait with the process terminal"
    );
    roll.drain_n_until(|| roll.view(&attach_target).status == "completed")
        .await;
    for target in [&wait_target, &attach_target] {
        let view = roll.view(target);
        assert_eq!(view.status, "completed", "`{target}` completed");
        assert_eq!(
            view.pinned_deployment_id,
            next.as_str(),
            "`{target}` resumed on N+1"
        );
    }
    roll.server.settle().await;
    roll.remove_n();
}

/// L3: a segment waiting on N is handed over by the drain to its successor
/// on N+1. For a resolution issued before the drain wakes the segment,
/// between the wake and the handover, between the handover and the
/// successor's registration of the wait, and after it, the successor's wait
/// returns that resolution exactly once; the wait is never resolved
/// `Cancelled` by the hand-off; the index settles once per key, the orphaned
/// N-side `await_resolution` included; and two resolvers on N and N+1 end in
/// one terminal, the loser answered with the same one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-3799: the drain's HandOver arm (wake a waiting segment, hand its wait to the successor) has not landed"]
async fn l3_a_wait_signal_crosses_the_drain_hand_off_exactly_once() {
    #[derive(Clone, Copy, Debug)]
    enum Resolved {
        BeforeWake,
        BetweenWakeAndHandover,
        BetweenHandoverAndRegister,
        AfterRegister,
    }
    for when in [
        Resolved::BeforeWake,
        Resolved::BetweenWakeAndHandover,
        Resolved::BetweenHandoverAndRegister,
        Resolved::AfterRegister,
    ] {
        let case = format!("resolved {when:?}");
        let mut roll = Roll::start(0x3795_d003).await;
        let key = roll.wait_key(&format!("fig-3795-l3-{when:?}"));
        let resolution = Resolution::Ok(serde_json::json!({ "case": case }));
        let host_n = Arc::clone(&roll.host_n);
        let resolve_on_n = || host_n.resolve_await_event(&key, resolution.clone());

        // The segment's wait, on N.
        let orphan = roll.await_resolution(&key);
        if matches!(when, Resolved::BeforeWake) {
            assert_eq!(
                resolve_on_n().await.expect("resolve"),
                ResolveOutcome::Accepted
            );
        }
        // The drain's wake of the waiting segment (FIG-3799).
        roll.register_next().await;
        if matches!(when, Resolved::BetweenWakeAndHandover) {
            assert_eq!(
                resolve_on_n().await.expect("resolve"),
                ResolveOutcome::Accepted
            );
        }
        // The drain's HandOver: build N retires and the successor on N+1
        // takes the wait over.
        roll.retire_n();
        if matches!(when, Resolved::BetweenHandoverAndRegister) {
            assert_eq!(
                resolve_on_n().await.expect("resolve"),
                ResolveOutcome::Accepted
            );
        }
        // The successor registers and awaits the same wait on N+1.
        let successor = roll.await_resolution(&key);
        if matches!(when, Resolved::AfterRegister) {
            assert_eq!(
                resolve_on_n().await.expect("resolve"),
                ResolveOutcome::Accepted
            );
        }
        assert_eq!(
            joined(successor, "the successor's wait").await,
            resolution,
            "{case}: the successor's wait returns the resolution"
        );
        roll.drain_n_until(|| orphan.is_finished()).await;
        assert_eq!(
            joined(orphan, "the orphaned wait").await,
            resolution,
            "{case}: the hand-off never resolves the wait Cancelled"
        );
        // A second resolver, on the other build, loses to the first.
        assert_eq!(
            roll.host_next
                .resolve_await_event(&key, Resolution::Ok(serde_json::json!("late")))
                .await
                .expect("the late resolver"),
            ResolveOutcome::AlreadyResolved {
                terminal: resolution.clone()
            },
            "{case}: the loser is answered with the one terminal"
        );
        roll.server.settle().await;
        let settles = roll
            .server
            .invocations()
            .into_iter()
            .filter(|view| {
                view.target.starts_with("LashDurableWaitIndex/") && view.target.ends_with("/settle")
            })
            .count();
        assert!(settles <= 1, "{case}: the index settles once per key");
    }
}
