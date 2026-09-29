//! Shared durable-wait journals across a build roll (FIG-3795 §5.2: L10,
//! L3) on the multi-deployment server double.
//!
//! The durable-wait workflow and the process attach are shared services:
//! bound under their stable names only, their journal shapes frozen, so an
//! invocation suspended on build N may resume on build N+1. L10 proves it —
//! the precondition for the drain moving them off a build it retires. L3
//! drives the drain's `HandOver` arm (FIG-3799): a real process segment
//! waiting for a signal on build N is woken for the drain of N's generation,
//! hands its open wait to a successor segment on N+1, and the successor
//! waits on the same wait again.

use super::effect_group_generation_routing::{RunLog, build_endpoint};
use super::*;
use lash_restate_test::{
    DeploymentHooks, Refusal, RestateTestServer, ResumeDeployment, ServerConfig,
};

use crate::durable_wait::RestateDurableWaitAwaitRequest;
use crate::process::RestateProcessCompleteRequest;
use crate::process_attach::RestateProcessAttachRequest;
use lash_core::ClockWallTime as _;
use lash_core::runtime::recovery_lease::RecoveryLease;
use std::num::NonZeroUsize;

const WAIT_WORKFLOW: &str = "LashDurableWaitWorkflow";

/// Builds N and N+1 over one store set, N registered; N+1 built and
/// registered by [`Roll::register_next`].
struct Roll {
    server: RestateTestServer,
    ingress: RestateIngressClient,
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
        let (_, endpoint_n) = build_endpoint(&connection, &stores, "N", &log).await;
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
        .register_process(executed_registration())
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

/// The process the L3 segments run: it waits for the signal `go` and ends
/// with its payload.
async fn signal_waiting_registration() -> ProcessRegistration {
    let environment = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::all(),
    );
    let linked = lash_typescript::link(
        r#"
        const worker = async () => {
          return await waitSignal("go");
        };
        finish(null);
        "#,
        &environment,
    )
    .expect("link the signal-waiting TypeScript process");
    lashlang::LashlangArtifacts::publish_module_artifact(
        &recovery_artifact_store(),
        &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
            lash_core::HostArtifactPin::mint(),
        ))
        .expect("host pin claim"),
        &linked.artifact,
    )
    .await
    .expect("store the signal-waiting process artifact");
    let worker = sole_lifted_process_name(&linked.artifact);
    let env_ref = persist_recovery_env_ref().await;
    ProcessRegistration::new(
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.artifact.module_ref().clone(),
            process_ref: linked
                .artifact
                .process_ref(&worker)
                .expect("the worker process ref")
                .clone(),
            host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
            process_name: worker,
            args: serde_json::Map::new(),
        }),
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

/// A deployment's lever for L3: while `hold` is set, the build refuses every
/// process segment `run` dispatch retryably, so the segment's invocation
/// stays where the case needs it until the hold lifts.
fn holding(hold: &Arc<AtomicBool>) -> DeploymentHooks {
    let hold = Arc::clone(hold);
    DeploymentHooks {
        served: None,
        refuse: Some(Arc::new(move |dispatch| {
            (hold.load(Ordering::SeqCst)
                && dispatch.service.starts_with("LashProcessWorkflow")
                && dispatch.handler == "run")
                .then_some(Refusal::Retryable)
        })),
    }
}

/// Builds N and N+1 of every lash service over one store set, each with a
/// real process worker; N registered, N+1 registered by
/// [`HandOff::register_next`].
struct HandOff {
    server: RestateTestServer,
    ingress: RestateIngressClient,
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    host_n: Arc<RestateEffectHost>,
    host_next: Arc<RestateEffectHost>,
    deployment_n: lash_restate_test::DeploymentId,
    endpoint_next: Option<Endpoint>,
    deployment_next: Option<lash_restate_test::DeploymentId>,
    hold_n: Arc<AtomicBool>,
    hold_next: Arc<AtomicBool>,
    registration: ProcessRegistration,
    stores: lash_sqlite_store::SqliteStoreSet,
}

