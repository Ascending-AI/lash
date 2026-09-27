//! Parent-end plans on Restate (ADR 0094, FIG-3822), on the in-process
//! server double through the endpoint's real handlers.
//!
//! A root's end is the root-close step after its terminal evidence (S7-A's
//! `CloseRootScope`), whose scope-close sink records the root's plan and
//! applies it: `ParentEnded` is delivered to each live `Until` child's
//! process workflow before the request is recorded. The laws drive the root
//! through the real drive and close it through the registry's production
//! sink ([`RegistryScopeClose`]) — over the endpoint's process port for
//! delivery, either installed on the drive's own runtime or called directly.
//! A process's end records its plan inside its terminal completion, and the
//! workflow's next journaled step applies it. A `Detached` child belongs to
//! the host and outlives both. A plan whose ending died before applying it
//! is applied by the reconcile tick's parent-end arm. A frame switch is not
//! a root's end: it owes a follow-on, writes no terminal evidence, and the
//! close never runs for it (FIG-3554).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::engine::{DriveRequest, DriveRequestId, RootOutcome};
use lash_core::{ProcessId, ProcessRegistry, ScopeId, SessionId, TurnId};

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

const PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::MIN.saturating_add(15);

/// One law's world: the harness, a backend over its store set, and a
/// session on it.
struct World {
    harness: LiveConformanceHarness,
    backend: lash_core::Backend,
    registry: Arc<dyn ProcessRegistry>,
    session_id: SessionId,
    store: Arc<dyn lash_core::RuntimePersistence>,
    nonce: u128,
    /// Whether the law's runtime closes root scopes through the registry's
    /// delivering sink itself: `true` exercises the drive's own close, and
    /// `false` leaves the close to the law's explicit sink calls.
    close_in_drive: bool,
}

impl World {
    /// A world whose drive closes root scopes through the registry's
    /// delivering sink: the production `RuntimeControlConfig.scope_close`
    /// wiring.
    async fn start(law: &str) -> Self {
        Self::build(law, |backend| backend, true).await
    }

    /// A world whose backend is `layer` over the harness's Restate backend,
    /// whose runtime leaves the root close to the law's own sink calls.
    async fn start_with(
        law: &str,
        layer: impl FnOnce(lash_core::Backend) -> lash_core::Backend,
    ) -> Self {
        Self::build(law, layer, false).await
    }

