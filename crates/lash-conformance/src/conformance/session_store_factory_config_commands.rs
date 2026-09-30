//! Durable session-config command conformance.

use super::*;
use crate::Clock;
use lash_core::plugin::PluginSessionRequest;
use pretty_assertions::assert_eq;
use std::future::Future;

struct PausedConfigSettlementStore {
    inner: Arc<dyn crate::RuntimeStore>,
    pause_after_enqueue: std::sync::atomic::AtomicBool,
    before_settlement_read: tokio::sync::Notify,
    release_settlement_read: tokio::sync::Notify,
}

impl PausedConfigSettlementStore {
    fn new(inner: Arc<dyn crate::RuntimeStore>) -> Self {
        Self {
            inner,
            pause_after_enqueue: std::sync::atomic::AtomicBool::new(false),
            before_settlement_read: tokio::sync::Notify::new(),
            release_settlement_read: tokio::sync::Notify::new(),
        }
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for PausedConfigSettlementStore {
    type Inner = dyn crate::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn enqueue_queued_work(
        &self,
        draft: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkBatch, crate::StoreError> {
        let batch = self.inner.enqueue_queued_work(draft).await?;
        if batch.is_session_command_work() {
            self.pause_after_enqueue
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(batch)
    }

    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, crate::StoreError> {
        if self
            .pause_after_enqueue
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.before_settlement_read.notify_one();
            self.release_settlement_read.notified().await;
        }
        self.inner.list_queued_work(session_id).await
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn ingress_follow_on_fork_and_command_coalescing_matrix(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let request = session_store_request(
        &SessionId::from("config-command-coalescing"),
        "config-command-base-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .admit_view(&request)
        .await
        .expect("create config-command conformance store");
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    let owed = crate::store::PendingFollowOn {
        follow_on_turn_id: TurnId::from("matrix-switch:agent-frame:1"),
        frame_id: state
            .current_frame_node_id
            .clone()
            .expect("initialized frame"),
        task: "follow-on matrix task".to_string(),
        options: None,
        resolved_run: None,
        chain_depth: 3,
        attempts: 2,
    };
    let mut switch = crate::RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::turn(&request.session_id, "matrix-switch", "final"),
    );
    switch.pending_follow_on = Some(owed.clone());
    state.apply_persisted_commit_result(
        store
            .commit_runtime_state(switch.clone())
            .await
            .expect("switch writes follow-on"),
    );
    state.mark_node_ids_persisted(switch.graph.nodes().iter().map(|node| node.node_id.clone()));
    let replay = store
        .commit_runtime_state(switch)
        .await
        .expect("switch receipt replays");
    assert_eq!(replay.pending_follow_on, Some(owed.clone()));
    let input = store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            &request.session_id,
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("queued behind follow-on"),
        ))
        .await
        .expect("input behind follow-on");
    for model in ["config-a", "config-b", "config-c"] {
        store
            .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
                &request.session_id,
                crate::DeliveryPolicy::AfterCurrentTurnCommit,
                crate::SessionCommand::ApplyConfigPatch {
                    patch: Box::new(crate::runtime::ApplyConfigPatch {
                        model: Some(
                            crate::ModelSpec::builder(model)
                                .context_window_tokens(32_000)
                                .build()
                                .expect("model"),
                        ),
                        ..crate::runtime::ApplyConfigPatch::default()
                    }),
                },
            ))
            .await
            .expect("enqueue config command");
    }
    let separator = store
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            &request.session_id,
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::SessionCommand::RefreshToolCatalog {
                reason: "separator".to_string(),
            },
        ))
        .await
        .expect("enqueue separator");
    let last = store
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            &request.session_id,
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::SessionCommand::ApplyConfigPatch {
                patch: Box::new(crate::runtime::ApplyConfigPatch::default()),
            },
        ))
        .await
        .expect("enqueue later patch");
    let node = store
        .load_session_head_meta()
        .await
        .expect("switch head")
        .expect("head")
        .leaf_node_id
        .expect("switch leaf");
    let fork_id = SessionId::from("follow-on-matrix-fork");
    factory
        .fork_session(&crate::ForkSessionRequest {
            session_id: fork_id.clone(),
            node_id: node,
            pending_observer_intents: Vec::new(),
            relation: crate::SessionRelation::Root,
            policy: request.config.session_policy(),
        })
        .await
        .expect("fork switched head");
    let fork = factory
        .live_view(&fork_id)
        .await
        .expect("lookup fork")
        .expect("fork store");
    assert!(
        fork.load_session_head_meta()
            .await
            .expect("fork head")
            .expect("head")
            .pending_follow_on
            .is_none(),
        "fork owes no source follow-on"
    );
    assert_eq!(
        store
            .load_session_head_meta()
            .await
            .expect("source head")
            .expect("head")
            .pending_follow_on,
        Some(owed.clone())
    );
    assert!(
        fork.list_pending_turn_inputs()
            .await
            .expect("fork input disposition")
            .is_empty()
    );
    assert!(
        fork.list_queued_work()
            .await
            .expect("fork command disposition")
            .is_empty()
    );
    let owner = crate::LeaseOwnerIdentity::opaque(
        "config-command-coalescing",
        "config-command-coalescing:incarnation",
    );
    let lease = store
        .store()
        .seal_drive_epoch_for_test(
            &request.session_id,
            &owner,
            "config-command-coalescing-executor",
            60_000,
        )
        .await
        .expect("claim config-command session lease")
        .acquired()
        .expect("config-command session lease");
    assert!(
        store
            .open_session_command_run(&lease)
            .await
            .expect("blocked commands")
            .is_empty()
    );
    let admitted = crate::testing::store_fixtures::admit_root_for_test(
        store.store(),
        &lease,
        &TurnId::from("blocked-root"),
        crate::store::AdmittedHead::Input(input.input_id.clone()),
    )
    .await
    .expect("follow-on blocks idle input");
    assert!(admitted.is_none());
    let mut unrelated = crate::RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::turn(&request.session_id, "unrelated", "final"),
    );
    unrelated.pending_follow_on = None;
    let refusal = store
        .commit_runtime_state(unrelated)
        .await
        .expect_err("unrelated commit cannot clear a follow-on");
    assert!(
        matches!(
            refusal,
            crate::StoreError::FollowOnPending { attempts: 2, .. }
        ),
        "{refusal:?}"
    );
    let mut terminal = crate::RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::turn(&request.session_id, &owed.follow_on_turn_id, "final"),
    );
    terminal.pending_follow_on = None;
    store
        .commit_runtime_state(terminal)
        .await
        .expect("follow-on terminal clears obligation");
    assert!(
        store
            .load_session_head_meta()
            .await
            .expect("cleared head")
            .expect("head")
            .pending_follow_on
            .is_none()
    );
    let before_revision = store
        .load_session_head_meta()
        .await
        .expect("head")
        .expect("head")
        .head_revision;
    let run = store
        .open_session_command_run(&lease)
        .await
        .expect("open leading config commands");

    assert_eq!(run.len(), 3);
    assert_eq!(
        crate::AdmittedQueuedWork {
            session_id: request.session_id.clone(),
            batches: run.clone(),
        }
        .session_commands()
        .expect("the run contains only config commands")
        .len(),
        3,
        "all adjacent config commands must share one command run"
    );
    let completed_batch_ids = run
        .iter()
        .map(|batch| batch.batch_id.clone())
        .collect::<Vec<_>>();
    commit_session_command_run(store.store(), &request, &lease, run).await;
    assert_eq!(
        store
            .load_session_head_meta()
            .await
            .expect("coalesced head")
            .expect("head")
            .head_revision,
        before_revision + 1
    );
    let next = store
        .open_session_command_run(&lease)
        .await
        .expect("separator run");
    assert_eq!(
        next.iter().map(|batch| &batch.batch_id).collect::<Vec<_>>(),
        vec![&separator.batch_id]
    );
    commit_session_command_run(store.store(), &request, &lease, next).await;
    let next = store
        .open_session_command_run(&lease)
        .await
        .expect("later patch run");
    assert_eq!(
        next.iter().map(|batch| &batch.batch_id).collect::<Vec<_>>(),
        vec![&last.batch_id]
    );
    commit_session_command_run(store.store(), &request, &lease, next).await;
    assert!(
        store
            .open_session_command_run(&lease)
            .await
            .expect("empty command lane")
            .is_empty()
    );
    assert_eq!(
        store
            .list_pending_turn_inputs()
            .await
            .expect("input retained")[0]
            .input
            .input_id,
        input.input_id
    );
    for batch_id in completed_batch_ids {
        assert!(
            store
                .queued_work_batch_completed(&batch_id)
                .await
                .expect("read config-command completion marker"),
            "every batch in a coalesced command commit must leave completion evidence"
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn session_store_factory_bounds_config_command_runs(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let request = session_store_request(
        &SessionId::from("config-command-run-bound"),
        "config-command-base-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .admit_view(&request)
        .await
        .expect("create bounded config-command store");
    let total = crate::store::queued_work::MAX_SESSION_COMMAND_BATCHES_PER_RUN + 3;
    for index in 0..total {
        store
            .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
                &request.session_id,
                crate::DeliveryPolicy::AfterCurrentTurnCommit,
                crate::SessionCommand::ApplyConfigPatch {
                    patch: Box::new(crate::runtime::ApplyConfigPatch {
                        model: Some(
                            crate::ModelSpec::builder(format!("bounded-config-{index}"))
                                .context_window_tokens(32_000)
                                .build()
                                .expect("model"),
                        ),
                        ..crate::runtime::ApplyConfigPatch::default()
                    }),
                },
            ))
            .await
            .expect("enqueue bounded config command");
    }
    let owner = crate::LeaseOwnerIdentity::opaque(
        "config-command-run-bound",
        "config-command-run-bound:incarnation",
    );
    let lease = store
        .store()
        .seal_drive_epoch_for_test(
            &request.session_id,
            &owner,
            "config-command-run-bound-executor",
            60_000,
        )
        .await
        .expect("claim bounded config-command session lease")
        .acquired()
        .expect("bounded config-command session lease");
    let first = store
        .open_session_command_run(&lease)
        .await
        .expect("open first bounded command prefix");
    assert_eq!(
        first.len(),
        crate::store::queued_work::MAX_SESSION_COMMAND_BATCHES_PER_RUN
    );
    commit_session_command_run(store.store(), &request, &lease, first).await;

    let second = store
        .open_session_command_run(&lease)
        .await
        .expect("open remaining bounded command prefix");
    assert_eq!(second.len(), 3);
    commit_session_command_run(store.store(), &request, &lease, second).await;

    assert!(
        store
            .open_session_command_run(&lease)
            .await
            .expect("check bounded command queue exhaustion")
            .is_empty(),
        "a longer config-command run must drain completely over multiple commits"
    );
}

async fn commit_session_command_run(
    store: &Arc<dyn crate::RuntimeStore>,
    request: &crate::SessionStoreCreateRequest,
    fence: &crate::store::DriveFence,
    run: Vec<crate::QueuedWorkBatch>,
) {
    commit_session_command_run_with(store, request, fence, run, |_| {}).await;
}

/// [`commit_session_command_run`], with `adjust` applied to the state the
/// settling commit writes.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn commit_session_command_run_with(
    store: &Arc<dyn crate::RuntimeStore>,
    request: &crate::SessionStoreCreateRequest,
    fence: &crate::store::DriveFence,
    run: Vec<crate::QueuedWorkBatch>,
    adjust: impl FnOnce(&mut crate::RuntimeSessionState),
) {
    let mut state = crate::conformance::helpers::load_window_state(store, &request.session_id)
        .await
        .expect("load config-command state")
        .unwrap_or_else(|| crate::RuntimeSessionState {
            session_id: request.session_id.clone(),
            policy: request.config.session_policy(),
            ..crate::RuntimeSessionState::new(request.config.session_policy())
        });
    state.ensure_agent_frame_initialized();
    adjust(&mut state);
    let first_batch_id = run
        .first()
        .expect("the command run has a batch")
        .batch_id
        .clone();
    let mut commit = crate::RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::OperationId::new(
            crate::ExecutionScope::queue_drain(&request.session_id, first_batch_id),
            "session-command",
        ),
    );
    commit.drive_fence = Some(Box::new(fence.clone()));
    commit.applied_commands = Some(crate::QueuedWorkCompletion {
        session_id: request.session_id.clone(),
        batch_ids: run.iter().map(|batch| batch.batch_id.clone()).collect(),
    });
    store
        .commit_runtime_state(commit)
        .await
        .expect("commit config-command run");
}