impl HandOff {
    async fn start(seed: u64) -> Self {
        let mut config = ServerConfig::default().with_seed(seed);
        // A held segment retries promptly and never pauses.
        config.retry.initial_interval = Duration::from_millis(10);
        config.retry.max_interval = Duration::from_millis(50);
        config.retry.max_attempts = None;
        let server = RestateTestServer::new(config).expect("start the server double");
        let connection =
            RestateConnection::with_transport(server.ingress_url(), server.transport());
        let ingress = RestateIngressClient::new(connection.clone());
        let stores = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open the shared store set");
        let storage = stores.process_registry();
        let continuations: Arc<dyn lash_core::ProcessContinuationStore> = storage.clone();
        let registry: Arc<dyn ProcessRegistry> = storage;
        let sessions = stores.session_store_factory() as Arc<dyn lash_core::SessionStoreFactory>;
        let (host_n, endpoint_n) = Self::build(
            &connection,
            &ingress,
            &registry,
            &continuations,
            &sessions,
            "N",
        )
        .await;
        let (host_next, endpoint_next) = Self::build(
            &connection,
            &ingress,
            &registry,
            &continuations,
            &sessions,
            "N+1",
        )
        .await;
        let hold_n = Arc::new(AtomicBool::new(false));
        let hold_next = Arc::new(AtomicBool::new(false));
        let deployment_n = server
            .register_with(endpoint_n, "build-N", holding(&hold_n))
            .await
            .expect("register build N");
        Self {
            server,
            ingress,
            registry,
            continuations,
            host_n,
            host_next,
            deployment_n,
            endpoint_next: Some(endpoint_next),
            deployment_next: None,
            hold_n,
            hold_next,
            registration: signal_waiting_registration().await,
            stores,
        }
    }

    async fn build(
        connection: &RestateConnection,
        ingress: &RestateIngressClient,
        registry: &Arc<dyn ProcessRegistry>,
        continuations: &Arc<dyn lash_core::ProcessContinuationStore>,
        sessions: &Arc<dyn lash_core::SessionStoreFactory>,
        build: &'static str,
    ) -> (Arc<RestateEffectHost>, Endpoint) {
        let generation = lash_core::engine::BuildGeneration::for_test(build);
        let host = Arc::new(RestateEffectHost::new_for_build(
            connection.clone(),
            test_restate_authority_id(),
            Some(generation.clone()),
            crate::RestateNamespace::default(),
        ));
        let worker = recovery_worker(Arc::clone(registry), Arc::clone(sessions)).await;
        let endpoint = crate::services::bind_lash_services(
            Endpoint::builder(),
            crate::services::LashServiceParts {
                effect_host: &host,
                ingress: ingress.clone(),
                sessions: Arc::clone(sessions),
                process_workflow: LashProcessWorkflowImpl::new(
                    Arc::new(crate::process::RestateCoreProcessRunner::new(worker)),
                    Arc::clone(registry),
                    Arc::clone(continuations),
                    ingress.clone(),
                    test_restate_authority_id(),
                    generation.clone(), &crate::services::DEFAULT_NAMESPACE
                )
                // A held segment keeps retrying for as long as a law holds
                // it — through a lease's lapse, too — and never pauses.
                .with_retry_max_attempts(10_000),
                session_driver: crate::RestateSessionDriverSlot::new(),
                build_generation: generation,
                namespace: crate::RestateNamespace::default(),
            },
        )
        .build();
        (host, endpoint)
    }

    async fn register_next(&mut self) {
        let endpoint = self.endpoint_next.take().expect("build N+1 registers once");
        self.deployment_next = Some(
            self.server
                .register_with(endpoint, "build-N+1", holding(&self.hold_next))
                .await
                .expect("register build N+1"),
        );
    }

    /// Register the process and send its segment 0 to the stable lane,
    /// where build N takes it.
    async fn start_process(&self) -> ProcessId {
        let process_id = self
            .registry
            .register_process(self.registration.clone())
            .await
            .expect("register the signal-waiting process")
            .id;
        self.ingress
            .send_workflow_json(
                "LashProcessWorkflow",
                &crate::process::process_segment_workflow_key(&process_id, 0),
                "run",
                &crate::process::RestateProcessWorkflowPayload::from(
                    crate::process::RestateProcessWorkflowInput {
                        process_id: process_id.clone(),
                        registration: self.registration.clone(),
                        execution_context: ProcessExecutionContext::default(),
                        segment_ordinal: 0,
                        sender_generation: None,
                    },
                ),
            )
            .await
            .expect("send segment 0");
        process_id
    }