    async fn build(
        law: &str,
        layer: impl FnOnce(lash_core::Backend) -> lash_core::Backend,
        close_in_drive: bool,
    ) -> Self {
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
        let nonce = harness.run_nonce();
        let backend = layer(harness.law_backend());
        let registry = backend.process_registry();
        let session_id = SessionId::from(format!("parent-end-{law}-{nonce}"));
        let store = backend
            .session_store_factory()
            .create_store(&lash_core::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: lash_core::SessionRelation::Root,
                policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            })
            .await
            .expect("create the law's session store");
        Self {
            harness,
            backend,
            registry,
            session_id,
            store,
            nonce,
            close_in_drive,
        }
    }

    fn root(&self, name: &str) -> TurnId {
        TurnId::from(format!("{name}-{}", self.nonce))
    }

    /// Register a child of `parent` that lives `Until` it, returning the id
    /// the registrar minted.
    async fn register_until_child(&self, name: &str, parent: &ScopeId) -> ProcessId {
        self.register_child(name, parent, true).await
    }

    /// Register a child of `parent` `Detached` from it, returning the id the
    /// registrar minted.
    async fn register_detached_child(&self, name: &str, parent: &ScopeId) -> ProcessId {
        self.register_child(name, parent, false).await
    }

    async fn register_child(&self, name: &str, parent: &ScopeId, until: bool) -> ProcessId {
        let _ = name;
        let mut registration = lash_core::ProcessRegistration::new(
            lash_core::ProcessInput::External {
                metadata: serde_json::json!({ "law": "parent-end" }),
            },
            lash_core::RecoveryContract::Rerunnable,
            lash_core::ProcessProvenance::session(lash_core::SessionScope::new(
                self.session_id.as_str(),
            )),
            lash_core::Lifetime::Detached,
        );
        registration.ancestry = lash_core::Ancestry::from_scopes([parent.clone()]);
        if until {
            registration.lifetime = lash_core::LifetimeDecision::Until {
                scope: parent.clone(),
                grant: lash_core::ScopeGrant::Ancestor,
            };
        }
        self.registry
            .register_process(registration)
            .await
            .expect("register the law's child under a live parent")
            .id
    }

    async fn runtime(&self) -> lash_core::facade_support::LashRuntime {
        let model = lash_core::testing::TestProvider::builder()
            .kind("stub")
            .complete(|_request| async {
                Ok(lash_core::LlmResponse {
                    parts: vec![lash_core::LlmOutputPart::Text {
                        text: "the root's answer".to_string(),
                        response_meta: None,
                    }],
                    ..lash_core::LlmResponse::default()
                })
            })
            .build();
        let mut host = lash_core::facade_support::RuntimeHostConfig::new(
            self.backend.clone(),
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        );
        if self.close_in_drive {
            host.control.scope_close = Arc::new(self.law_sink());
        }
        host.providers.provider_resolver = Arc::new(
            lash_core::facade_support::SingleProviderResolver::new(model.into_handle()),
        );
        let mut policy = lash_core::testing::mock_session_policy();
        policy.session_id = Some(self.session_id.clone());
        let state = lash_core::RuntimeSessionState {
            session_id: self.session_id.clone(),
            policy: policy.clone(),
            ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        };
        Box::pin(
            lash_core::facade_support::LashRuntime::builder(
                host,
                lash_core::testing::runtime_lease_owner(),
            )
            .with_session_id(&self.session_id)
            .with_policy(policy)
            .with_initial_state(state)
            .with_plugin_factories(lash_core::testing::test_standard_protocol_factories())
            .with_store(Arc::clone(&self.store))
            .with_queued_work(Arc::new(lash_core::NoSessionWork::new()))
            .with_process_work(
                self.backend
                    .process_work()
                    .expect("a Restate backend has process work"),
            )
            .build(),
        )
        .await
        .expect("build the law's runtime")
    }

    /// Accept one input the root `root` answers, then drive the session to
    /// a stop inside the harness's turn handler, and return the roots it ran.
    async fn drive_root(self: &Arc<Self>, root: &TurnId) -> Vec<RootOutcome> {
        let mut draft = lash_core::PendingTurnInputDraft::new(
            self.session_id.clone(),
            lash_core::TurnInputIngress::next_turn(),
            lash_core::TurnInput::text("the root's question"),
        );
        draft = draft.with_source_key(root.as_str());
        self.store
            .enqueue_pending_turn_input(draft)
            .await
            .expect("accept the root's input");
        let request = DriveRequest {
            session: self.session_id.clone(),
            request: DriveRequestId::new(format!("parent-end-drive-{}", self.nonce)),
            build_generation: lash_core::engine::BuildGeneration::for_test("parent-end-laws"),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let world = Arc::clone(self);
        self.harness
            .turn_runner()
            .run_turn(
                lash_core::AdmittedScope::turn(&self.session_id, TurnId::from("parent-end-driver")),
                Arc::new(move |scope| {
                    let world = Arc::clone(&world);
                    let request = request.clone();
                    let tx = tx.clone();
                    Box::pin(async move {
                        let mut runtime = world.runtime().await;
                        let outcome =
                            lash_core::drive::drive_session(&mut runtime, &scope, &request)
                                .await
                                .expect("the drive runs");
                        let _ = tx.send(outcome.ran);
                        lash_conformance::ConformanceTurnEnd::Settled
                    })
                }),
            )
            .await;
        let mut ran = None;
        while let Ok(outcome) = rx.try_recv() {
            ran = Some(outcome);
        }
        ran.expect("the harness ran the drive")
    }

    async fn record(&self, id: &ProcessId) -> lash_core::ProcessRecord {
        self.registry
            .get_process(id)
            .await
            .expect("read the child")
            .expect("the child is retained")
    }

    /// The invocations of `id`'s `cancel` handler on the server double.
    fn cancel_invocations(&self, id: &ProcessId) -> usize {
        let target = format!(
            "{}/{}/cancel",
            crate::LashService::ProcessWorkflow.name(),
            id
        );
        self.harness
            .server_double()
            .expect("the law runs on the server double")
            .invocations()
            .into_iter()
            .filter(|view| view.target == target)
            .count()
    }

    /// What `id`'s process workflow observed of a cancel: its cancel
    /// promise, which only the `cancel` handler resolves.
    async fn cancel_signal(&self, id: &ProcessId) -> crate::RestateProcessCancelSignal {
        crate::RestateIngressClient::new(self.harness.connection())
            .call_workflow_json::<_, crate::RestateProcessCancelSignal>(
                crate::LashService::ProcessWorkflow.name(),
                id.as_str(),
                "await_cancel",
                &crate::RestateProcessAwaitRequest {
                    process_id: id.clone(),
                },
            )
            .await
            .expect("the child's cancel promise resolved")
    }
}