tokio::task_local! {
    /// Marks the facade writer's task: only its sleeps move the virtual clock.
    static DRIVES_SETTLEMENT_CLOCK: ();
}

/// Virtual clock for runtime settlement tests.
///
/// Only the facade writer drives it: a sleep inside [`Self::driving`] advances
/// the clock by its duration and yields once, so the settlement deadline is
/// exercised without waiting on wall time. Every other sleeper (the session
/// lease renewer the settlement loop spawns, for one) waits for the writer to
/// move the clock past its deadline. A background sleeper therefore never
/// pushes the writer's deadline forward, and the writer answers at exactly
/// the bound.
#[derive(Debug)]
struct ConfigSettlementClock {
    epoch_ms: std::sync::atomic::AtomicU64,
    monotonic_origin: std::time::Instant,
    epoch_origin_ms: u64,
    advanced: tokio::sync::Notify,
}

impl ConfigSettlementClock {
    fn new(epoch_ms: u64) -> Self {
        Self {
            epoch_ms: std::sync::atomic::AtomicU64::new(epoch_ms),
            monotonic_origin: std::time::Instant::now(),
            epoch_origin_ms: epoch_ms,
            advanced: tokio::sync::Notify::new(),
        }
    }

    /// Run `writer` as the task whose sleeps drive the clock.
    async fn driving<T>(writer: impl Future<Output = T>) -> T {
        DRIVES_SETTLEMENT_CLOCK.scope((), writer).await
    }