    fn signal_key(&self, process_id: &ProcessId) -> AwaitEventKey {
        restate_await_event_key_for_authority(
            &test_restate_authority_id(),
            &ExecutionScope::process(process_id.clone()),
            lash_core::AwaitEventWaitIdentity::process_signal(process_id.clone(), "go", 1),
        )
        .expect("the signal wait's key")
    }

    fn segment_runs(
        &self,
        process_id: &ProcessId,
        ordinal: u64,
    ) -> Vec<lash_restate_test::InvocationView> {
        let target = format!(
            "LashProcessWorkflow/{}/run",
            crate::process::process_segment_workflow_key(process_id, ordinal)
        );
        self.server
            .invocations()
            .into_iter()
            .filter(|view| view.target == target)
            .collect()
    }

    /// Wait (bounded) for `done`, letting the double run between looks.
    async fn until(&self, what: &str, done: impl AsyncFn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !done(self).await {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{what} never happened: {:#?}",
                self.server.invocations()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Whether segment `ordinal` of `process_id` sits parked in its wait —
    /// suspended, or blocked on the server — with the process's wait state
    /// armed.
    async fn waiting_in(&self, process_id: &ProcessId, ordinal: u64) -> bool {
        let suspended = self.segment_runs(process_id, ordinal).iter().any(|view| {
            view.status == "suspended"
                || (view.status == "running" && view.blocked_on_server == Some(true))
        });
        suspended
            && self
                .registry
                .get_process(process_id)
                .await
                .ok()
                .flatten()
                .is_some_and(|record| record.wait.is_some())
    }

    async fn ended(&self, process_id: &ProcessId) -> bool {
        self.registry
            .get_process(process_id)
            .await
            .ok()
            .flatten()
            .is_some_and(|record| record.is_terminal())
    }

    /// The drain's wake of `process_id` for build N's generation.
    async fn wake(&self, process_id: &ProcessId) {
        crate::process::deliver_process_hand_over(
            &self.ingress,
            &crate::services::DEFAULT_NAMESPACE,
            self.continuations.as_ref(),
            process_id,
            &lash_core::engine::BuildGeneration::for_test("N"),
        )
        .await
        .expect("deliver the drain's wake");
    }

    async fn terminal(&self, process_id: &ProcessId) -> ProcessAwaitOutput {
        tokio::time::timeout(
            Duration::from_secs(60),
            self.ingress.call_workflow_json::<_, ProcessAwaitOutput>(
                "LashProcessWorkflow",
                process_id.as_str(),
                "await_terminal",
                &crate::process::RestateProcessAwaitRequest {
                    process_id: process_id.clone(),
                },
            ),
        )
        .await
        .expect("the process terminal arrives")
        .expect("await the process terminal")
    }
}

/// The recovery leader lease a deployment of the laws holds: a short TTL, so
/// a leader that dies without resigning lapses within the law, and no minimum
/// tenure, so the newest generation takes the lease at its first attempt.
fn law_lease(roll: &HandOff, generation_rank: i64) -> RecoveryLease {
    RecoveryLease::new(
        lash_core::StoreSet::recovery_leader(&roll.stores),
        lash_core::store::LeaseName::new("recovery:fig-3799-hand-over"),
        generation_rank,
        lash_core::engine::RecoveryLeaseTimings {
            ttl: Duration::from_millis(600),
            renew_every: Duration::from_millis(100),
            renew_timeout: Duration::from_secs(5),
            trust_margin: Duration::from_millis(100),
            follower_retry: Duration::from_millis(50),
            follower_jitter: Duration::ZERO,
            min_tenure: Duration::ZERO,
        },
        Arc::new(lash_core::facade_support::SystemClock),
    )
}

impl HandOff {
    /// Mark build N's generation draining, as an operator retiring N does.
    async fn mark_n_draining(&self) {
        assert!(
            lash_core::StoreSet::generation_drain(&self.stores)
                .mark_draining(
                    &lash_core::engine::BuildGeneration::for_test("N"),
                    lash_core::facade_support::SystemClock.timestamp_ms(),
                )
                .await
                .expect("mark N draining"),
            "N was not marked before"
        );
    }

    /// The work N's generation still holds.
    async fn n_work(&self) -> lash_core::store::generation_drain::GenerationWork {
        lash_core::StoreSet::generation_drain(&self.stores)
            .generation_work(&lash_core::engine::BuildGeneration::for_test("N"))
            .await
            .expect("read N's work")
    }

    /// One reconcile tick of a deployment of `build`, holding `lease`: the
    /// duties it runs are the lease's, as a core's tick asks them.
    async fn tick(
        &self,
        build: &'static str,
        lease: &RecoveryLease,
        cursor: &lash_core::engine::ReconcileCursor,
    ) -> lash_core::engine::ReconcileTick {
        let sessions = lash_core::StoreSet::session_store_factory(&self.stores);
        let drain = lash_core::StoreSet::generation_drain(&self.stores);
        let port = crate::process::RestateProcessIngressRunner::over_ingress(
            self.ingress.clone(),
            crate::RestateNamespace::default(),
            Arc::clone(&self.registry),
            Arc::clone(&self.continuations),
        );
        let work = lash_core::NoSessionWork::new();
        let scopes = lash_core::engine::NoScopeClose;
        let clock = lash_core::facade_support::SystemClock;
        let generation = lash_core::engine::BuildGeneration::for_test(build);
        lash_core::drive::reconcile_once(
            &lash_core::drive::ReconcileParts {
                sessions: sessions.as_ref(),
                work: &work,
                scopes: &scopes,
                processes: Some(lash_core::drive::ReconcileProcesses {
                    registry: self.registry.as_ref(),
                    port: &port,
                    drain: drain.as_ref(),
                    generation: &generation,
                }),
                clock: &clock,
                duties: lease.duties(clock.timestamp_ms()),
                relays: &[],
            },
            cursor,
            NonZeroUsize::new(16).unwrap_or(NonZeroUsize::MIN),
        )
        .await
    }

    /// Step `lease` until it leads, as its cadence would.
    async fn until_leading(&self, lease: &RecoveryLease) {
        self.until("the lease's election", async |_| {
            lease.step().await;
            lease.leads(lash_core::facade_support::SystemClock.timestamp_ms())
        })
        .await;
    }

    /// Whether segment 0 handed over and its successor is sent.
    fn handed_over(&self, process_id: &ProcessId) -> bool {
        self.segment_runs(process_id, 0)
            .iter()
            .any(|view| view.status == "completed")
            && !self.segment_runs(process_id, 1).is_empty()
    }

    /// The assertions every hand-over law ends with: the process ends with
    /// the signal, the successor ran once on N+1 and waited again rather than
    /// handing over again, and every invocation completed.
    async fn assert_handed_over_once(&self, process_id: &ProcessId, case: &str) {
        let key = self.signal_key(process_id);
        let resolution = Resolution::Ok(serde_json::json!({ "case": case }));
        assert_eq!(
            self.host_next
                .resolve_await_event(&key, resolution.clone())
                .await
                .expect("resolve the signal"),
            ResolveOutcome::Accepted,
            "{case}: the signal is the wait's first resolution"
        );
        let terminal = self.terminal(process_id).await;
        assert_eq!(
            success_value(&terminal),
            Some(serde_json::json!({ "case": case })),
            "{case}: the process ends with the signal: {terminal:?}"
        );
        self.server.settle().await;
        let successors = self.segment_runs(process_id, 1);
        assert_eq!(successors.len(), 1, "{case}: one successor runs");
        assert_eq!(
            Some(successors[0].pinned_deployment_id.as_str()),
            self.deployment_next.as_ref().map(|id| id.as_str()),
            "{case}: the successor runs on the newest build"
        );
        assert!(
            self.segment_runs(process_id, 2).is_empty(),
            "{case}: the successor waits again rather than handing over again"
        );
        for view in self.server.invocations() {
            assert_eq!(
                view.status, "completed",
                "{case}: every invocation completes: {view:#?}"
            );
        }
    }
}

/// The value a settled success carries.
fn success_value(output: &ProcessAwaitOutput) -> Option<serde_json::Value> {
    match output {
        ProcessAwaitOutput::Settled { output } => match &output.outcome {
            lash_core::ToolCallOutcome::Success(value) => Some(value.to_json_value()),
            _ => None,
        },
        _ => None,
    }
}

/// L3: a segment waiting on N is handed over by the drain to its successor
/// on N+1 (FIG-3799). For a signal issued before the drain wakes the
/// segment, between the wake and the hand-over, between the hand-over and
/// the successor's registration of the wait, and after it, the process ends
/// with that signal exactly once; the wait is never resolved `Cancelled` by
/// the hand-off; the index settles once per key, the orphaned N-side
/// `await_resolution` included; and a second resolver, on the other build,
/// is answered with the one terminal. Every hand-over runs its successor
/// once, on N+1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l3_a_wait_signal_crosses_the_drain_hand_off_exactly_once() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        let case = format!("signalled {when:?}");
        let mut roll = HandOff::start(0x3799_0003).await;
        let process_id = roll.start_process().await;
        let key = roll.signal_key(&process_id);
        let resolution = Resolution::Ok(serde_json::json!({ "case": case }));
        roll.until("segment 0 waiting on N", async |roll| {
            roll.waiting_in(&process_id, 0).await
        })
        .await;
        let signal = async |roll: &HandOff| {
            assert_eq!(
                roll.host_n
                    .resolve_await_event(&key, resolution.clone())
                    .await
                    .expect("resolve the signal"),
                ResolveOutcome::Accepted,
                "{case}: the signal is the wait's first resolution"
            );
        };
        match when {
            Resolved::BeforeWake => {
                signal(&roll).await;
                roll.until("the process end", async |roll| {
                    roll.ended(&process_id).await
                })
                .await;
                roll.register_next().await;
                roll.wake(&process_id).await;
            }
            Resolved::BetweenWakeAndHandover => {
                roll.register_next().await;
                roll.hold_n.store(true, Ordering::SeqCst);
                roll.wake(&process_id).await;
                signal(&roll).await;
                roll.hold_n.store(false, Ordering::SeqCst);
            }
            Resolved::BetweenHandoverAndRegister => {
                roll.register_next().await;
                roll.hold_next.store(true, Ordering::SeqCst);
                roll.wake(&process_id).await;
                roll.until("segment 0's hand-over", async |roll| {
                    roll.segment_runs(&process_id, 0)
                        .iter()
                        .any(|view| view.status == "completed")
                        && !roll.segment_runs(&process_id, 1).is_empty()
                })
                .await;
                signal(&roll).await;
                roll.hold_next.store(false, Ordering::SeqCst);
            }
            Resolved::AfterRegister => {
                roll.register_next().await;
                roll.wake(&process_id).await;
                roll.until("segment 1 waiting on N+1", async |roll| {
                    roll.waiting_in(&process_id, 1).await
                })
                .await;
                signal(&roll).await;
            }
        }
        let terminal = roll.terminal(&process_id).await;
        assert_eq!(
            success_value(&terminal),
            Some(serde_json::json!({ "case": case })),
            "{case}: the process ends with the signal: {terminal:?}"
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
            "{case}: the hand-off never resolved the wait Cancelled, and the loser \
             is answered with the one terminal"
        );
        roll.server.settle().await;
        assert!(
            roll.segment_runs(&process_id, 0)
                .iter()
                .all(|view| view.pinned_deployment_id == roll.deployment_n.as_str()),
            "{case}: segment 0 ran on N"
        );
        let successors = roll.segment_runs(&process_id, 1);
        if when == Resolved::BeforeWake {
            assert!(
                successors.is_empty(),
                "{case}: a wait the signal settled first is never handed over"
            );
        } else {
            assert_eq!(successors.len(), 1, "{case}: one successor runs");
            assert_eq!(
                Some(successors[0].pinned_deployment_id.as_str()),
                roll.deployment_next.as_ref().map(|id| id.as_str()),
                "{case}: the successor runs on the newest build"
            );
        }
        assert!(
            roll.segment_runs(&process_id, 2).is_empty(),
            "{case}: the successor waits again rather than handing over again"
        );
        let views = roll.server.invocations();
        // Every waiter on the key — the orphaned N-side call and the
        // successor's — returns the one resolution, and settles the index
        // with it: one record per key, whoever writes it.
        let waits = views
            .iter()
            .filter(|view| {
                view.target.starts_with("LashDurableWaitWorkflow/")
                    && view.target.ends_with("/await_resolution")
            })
            .collect::<Vec<_>>();
        assert!(!waits.is_empty(), "{case}: the segment waited");
        for wait in &waits {
            let returned = roll
                .server
                .outcome(&wait.id)
                .expect("the wait's outcome")
                .expect("the wait succeeded");
            assert_eq!(
                serde_json::from_slice::<Resolution>(&returned).expect("a resolution"),
                resolution,
                "{case}: every waiter on the key returns the signal, never Cancelled"
            );
        }
        let settles = views
            .iter()
            .filter(|view| {
                view.target.starts_with("LashDurableWaitIndex/") && view.target.ends_with("/settle")
            })
            .count();
        assert!(
            settles <= waits.len(),
            "{case}: the index settles at most once per waiter, each with the one resolution"
        );
        for view in &views {
            assert_eq!(
                view.status, "completed",
                "{case}: every invocation completes, the orphaned wait included: {view:#?}"
            );
        }
    }
}

/// L3F: the drain's hand-over survives the recovery leader's death
/// (FIG-3799, ADR 0109 §1.7). A follower's tick wakes nothing; the leader's
/// tick wakes the process waiting on N and dies without resigning, its
/// cursor lost, while the successor it caused has not started. Once the
/// dead leader's lease lapses the follower leads, and its tick wakes the
/// process again from the start: the wake lands on the successor, which
/// runs for N+1 and ignores a wake for N. The process hands over exactly
/// once, and N's generation drains to no live process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l3f_a_hand_over_survives_the_recovery_leaders_death() {
    let case = "leader failover";
    let mut roll = HandOff::start(0x3799_00f0).await;
    let process_id = roll.start_process().await;
    roll.until("segment 0 waiting on N", async |roll| {
        roll.waiting_in(&process_id, 0).await
    })
    .await;
    roll.register_next().await;
    roll.mark_n_draining().await;
    // Two cores of N+1 share the lease; the successor is held so the
    // process stays N's until the failover is over.
    roll.hold_next.store(true, Ordering::SeqCst);
    let leader = law_lease(&roll, 2);
    let follower = law_lease(&roll, 2);
    roll.until_leading(&leader).await;
    follower.step().await;