impl World {
    /// The registry's scope-close sink with the endpoint's process port as
    /// its delivery: the body the drive's close step and the reconcile arm
    /// run.
    fn law_sink(&self) -> lash_core::RegistryScopeClose {
        lash_core::RegistryScopeClose::with_delivery(
            Arc::clone(&self.registry),
            Arc::clone(
                self.backend
                    .process_work()
                    .expect("a Restate backend has process work")
                    .port(),
            ),
            self.backend.clock(),
        )
    }

    /// The root-close step's call: close `root`'s scope after its terminal.
    async fn close_root(&self, root: &TurnId) {
        use lash_core::engine::ScopeCloseSink as _;
        let terminal = lash_core::store::RootTerminal {
            session_id: self.session_id.clone(),
            root: root.clone(),
            kind: lash_core::store::RootTerminalKind::Answered,
            cause: lash_core::store::RootTerminalCause::Committed {
                commit: lash_core::store::TurnCommitId::new(root.clone(), 0),
                turn: root.clone(),
                stop: None,
            },
            head_revision: None,
            at_ms: 1,
        };
        self.law_sink()
            .close_root_scope(&terminal)
            .await
            .expect("close the root's scope");
    }

    /// The root's plan, which no physical commit of a drive-run root writes.
    async fn plan(&self, parent: &ScopeId) -> Option<lash_core::ParentEndPlan> {
        self.registry
            .get_parent_end_plan(parent)
            .await
            .expect("read the plan")
    }
}

fn assert_parent_ended(record: &lash_core::ProcessRecord, parent: &ScopeId) {
    let request = record
        .cancel_request
        .as_deref()
        .unwrap_or_else(|| panic!("`{}` carries a cancel request", record.id));
    assert_eq!(request.origin, lash_core::CancelOrigin::ParentEnded);
    assert_eq!(
        request.requester,
        parent.storage_id(),
        "the requester names the ended scope"
    );
}

