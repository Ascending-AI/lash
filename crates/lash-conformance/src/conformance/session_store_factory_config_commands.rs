//! Durable session-config command conformance.

use super::*;
use crate::Clock;
use lash_core::plugin::PluginSessionRequest;
use lash_core::testing::{Script, StoreOp};
use pretty_assertions::assert_eq;
use std::future::Future;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn ingress_follow_on_fork_and_command_run_matrix(
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
        continuation: None,
        follow_on_turn_id: TurnId::from("matrix-switch:agent-frame:1"),
        frame_id: state
            .current_frame_node_id
            .clone()
            .expect("initialized frame"),
        task: "follow-on matrix task".to_string(),
        resolved_run: crate::conformance::helpers::default_resolved_run(),
        chain_depth: 3,
        attempts: 2,
    };
    let mut switch = crate::RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
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
    let mut transactions = Vec::new();
    for model in ["config-a", "config-b", "config-c"] {
        transactions.push(
            store
                .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
                    &request.session_id,
                    crate::DeliveryPolicy::AfterCurrentTurnCommit,
                    config_transaction_command(model),
                ))
                .await
                .expect("enqueue config command"),
        );
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
            config_transaction_command("config-later"),
        ))
        .await
        .expect("enqueue later transaction");
    let switched = store
        .load_session_head_meta()
        .await
        .expect("switch head")
        .expect("head");
    let fork_id = SessionId::from("follow-on-matrix-fork");
    factory
        .fork_session(&crate::ForkSessionRequest {
            session_id: fork_id.clone(),
            source_session_id: request.session_id.clone(),
            head_revision: switched.head_revision,
            pending_observer_intents: Vec::new(),
            relation: crate::SessionRelation::Root,
            config: request.config.session_policy().into(),
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
        crate::OperationId::turn(&request.session_id, &owed.follow_on_turn_id, "final"),
    );
    terminal.pending_follow_on = None;
    // The follow-on's terminal is its drive's commit: it presents the drive's
    // fence, as every head write while commands are open must (FIG-4202).
    terminal.drive_fence = Some(Box::new(lease.clone()));
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
    // Every session command is a run of one: each config transaction
    // applies alone, in its own settling commit (FIG-4379).
    let mut completed_batch_ids = Vec::new();
    for (ordinal, transaction) in transactions.iter().enumerate() {
        let run = store
            .open_session_command_run(&lease)
            .await
            .expect("open the leading config command");
        assert_eq!(
            run.iter().map(|batch| &batch.batch_id).collect::<Vec<_>>(),
            vec![&transaction.batch_id],
            "config command {ordinal} is a run of its own"
        );
        completed_batch_ids.push(transaction.batch_id.clone());
        commit_session_command_run(store.store(), &request, &lease, run).await;
    }
    assert_eq!(
        store
            .load_session_head_meta()
            .await
            .expect("settled head")
            .expect("head")
            .head_revision,
        before_revision + 3,
        "each config command settles in its own commit"
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
        .expect("later transaction run");
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
                .queued_work_batch_completion(&batch_id)
                .await
                .expect("read config-command completion marker")
                .is_some(),
            "every settled config command must leave completion evidence"
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn session_store_factory_runs_every_config_command_alone(
    factory: Arc<dyn crate::DeploymentStore>,
) {
    let request = session_store_request(
        &SessionId::from("config-command-run-alone"),
        "config-command-base-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .admit_view(&request)
        .await
        .expect("create config-command store");
    let mut enqueued = Vec::new();
    for index in 0..3 {
        enqueued.push(
            store
                .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
                    &request.session_id,
                    crate::DeliveryPolicy::AfterCurrentTurnCommit,
                    config_transaction_command(&format!("alone-config-{index}")),
                ))
                .await
                .expect("enqueue config command"),
        );
    }
    let owner = crate::LeaseOwnerIdentity::opaque(
        "config-command-run-alone",
        "config-command-run-alone:incarnation",
    );
    let lease = store
        .store()
        .seal_drive_epoch_for_test(
            &request.session_id,
            &owner,
            "config-command-run-alone-executor",
            60_000,
        )
        .await
        .expect("claim config-command session lease")
        .acquired()
        .expect("config-command session lease");
    for batch in &enqueued {
        let run = store
            .open_session_command_run(&lease)
            .await
            .expect("open the leading config command");
        assert_eq!(
            run.iter().map(|open| &open.batch_id).collect::<Vec<_>>(),
            vec![&batch.batch_id],
            "a config command is a run of one"
        );
        commit_session_command_run(store.store(), &request, &lease, run).await;
    }
    assert!(
        store
            .open_session_command_run(&lease)
            .await
            .expect("check the command lane")
            .is_empty(),
        "every config command drained, one commit each"
    );
}

/// A config transaction setting `model` through the core owner, as ingress
/// records it.
fn config_transaction_command(model: &str) -> crate::SessionCommand {
    crate::SessionCommand::ApplyConfigTransaction {
        transaction: Box::new(crate::ConfigTransactionRecord {
            id: format!("config-command:{model}"),
            expected_revision: 0,
            entries: vec![crate::ConfigCommandEntry {
                owner: crate::CORE_CONFIG_OWNER.to_string(),
                command: "set_llm_profile".to_string(),
                args: serde_json::json!({ "model": model }),
            }],
        }),
    }
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
        crate::OperationId::new(
            crate::ExecutionScope::session_operation(&request.session_id, first_batch_id),
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

    /// A deadline the writer races (an obligation attempt's budget) never
    /// moves the clock: only the writer's own waits do, so the deadline
    /// passes when they carry the clock past it.
    async fn sleep_until(&self, deadline: std::time::Instant) {
        let deadline_ms = self.epoch_origin_ms.saturating_add(Self::duration_ms(
            deadline.saturating_duration_since(self.monotonic_origin),
        ));
        self.wait_until_ms(deadline_ms).await;
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
            crate::plugin::SessionAuthorityContext {
                plugin_config: state.admitted_plugin_config(),
                ..Default::default()
            },
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
    // The laws change the session's model: resolution mints the key through
    // the host's models, so the host must serve every key the laws name. The
    // model still never settles — the queued blocker holds the FIFO head.
    let provider = crate::testing::TestProvider::builder()
        .kind("conformance-provider")
        .build()
        .into_handle();
    let mut models = crate::LlmProfileRegistry::new();
    for key in CONFIG_SETTLEMENT_PROFILE_KEYS {
        models = models
            .register(
                key,
                crate::RegisteredLlmProfile::new(
                    crate::testing::test_llm_profile_metadata(key),
                    provider.clone(),
                ),
            )
            .expect("register config-settlement model");
    }
    host.providers.models = Arc::new(models);
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

/// The keys the config-settlement laws move the session to; the host's
/// models serve each.
const CONFIG_SETTLEMENT_PROFILE_KEYS: [&str; 3] =
    ["must-remain-pending", "must-be-cancelled", "first-settled"];

/// A recorded model as a host's models mint it for `key`.
fn config_command_model(key: &str) -> crate::LlmProfileConfig {
    crate::testing::test_llm_profile_config(key, crate::testing::test_llm_profile_metadata(key))
}

/// Submit a config transaction moving the session to the model `key` and
/// read how it settled, once: a transaction no drive applied yet answers
/// `Pending`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store accepts the transaction"
)]
async fn submit_config_settlement(
    runtime: &mut crate::LashRuntime,
    key: &str,
) -> crate::runtime::SessionCommandSettlement {
    let revision = runtime.config_revision();
    let receipt = runtime
        .submit_config_transaction(
            format!("config-settlement:{key}"),
            revision,
            &crate::ConfigTransaction::of(crate::plugin::config::core::SetLlmProfile {
                model: crate::LlmProfileKey::new(key),
            }),
        )
        .await
        .expect("the config transaction is accepted");
    runtime
        .settle_session_command(receipt)
        .await
        .expect("read the config transaction's settlement")
}

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
    let original_profile = runtime.export_persistence_state().policy.model.clone();
    let started = clock.now();
    let settlement = ConfigSettlementClock::driving(Box::pin(submit_config_settlement(
        &mut runtime,
        "must-remain-pending",
    )))
    .await;
    assert!(
        matches!(
            settlement,
            crate::runtime::SessionCommandSettlement::Pending(_)
        ),
        "a blocked config transaction answers pending: {settlement:?}"
    );
    assert_eq!(
        clock.now().saturating_duration_since(started),
        std::time::Duration::ZERO,
        "the submission returns the pending receipt without driving the command"
    );
    assert_eq!(
        runtime.export_persistence_state().policy.model,
        original_profile
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
    let script = Script::new();
    let gate = script
        .on(StoreOp::enqueue_queued_work_with_outcome)
        .after()
        .pause();
    let paused_store = script.wrap("setter", Arc::clone(&store));
    let runtime = runtime_for_config_settlement(
        backend.clone(),
        Arc::clone(&paused_store) as Arc<dyn crate::RuntimeStore>,
        &request,
        Arc::clone(&clock),
    )
    .await;
    let original_profile = runtime.export_persistence_state().policy.model.clone();
    let setter = crate::task::spawn(ConfigSettlementClock::driving(async move {
        let mut runtime = runtime;
        let result = submit_config_settlement(&mut runtime, "must-be-cancelled").await;
        (result, runtime)
    }));

    // Hold the setter before its settlement read. Cancellation must commit
    // before the read, so the batch state has one unambiguous ordering.
    gate.reached(1).await;
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
            .queued_work_batch_completion(&request.session_id, &command_batch.batch_id)
            .await
            .expect("read cancelled config marker")
            .is_some(),
        "cancellation must not manufacture completion evidence"
    );

    gate.open_all();

    let (settlement, runtime) = setter.await.expect("cancelled setter task");
    assert!(
        matches!(
            &settlement,
            crate::runtime::SessionCommandSettlement::Cancelled(receipt)
                if receipt.batch_id == command_batch.batch_id
        ),
        "a cancelled config transaction settles cancelled: {settlement:?}"
    );
    assert_eq!(
        runtime.export_persistence_state().policy.model,
        original_profile
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
        let result = submit_config_settlement(&mut runtime, "first-settled").await;
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
    let superseding_model = Some(config_command_model("second-newer"));
    let newer_model = superseding_model.clone();
    commit_session_command_run_with(&store, &request, &lease, run, move |state| {
        state.policy.model = newer_model;
    })
    .await;

    let (settlement, mut runtime) = setter.await.expect("superseded setter task");
    if let crate::runtime::SessionCommandSettlement::Pending(receipt) = settlement {
        assert!(matches!(
            runtime
                .settle_session_command(receipt)
                .await
                .expect("read superseded command settlement"),
            crate::runtime::SessionCommandSettlement::Durable(_)
        ));
    } else {
        assert!(
            matches!(
                settlement,
                crate::runtime::SessionCommandSettlement::Durable(_)
            ),
            "the superseding writer settled the transaction: {settlement:?}"
        );
    }
    assert_eq!(
        runtime.export_persistence_state().policy.model,
        superseding_model,
        "settlement adopts the newer durable head instead of re-publishing \
         the settled command's older values"
    );
}