    let idle = roll.tick("N+1", &follower, &Default::default()).await;
    assert!(
        !idle.led && idle.drain_hand_over == Default::default(),
        "{case}: a follower's tick wakes nothing: {idle:?}"
    );
    assert!(
        roll.segment_runs(&process_id, 1).is_empty(),
        "{case}: nothing handed over before a leader's tick"
    );
    let led = roll.tick("N+1", &leader, &Default::default()).await;
    assert_eq!(
        (
            led.led,
            led.drain_hand_over.handled,
            led.drain_hand_over.deferred
        ),
        (true, 1, 0),
        "{case}: the leader's tick wakes the waiting process: {:?}",
        led.failures
    );
    // The leader dies: no resign, and its tick's cursor is gone with it.
    drop(led);
    drop(leader);
    roll.until("segment 0's hand-over", async |roll| {
        roll.handed_over(&process_id)
    })
    .await;
    assert_eq!(
        roll.n_work().await.live_processes,
        1,
        "{case}: the process is N's until its successor starts"
    );

    roll.until_leading(&follower).await;
    let taken_over = roll.tick("N+1", &follower, &Default::default()).await;
    assert_eq!(
        (
            taken_over.led,
            taken_over.drain_hand_over.handled,
            taken_over.drain_hand_over.deferred
        ),
        (true, 1, 0),
        "{case}: the new leader wakes the process again: {:?}",
        taken_over.failures
    );
    roll.hold_next.store(false, Ordering::SeqCst);
    roll.until("segment 1 waiting on N+1", async |roll| {
        roll.waiting_in(&process_id, 1).await
    })
    .await;
    assert_eq!(
        roll.n_work().await,
        Default::default(),
        "{case}: N's generation holds no live process once the successor runs"
    );
    let drained = roll.tick("N+1", &follower, &taken_over.next).await;
    assert_eq!(
        drained.drain_hand_over,
        Default::default(),
        "{case}: nothing is left to wake"
    );
    roll.assert_handed_over_once(&process_id, case).await;
}