/// A root's end cancels its `Until` children on Restate: the root's close
/// records its plan, delivers `ParentEnded` to each child's process workflow
/// once, records the request, and settles the plan; a repeated close is a
/// no-op. None of the root's commits recorded its end before the close, and
/// a child of another root is untouched.
#[tokio::test]
async fn a_root_end_cancels_its_cancel_children_once_on_restate() {
    let world = Arc::new(World::start_with("root-cancel", |backend| backend).await);
    let root = world.root("root-cancel-root");
    let parent = ScopeId::turn(world.session_id.clone(), root.clone());
    let other = ScopeId::turn(world.session_id.clone(), world.root("root-cancel-other"));
    let first = world.register_until_child("root-cancel-a", &parent).await;
    let second = world.register_until_child("root-cancel-b", &parent).await;
    let bystander = world
        .register_until_child("root-cancel-other-child", &other)
        .await;

    let ran = world.drive_root(&root).await;
    assert!(
        matches!(ran.as_slice(), [RootOutcome::Committed { root: ran, .. }] if *ran == root),
        "the drive ran the root to its terminal: {ran:?}"
    );
    assert_eq!(
        world.plan(&parent).await,
        None,
        "no physical commit of a drive-run root records its end"
    );
    world.close_root(&root).await;
    world.close_root(&root).await;

    for child in [&first, &second] {
        assert_parent_ended(&world.record(child).await, &parent);
        assert_eq!(
            world.cancel_signal(child).await,
            crate::RestateProcessCancelSignal::CancelRequested,
            "`{child}`'s execution observes the cancel through its workflow"
        );
        assert_eq!(
            world.cancel_invocations(child),
            1,
            "`{child}`'s cancel is delivered once"
        );
    }
    let plan = world
        .registry
        .get_parent_end_plan(&parent)
        .await
        .expect("read the root's plan")
        .expect("the root's end recorded its plan");
    assert!(
        plan.settled_at_ms.is_some(),
        "the plan is settled: {plan:?}"
    );
    assert!(
        world.record(&bystander).await.cancel_request.is_none(),
        "another root's child is untouched"
    );
    assert_eq!(world.cancel_invocations(&bystander), 0);
    assert!(
        world
            .registry
            .list_pending_parent_end_plans(PAGE)
            .await
            .expect("list pending plans")
            .is_empty(),
        "no plan is left for a reconcile pass"
    );
    world.harness.finish().await;
}

/// A `Detached` child outlives its root's end: it stays live, and no cancel
/// is delivered to it or recorded for it.
#[tokio::test]
async fn a_detached_child_outlives_its_root_end_on_restate() {
    let world = Arc::new(World::start_with("root-abandon", |backend| backend).await);
    let root = world.root("root-abandon-root");
    let parent = ScopeId::turn(world.session_id.clone(), root.clone());
    let detached = world
        .register_detached_child("root-abandon-child", &parent)
        .await;

    let ran = world.drive_root(&root).await;
    assert!(
        matches!(ran.as_slice(), [RootOutcome::Committed { .. }]),
        "the drive ran the root to its terminal: {ran:?}"
    );
    world.close_root(&root).await;

    let record = world.record(&detached).await;
    assert!(
        record.cancel_request.is_none() && !record.is_terminal(),
        "the detached child is live and uncancelled: {record:?}"
    );
    assert_eq!(world.cancel_invocations(&detached), 0);
    let plan = world
        .registry
        .get_parent_end_plan(&parent)
        .await
        .expect("read the root's plan")
        .expect("the root's end recorded its plan");
    assert!(
        plan.settled_at_ms.is_some(),
        "the plan is settled: {plan:?}"
    );
    world.harness.finish().await;
}

/// A session's close ends every root it closed: each root's `Until`
/// children are cancelled as that root's own close would cancel them, and
/// its `Detached` children outlive the session's close.
#[tokio::test]
async fn a_session_close_ends_the_scopes_of_its_roots_on_restate() {
    use lash_core::engine::ScopeCloseSink as _;
    let world = Arc::new(World::start("session-close").await);
    let active = world.root("session-close-active");
    let parked = world.root("session-close-parked");
    let active_scope = ScopeId::turn(world.session_id.clone(), active.clone());
    let parked_scope = ScopeId::turn(world.session_id.clone(), parked.clone());
    let first = world
        .register_until_child("session-close-a", &active_scope)
        .await;
    let second = world
        .register_until_child("session-close-b", &parked_scope)
        .await;
    let detached = world
        .register_detached_child("session-close-detached", &active_scope)
        .await;

    world
        .law_sink()
        .close_session_scope(
            &world.session_id,
            lash_core::store::ControlIntentId::from_sequence(1),
            &[active.clone(), parked.clone()],
        )
        .await
        .expect("close the session's roots");

    assert_parent_ended(&world.record(&first).await, &active_scope);
    assert_parent_ended(&world.record(&second).await, &parked_scope);
    assert_eq!(world.cancel_invocations(&first), 1);
    assert_eq!(world.cancel_invocations(&second), 1);
    assert!(world.record(&detached).await.cancel_request.is_none());
    assert_eq!(world.cancel_invocations(&detached), 0);
    for scope in [&active_scope, &parked_scope] {
        assert!(
            world
                .plan(scope)
                .await
                .is_some_and(|plan| plan.settled_at_ms.is_some()),
            "each closed root's plan is settled"
        );
    }
    world.harness.finish().await;
}