    fn duration_ms(duration: std::time::Duration) -> u64 {
        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
    }

    fn advance(&self, duration: std::time::Duration) {
        self.epoch_ms.fetch_add(
            Self::duration_ms(duration),
            std::sync::atomic::Ordering::SeqCst,
        );
        self.advanced.notify_waiters();
    }

    async fn wait_until_ms(&self, deadline_ms: u64) {
        loop {
            let advanced = self.advanced.notified();
            tokio::pin!(advanced);
            advanced.as_mut().enable();
            if self.epoch_ms.load(std::sync::atomic::Ordering::SeqCst) >= deadline_ms {
                return;
            }
            advanced.await;
        }
    }
}

#[async_trait::async_trait]
impl crate::Clock for ConfigSettlementClock {
    fn now(&self) -> std::time::Instant {
        self.monotonic_origin
            + std::time::Duration::from_millis(
                self.epoch_ms
                    .load(std::sync::atomic::Ordering::SeqCst)
                    .saturating_sub(self.epoch_origin_ms),
            )
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = self.epoch_ms.load(std::sync::atomic::Ordering::SeqCst);
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        if DRIVES_SETTLEMENT_CLOCK.try_with(|()| ()).is_ok() {
            self.advance(duration);
            tokio::task::yield_now().await;
            return;
        }
        let deadline_ms = self
            .epoch_ms
            .load(std::sync::atomic::Ordering::SeqCst)
            .saturating_add(Self::duration_ms(duration));
        self.wait_until_ms(deadline_ms).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        self.sleep(deadline.saturating_duration_since(self.now()))
            .await;
    }
}