/// L3R: during a rolling deploy the newest generation leads, and it is the
/// one that hands N's processes over (FIG-3799, ADR 0109 §1.7). While N
/// leads, N's drain is marked but N's own tick never wakes N's processes: a
/// successor sent now would land right back on N. Build N+1 arrives, its
/// lease outranks N's and takes the lead, N's next attempt finds it a
/// follower, and N+1's tick hands the waiting process over to N+1 once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l3r_the_newest_generation_leads_the_rolling_deploys_hand_over() {
    let case = "rolling deploy";
    let mut roll = HandOff::start(0x3799_00e0).await;
    let process_id = roll.start_process().await;
    roll.until("segment 0 waiting on N", async |roll| {
        roll.waiting_in(&process_id, 0).await
    })
    .await;
    let lease_n = law_lease(&roll, 1);
    roll.until_leading(&lease_n).await;
    roll.mark_n_draining().await;

    let own = roll.tick("N", &lease_n, &Default::default()).await;
    assert_eq!(
        (own.led, own.drain_hand_over),
        (true, Default::default()),
        "{case}: N's leader never wakes its own generation: {:?}",
        own.failures
    );
    roll.server.settle().await;
    assert!(
        roll.segment_runs(&process_id, 1).is_empty() && roll.waiting_in(&process_id, 0).await,
        "{case}: the process still waits on N"
    );

    roll.register_next().await;
    let lease_next = law_lease(&roll, 2);
    roll.until_leading(&lease_next).await;
    lease_n.step().await;
    let deposed = roll.tick("N", &lease_n, &Default::default()).await;
    assert!(
        !deposed.led && deposed.drain_hand_over == Default::default(),
        "{case}: N, outranked, runs no leader duty: {deposed:?}"
    );
    let newest = roll.tick("N+1", &lease_next, &Default::default()).await;
    assert_eq!(
        (
            newest.led,
            newest.drain_hand_over.handled,
            newest.drain_hand_over.deferred
        ),
        (true, 1, 0),
        "{case}: the newest generation's tick wakes the process: {:?}",
        newest.failures
    );
    roll.until("segment 1 waiting on N+1", async |roll| {
        roll.waiting_in(&process_id, 1).await
    })
    .await;
    assert_eq!(
        roll.n_work().await,
        Default::default(),
        "{case}: N's generation is drained of live processes"
    );
    roll.assert_handed_over_once(&process_id, case).await;
}