/// A process's end cancels its `Until` children on Restate: its terminal
/// completion records the plan in the same transaction, and the workflow's
/// next journaled step applies it and settles it.
#[tokio::test]
async fn a_process_end_cancels_its_cancel_children_on_restate() {
    let world = Arc::new(World::start("process-cancel").await);
    let parent_key = lash_core::StartKey::for_host(
        lash_core::StartKeyOwner::HOST,
        format!("process-cancel-parent-{}", world.nonce),
    );
    let registration = lash_core::ProcessRegistration::new(
        lash_core::ProcessInput::External {
            metadata: serde_json::json!({ "law": "parent-end-process" }),
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_start_key(Some(parent_key.clone()));
    let parent_record = world
        .registry
        .register_process(registration.clone())
        .await
        .expect("register the parent process");
    let parent_id = parent_record.id.clone();
    let parent = ScopeId::process(parent_id.clone());
    let cancelled = world
        .register_until_child("process-cancel-child", &parent)
        .await;
    let detached = world
        .register_detached_child("process-cancel-detached", &parent)
        .await;

    world
        .harness
        .turn_runner()
        .serve_segments(
            &parent_key,
            Arc::new(|_scope| Box::pin(async { lash_conformance::ConformanceTurnEnd::Settled })),
        )
        .await;
    crate::RestateIngressClient::new(world.harness.connection())
        .call_workflow_json::<_, crate::RestateProcessWorkflowOutput>(
            crate::LashService::ProcessWorkflow.name(),
            &crate::process::process_segment_workflow_key(&parent_id, 0),
            "run",
            &crate::RestateProcessWorkflowInput {
                process_id: parent_id.clone(),
                registration,
                execution_context: lash_core::ProcessExecutionContext::default(),
                segment_ordinal: 0,
                sender_generation: None,
            },
        )
        .await
        .expect("the parent process runs to its terminal");

    assert!(world.record(&parent_id).await.is_terminal());
    assert_parent_ended(&world.record(&cancelled).await, &parent);
    assert!(
        world.record(&detached).await.cancel_request.is_none(),
        "the detached child outlives its parent process"
    );
    let plan = world
        .registry
        .get_parent_end_plan(&parent)
        .await
        .expect("read the process's plan")
        .expect("the terminal completion recorded the plan");
    assert!(
        plan.settled_at_ms.is_some(),
        "the plan is settled: {plan:?}"
    );
    world.harness.finish().await;
}

/// A process port whose first delivery reaches the engine and then fails,
/// the way a transport fault loses the reply to a send that landed.
struct LosesFirstReply {
    inner: Arc<dyn lash_core::ProcessWorkSubstrate>,
    faults: AtomicUsize,
}

#[async_trait::async_trait]
impl lash_core::ProcessWorkSubstrate for LosesFirstReply {
    async fn admit_pending_processes(
        &self,
        reason: &str,
    ) -> Result<lash_core::facade_support::ProcessAdmissionReport, lash_core::PluginError> {
        self.inner.admit_pending_processes(reason).await
    }

    async fn await_process_terminal(
        &self,
        process_id: &ProcessId,
    ) -> Result<lash_core::ProcessTerminalWait, lash_core::PluginError> {
        self.inner.await_process_terminal(process_id).await
    }

    async fn deliver_cancel(
        &self,
        process_id: &ProcessId,
        request: &lash_core::CancelRequest,
        delivery_key: &str,
    ) -> Result<(), lash_core::PluginError> {
        self.inner
            .deliver_cancel(process_id, request, delivery_key)
            .await?;
        if self.faults.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(lash_core::PluginError::Runtime(
                lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineProcessCancel,
                    "injected: the delivery's reply was lost",
                ),
            ));
        }
        Ok(())
    }
}