#[test]
fn config_settlement_clock_wall_clock_faces_agree() {
    let clock = ConfigSettlementClock::new(1_700_000_000_123);
    let clock: &dyn crate::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
}

/// The law's store: a fresh backend, and `request`'s store from its
/// factory. The backend is returned so the runtime takes its ports from it.
///
/// The backend keeps its own clock. Only the runtime's settlement wait runs
/// on the law's virtual clock: a backend on it would advance that clock with
/// every lease-renewal sleep of its own and blur the bound the law measures.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn config_settlement_store<M, Fut>(
    make: &M,
    request: &crate::SessionStoreCreateRequest,
) -> (crate::Backend, Arc<dyn crate::RuntimeStore>)
where
    M: Fn() -> Fut,
    Fut: Future<Output = crate::Backend>,
{
    let backend = make().await;
    let store = backend
        .session_store_factory()
        .admit_view(request)
        .await
        .expect("create config-settlement store");
    (backend, Arc::clone(store.store()))
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn runtime_for_config_settlement(
    backend: crate::Backend,
    store: Arc<dyn crate::RuntimeStore>,
    request: &crate::SessionStoreCreateRequest,
    clock: Arc<ConfigSettlementClock>,
) -> crate::LashRuntime {
    let mut state = crate::conformance::helpers::load_window_state(&store, &request.session_id)
        .await
        .expect("load config-settlement state")
        .unwrap_or_else(|| crate::RuntimeSessionState {
            session_id: request.session_id.clone(),
            policy: request.config.session_policy(),
            ..crate::RuntimeSessionState::new(request.config.session_policy())
        });
    state.ensure_agent_frame_initialized();
    let host = crate::PluginHost::new(crate::testing::test_standard_protocol_factories());
    let plugins = match state.plugin_state() {
        Some(snapshot) => host.build_session(PluginSessionRequest::rematerialization(
            request.session_id.clone(),
            snapshot,
            crate::plugin::RecordedSessionConfig::new(state.protocol_turn_options.clone()),
        )),
        None => host.build_session(PluginSessionRequest::creation(
            request.session_id.clone(),
            Default::default(),
        )),
    }
    .expect("config-settlement plugins");
    let mut host = crate::RuntimeHostConfig::new(
        backend,
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(clock as Arc<dyn crate::Clock>);
    // The laws patch the route's model: S6 validates the route at acceptance,
    // so the host must serve the request's `conformance-provider`. The model
    // still never settles — the queued blocker holds the FIFO head.
    host.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        crate::testing::TestProvider::builder()
            .kind("conformance-provider")
            .build()
            .into_handle(),
    ));
    let runtime_host = crate::EmbeddedRuntimeHost::new(host);
    let runtime_services = crate::PersistentRuntimeServices::new(
        plugins,
        crate::conformance::helpers::session_view(&store, request.session_id.clone()),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    crate::LashRuntime::from_persistent_embedded_state(
        request.config.session_policy(),
        runtime_host,
        runtime_services,
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("build config-settlement runtime")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn hold_config_settlement_lease(store: &dyn crate::RuntimeStore, session_id: &SessionId) {
    let owner = crate::LeaseOwnerIdentity::opaque("config-blocker", "config-blocker:incarnation");
    store
        .seal_drive_epoch_for_test(session_id, &owner, "config-blocker", 600_000)
        .await
        .expect("claim the competing writer lease")
        .acquired()
        .expect("competing writer lease");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn config_settlement_patch(model_id: &str) -> crate::SessionConfigPatch {
    crate::SessionConfigPatch {
        model: Some(
            crate::ModelSpec::builder(model_id)
                .context_window_tokens(32_000)
                .build()
                .expect("config-settlement model"),
        ),
        ..crate::SessionConfigPatch::default()
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_config_settlement_pending_returns_without_wait<M, Fut>(make: M)
where
    M: Fn() -> Fut,
    Fut: Future<Output = crate::Backend>,
{
    let clock = Arc::new(ConfigSettlementClock::new(1_800_000_000_000));
    let request = session_store_request(
        &SessionId::from("config-settlement-timeout"),
        "config-settlement-original",
        crate::SessionRelation::Root,
    );
    let (backend, store) = config_settlement_store(&make, &request).await;
    hold_config_settlement_lease(store.as_ref(), &request.session_id).await;
    let mut runtime = runtime_for_config_settlement(
        backend.clone(),
        Arc::clone(&store),
        &request,
        Arc::clone(&clock),
    )
    .await;
    let original_model = runtime.export_persistence_state().policy.model.clone();
    let started = clock.now();
    let error = ConfigSettlementClock::driving(
        runtime.update_session_config(config_settlement_patch("must-remain-pending")),
    )
    .await
    .expect_err("blocked config setter must return a typed pending error");
    assert!(
        matches!(error, crate::SessionError::SessionCommandPending(_)),
        "blocked config setter returned {error:?}"
    );
    assert_eq!(
        clock.now().saturating_duration_since(started),
        std::time::Duration::ZERO,
        "the setter returns the pending receipt without driving the command"
    );
    assert_eq!(
        runtime.export_persistence_state().policy.model,
        original_model
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn cancelled_session_config_settlement_is_typed<M, Fut>(make: M)
where
    M: Fn() -> Fut,
    Fut: Future<Output = crate::Backend>,
{
    let clock = Arc::new(ConfigSettlementClock::new(1_800_000_000_000));
    let request = session_store_request(
        &SessionId::from("config-settlement-cancelled"),
        "config-settlement-original",
        crate::SessionRelation::Root,
    );
    let (backend, store) = config_settlement_store(&make, &request).await;
    hold_config_settlement_lease(store.as_ref(), &request.session_id).await;
    let paused_store = Arc::new(PausedConfigSettlementStore::new(Arc::clone(&store)));
    let runtime = runtime_for_config_settlement(
        backend.clone(),
        Arc::clone(&paused_store) as Arc<dyn crate::RuntimeStore>,
        &request,
        Arc::clone(&clock),
    )
    .await;
    let original_model = runtime.export_persistence_state().policy.model.clone();
    let setter = crate::task::spawn(ConfigSettlementClock::driving(async move {
        let mut runtime = runtime;
        let result = runtime
            .update_session_config(config_settlement_patch("must-be-cancelled"))
            .await;
        (result, runtime)
    }));

    // Hold the setter before its settlement read. Cancellation must commit
    // before the read, so the batch state has one unambiguous ordering.
    paused_store.before_settlement_read.notified().await;
    let command_batch = store
        .list_queued_work(&request.session_id)
        .await
        .expect("list queued config command")
        .into_iter()
        .find(crate::QueuedWorkBatch::is_session_command_work)
        .expect("config setter enqueued its command");
    let cancelled = store
        .cancel_queued_work_batch(&request.session_id, &command_batch.batch_id)
        .await
        .expect("cancel queued config command")
        .expect("config command cancellation wins before admission");
    assert_eq!(cancelled.batch_id, command_batch.batch_id);
    assert!(
        !store
            .queued_work_batch_completed(&request.session_id, &command_batch.batch_id)
            .await
            .expect("read cancelled config marker"),
        "cancellation must not manufacture completion evidence"
    );

    paused_store.release_settlement_read.notify_one();

    let (result, runtime) = setter.await.expect("cancelled setter task");
    let error = result.expect_err("cancelled config setter must be typed");
    assert!(
        matches!(
            &error,
            crate::SessionError::SessionCommandCancelled(receipt)
                if receipt.batch_id == command_batch.batch_id
        ),
        "cancelled config setter returned {error:?}"
    );
    assert_eq!(
        runtime.export_persistence_state().policy.model,
        original_model
    );
}

/// Head-authoritative settlement (FIG-1875): when another writer supersedes a
/// settled config command before the facade writer observes settlement, the
/// facade writer adopts the newer durable head as-is — it never re-publishes
/// its own older patch values over that head residently.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn superseded_config_settlement_adopts_the_newer_head<M, Fut>(make: M)
where
    M: Fn() -> Fut,
    Fut: Future<Output = crate::Backend>,
{
    let clock = Arc::new(ConfigSettlementClock::new(1_800_000_000_000));
    let request = session_store_request(
        &SessionId::from("config-settlement-superseded"),
        "config-settlement-original",
        crate::SessionRelation::Root,
    );
    let (backend, store) = config_settlement_store(&make, &request).await;
    let runtime = runtime_for_config_settlement(
        backend.clone(),
        Arc::clone(&store),
        &request,
        Arc::clone(&clock),
    )
    .await;

    // A second writer holds the session-execution lease for the whole test,
    // so the facade writer can neither drain inline nor drain from its
    // settlement wait loop.
    let owner =
        crate::LeaseOwnerIdentity::opaque("superseding-writer", "superseding-writer:incarnation");
    let lease = store
        .seal_drive_epoch_for_test(&request.session_id, &owner, "superseding-executor", 600_000)
        .await
        .expect("claim superseding session lease")
        .acquired()
        .expect("superseding session lease");

    let setter = crate::task::spawn(ConfigSettlementClock::driving(async move {
        let mut runtime = runtime;
        let result = runtime
            .update_session_config(config_settlement_patch("first-settled"))
            .await;
        (result, runtime)
    }));

    let command_batch = loop {
        if let Some(batch) = store
            .list_queued_work(&request.session_id)
            .await
            .expect("list queued config command")
            .into_iter()
            .find(crate::QueuedWorkBatch::is_session_command_work)
        {
            break batch;
        }
        tokio::task::yield_now().await;
    };

    // The second writer drains the facade writer's command...
    let run = store
        .open_session_command_run(&lease)
        .await
        .expect("open facade config command");
    assert!(
        run.iter()
            .any(|batch| batch.batch_id == command_batch.batch_id),
        "the superseding writer drains the facade writer's command"
    );
    // ...and, in the same commit that settles it, advances the durable head
    // with a newer model. One commit keeps the law independent of when the
    // facade writer polls: it can only ever observe its command settled under
    // the newer head.
    let superseding_model = crate::ModelSpec::builder("second-newer")
        .context_window_tokens(32_000)
        .build()
        .expect("superseding model");
    let newer_model = superseding_model.clone();
    commit_session_command_run_with(&store, &request, &lease, run, move |state| {
        state.policy.model = newer_model;
    })
    .await;

    let (result, mut runtime) = setter.await.expect("superseded setter task");
    match result {
        Ok(()) => {}
        Err(crate::SessionError::SessionCommandPending(receipt)) => {
            assert!(matches!(
                runtime
                    .settle_session_command(receipt)
                    .await
                    .expect("read superseded command settlement"),
                crate::runtime::SessionCommandSettlement::Durable(_)
            ));
        }
        Err(error) => panic!("superseded config setter: {error}"),
    }
    assert_eq!(
        runtime.export_persistence_state().policy.model,
        superseding_model,
        "settlement adopts the newer durable head instead of re-publishing \
         the settled command's older values"
    );
}