/// A process port that records every drain wake and fails the ones it is
/// told to, answering nothing else.
#[derive(Default)]
struct RecordedWakes {
    wakes: std::sync::Mutex<Vec<(ProcessId, lash_core::engine::BuildGeneration)>>,
    failing: std::sync::Mutex<Vec<ProcessId>>,
}

#[async_trait::async_trait]
impl lash_core::ProcessWorkSubstrate for RecordedWakes {
    async fn deliver_process_start(
        &self,
        record: &lash_core::ProcessRecord,
    ) -> Result<(), lash_core::PluginError> {
        Err(lash_core::PluginError::Invoke(format!(
            "no process start for `{}` in the slot law",
            record.id
        )))
    }

    async fn await_process_terminal(
        &self,
        process_id: &ProcessId,
    ) -> Result<lash_core::ProcessTerminalWait, lash_core::PluginError> {
        Err(lash_core::PluginError::Invoke(format!(
            "no terminal wait for `{process_id}` in the slot law"
        )))
    }

    async fn deliver_cancel(
        &self,
        _process_id: &ProcessId,
        _request: &lash_core::CancelRequest,
        _delivery_key: &str,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }

    async fn publish_process_terminal(
        &self,
        _process_id: &ProcessId,
        _output: &lash_core::ProcessAwaitOutput,
        _key: &str,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }

    async fn deliver_hand_over(
        &self,
        process_id: &ProcessId,
        generation: &lash_core::engine::BuildGeneration,
    ) -> Result<(), lash_core::PluginError> {
        self.wakes
            .lock()
            .expect("the wake log")
            .push((process_id.clone(), generation.clone()));
        if self
            .failing
            .lock()
            .expect("the failing set")
            .contains(process_id)
        {
            return Err(lash_core::PluginError::Invoke(
                "injected: the wake's send failed".to_string(),
            ));
        }
        Ok(())
    }
}

/// The drain slot's pages (FIG-3799): it wakes the live processes of every
/// draining generation but the deployment's own, in (generation, process)
/// order, at most a page per pass; the cursor resumes the next pass exactly
/// where the last stopped, across a generation's end; an unmarked
/// generation, a never-started process and an ended one are never woken; a
/// failed wake is counted deferred without failing the page, and a later
/// pass wakes it again.
#[tokio::test]
async fn the_drain_slot_pages_every_draining_generation_but_its_own() {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open the store set");
    let registry = lash_core::StoreSet::process_registry(&stores);
    let drain = lash_core::StoreSet::generation_drain(&stores);
    let generation = |label: &'static str| lash_core::engine::BuildGeneration::for_test(label);
    let own = generation("own");
    let mut marked = [generation("old"), generation("older")];
    marked.sort();
    let unmarked = generation("unmarked");
    for stamp in marked.iter().chain([&own]) {
        drain.mark_draining(stamp, 1).await.expect("mark");
    }
    let start = async |stamp: Option<&lash_core::engine::BuildGeneration>| {
        let process_id = registry
            .register_process(executed_registration())
            .await
            .expect("register")
            .id;
        if let Some(stamp) = stamp {
            let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
                process_id.clone(),
                format!("slot-law-{process_id}"),
            )
            .bind_attempt(1);
            let mut started = authority.invocation_started().expect("a start fact");
            started.build_generation = Some(stamp.clone());
            registry
                .record_first_started_with_authority(&process_id, started, &authority)
                .await
                .expect("start under the generation");
        }
        process_id
    };
    let mut expected = Vec::new();
    for stamp in &marked {
        let mut live = Vec::new();
        for _ in 0..3 {
            live.push(start(Some(stamp)).await);
        }
        live.sort();
        expected.extend(live.into_iter().map(|id| (id, stamp.clone())));
    }
    let ended = start(Some(&marked[0])).await;
    registry
        .complete_process(
            &ended,
            ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::workflow_key("slot-law-ended"),
        )
        .await
        .expect("end one of the old generation's processes");
    start(Some(&own)).await;
    start(Some(&unmarked)).await;
    start(None).await;

    let port = RecordedWakes::default();
    let processes = lash_core::drive::ReconcileProcesses {
        registry: registry.as_ref(),
        port: &port,
        drain: drain.as_ref(),
        generation: &own,
    };
    let page = NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN);
    let mut cursor = None;
    let mut passes = Vec::new();
    // Bounded: a cursor that does not advance would page forever.
    for _ in 0..8 {
        let pass = lash_core::drive::drain_hand_over_slot(&processes, cursor.as_ref(), page)
            .await
            .expect("a slot pass");
        passes.push(pass.pass.handled);
        cursor = pass.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        passes,
        [2, 2, 2, 0],
        "full pages until the end, then an empty pass"
    );
    assert_eq!(
        *port.wakes.lock().expect("the wake log"),
        expected,
        "every live process of the marked generations, in order, each once"
    );

    // A failed wake defers without failing the page; the next pass, from
    // the start, wakes it again.
    let failing = expected[1].0.clone();
    port.failing
        .lock()
        .expect("the failing set")
        .push(failing.clone());
    port.wakes.lock().expect("the wake log").clear();
    let whole = NonZeroUsize::new(64).unwrap_or(NonZeroUsize::MIN);
    let pass = lash_core::drive::drain_hand_over_slot(&processes, None, whole)
        .await
        .expect("a slot pass");
    assert_eq!((pass.pass.handled, pass.pass.deferred), (5, 1));
    assert_eq!(pass.next, None, "the pass read every generation");
    port.failing.lock().expect("the failing set").clear();
    let pass = lash_core::drive::drain_hand_over_slot(&processes, None, whole)
        .await
        .expect("a slot pass");
    assert_eq!((pass.pass.handled, pass.pass.deferred), (6, 0));

    // A cleared mark is no longer swept.
    drain.clear_draining(&marked[0]).await.expect("clear");
    port.wakes.lock().expect("the wake log").clear();
    let pass = lash_core::drive::drain_hand_over_slot(&processes, None, whole)
        .await
        .expect("a slot pass");
    assert_eq!(pass.pass.handled, 3);
    assert!(
        port.wakes
            .lock()
            .expect("the wake log")
            .iter()
            .all(|(_, stamp)| *stamp == marked[1]),
        "only the still-marked generation is woken"
    );
}