/// A delivery whose reply is lost fails the close's attempt, the engine
/// runs the close again, and the child's cancel still reaches the engine
/// once: a re-send names the first one's key.
#[tokio::test]
async fn a_lost_parent_end_delivery_is_retried_and_delivered_once() {
    let faults = Arc::new(std::sync::Mutex::new(None::<Arc<LosesFirstReply>>));
    let installed = Arc::clone(&faults);
    let world = Arc::new(
        World::start_with("root-retry", move |backend| {
            lash_core::testing::runtime_helpers::LayeredBackend::over(backend)
                .map_process_work_port(move |port| {
                    let port = Arc::new(LosesFirstReply {
                        inner: port,
                        faults: AtomicUsize::new(0),
                    });
                    *installed.lock().expect("fault slot") = Some(Arc::clone(&port));
                    port
                })
                .into_backend()
        })
        .await,
    );
    let root = world.root("root-retry-root");
    let parent = ScopeId::turn(world.session_id.clone(), root.clone());
    let child = world
        .register_until_child("root-retry-child", &parent)
        .await;

    let ran = world.drive_root(&root).await;
    assert!(
        matches!(ran.as_slice(), [RootOutcome::Committed { .. }]),
        "the drive ran the root to its terminal: {ran:?}"
    );
    // The close's first run fails after its send landed; the engine runs the
    // close again, as a failed recorded step is run again.
    use lash_core::engine::ScopeCloseSink as _;
    let terminal = lash_core::store::RootTerminal {
        session_id: world.session_id.clone(),
        root: root.clone(),
        kind: lash_core::store::RootTerminalKind::Answered,
        cause: lash_core::store::RootTerminalCause::Committed {
            commit: lash_core::store::TurnCommitId::new(root.clone(), 0),
            turn: root.clone(),
            stop: None,
        },
        head_revision: None,
        at_ms: 1,
    };
    let sink = world.law_sink();
    assert!(
        sink.close_root_scope(&terminal).await.is_err(),
        "the lost reply fails the close's first run"
    );
    assert!(
        world
            .plan(&parent)
            .await
            .is_some_and(|plan| plan.settled_at_ms.is_none()),
        "the failed run leaves the plan recorded and unsettled"
    );
    sink.close_root_scope(&terminal)
        .await
        .expect("the close's second run applies the plan");

    let port = faults
        .lock()
        .expect("fault slot")
        .clone()
        .expect("the layered port was installed");
    // The second run finds the child again and re-sends under the first
    // key, which the engine dedupes, or finds its request already recorded
    // by the handler the first send reached; either way the engine saw one
    // cancel.
    let deliveries = port.faults.load(Ordering::SeqCst);
    assert!(
        (1..=2).contains(&deliveries),
        "one lost delivery, then at most one re-send: {deliveries}"
    );
    assert_parent_ended(&world.record(&child).await, &parent);
    assert_eq!(
        world.cancel_invocations(&child),
        1,
        "the retried delivery names the first one"
    );
    let plan = world
        .registry
        .get_parent_end_plan(&parent)
        .await
        .expect("read the root's plan")
        .expect("the root's end recorded its plan");
    assert!(
        plan.settled_at_ms.is_some(),
        "the plan is settled: {plan:?}"
    );
    world.harness.finish().await;
}

/// A plan recorded by an ending whose execution died before applying it is
/// applied exactly once by the engine-neutral reconcile tick's parent-end
/// arm, and the next tick finds nothing to do.
#[tokio::test]
async fn an_unapplied_plan_is_applied_once_by_reconcile() {
    let world = Arc::new(World::start("reconcile").await);
    let parent = ScopeId::turn(world.session_id.clone(), world.root("reconcile-root"));
    let child = world.register_until_child("reconcile-child", &parent).await;
    let detached = world
        .register_detached_child("reconcile-detached", &parent)
        .await;
    // The ending's execution recorded the plan and died before applying it.
    world
        .registry
        .record_parent_end(&parent)
        .await
        .expect("record the plan");
    assert!(
        world.record(&child).await.cancel_request.is_none(),
        "nothing applied the plan yet"
    );

    let wiring = world
        .backend
        .process_work()
        .expect("a Restate backend has process work");
    let sessions = world.backend.session_store_factory();
    let clock = world.backend.clock();
    let work = lash_core::NoSessionWork::new();
    let scopes = lash_core::engine::NoScopeClose;
    let drain = world.backend.generation_drain();
    let generation = world.backend.build_generation().clone();
    let parts = lash_core::drive::ReconcileParts {
        sessions: sessions.as_ref(),
        work: &work,
        scopes: &scopes,
        processes: Some(lash_core::drive::ReconcileProcesses {
            registry: world.registry.as_ref(),
            port: wiring.port().as_ref(),
            drain: drain.as_ref(),
            generation: &generation,
        }),
        clock: clock.as_ref(),
        duties: lash_core::runtime::recovery_lease::RecoveryDuties::ALL,
        relays: &[],
    };
    let first = lash_core::drive::reconcile_once(
        &parts,
        &lash_core::engine::ReconcileCursor::default(),
        PAGE,
        "parent-end-tick-1",
    )
    .await;
    assert_eq!(
        (
            first.parent_end_plans.handled,
            first.parent_end_plans.deferred
        ),
        (1, 0),
        "the tick applied the unapplied plan: {:?}",
        first.failures
    );
    let second =
        lash_core::drive::reconcile_once(&parts, &first.next, PAGE, "parent-end-tick-2").await;
    assert_eq!(
        (
            second.parent_end_plans.handled,
            second.parent_end_plans.deferred
        ),
        (0, 0),
        "nothing is left for the next tick"
    );

    assert_parent_ended(&world.record(&child).await, &parent);
    assert_eq!(world.cancel_invocations(&child), 1, "delivered once");
    assert!(world.record(&detached).await.cancel_request.is_none());
    assert!(
        world
            .plan(&parent)
            .await
            .is_some_and(|plan| plan.settled_at_ms.is_some()),
        "the plan is settled"
    );
    world.harness.finish().await;
}

/// End to end on the drive path: the root's own recorded close — the
/// `CloseRootScope` step after its terminal evidence, with the registry's
/// delivering sink installed — records and applies its plan with no
/// law-side sink call.
#[tokio::test]
async fn a_root_end_on_the_drive_path_cancels_its_cancel_children() {
    let world = Arc::new(World::start("drive-path").await);
    let root = world.root("drive-path-root");
    let parent = ScopeId::turn(world.session_id.clone(), root.clone());
    let child = world
        .register_until_child("drive-path-child", &parent)
        .await;

    let ran = world.drive_root(&root).await;
    assert!(
        matches!(ran.as_slice(), [RootOutcome::Committed { .. }]),
        "the drive ran the root to its terminal: {ran:?}"
    );

    assert_parent_ended(&world.record(&child).await, &parent);
    assert_eq!(world.cancel_invocations(&child), 1);
    assert!(
        world
            .plan(&parent)
            .await
            .is_some_and(|plan| plan.settled_at_ms.is_some()),
        "the root's close settled its plan"
    );
    world.harness.finish().await;
}
