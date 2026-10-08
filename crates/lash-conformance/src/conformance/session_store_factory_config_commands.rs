//! Durable session-config command conformance.

use super::*;
use crate::Clock;
use lash_core::plugin::PluginSessionRequest;
use lash_core::testing::{Script, StoreOp};
use pretty_assertions::assert_eq;
use std::future::Future;

/// [`commit_session_command_run`], with `adjust` applied to the state the
/// settling commit writes.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn commit_session_command_run_with(
    store: &Arc<dyn crate::RuntimeStore>,
    request: &crate::SessionStoreCreateRequest,
    run: Vec<crate::QueuedWorkBatch>,
    adjust: impl FnOnce(&mut crate::RuntimeSessionState),
) {
    let mut state = crate::conformance::helpers::load_window_state(store, &request.session_id)
        .await
        .expect("load config-command state")
        .unwrap_or_else(|| crate::RuntimeSessionState {
            session_id: request.session_id.clone(),
            policy: request.config.session_policy(),
            ..crate::RuntimeSessionState::ambient_fixture(request.config.session_policy())
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
    static SHIFTS_SETTLEMENT_CLOCK: ();
}

/// Virtual clock for runtime settlement tests.
///
/// Only the facade writer executes it: a sleep inside [`Self::executing`] advances
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

    /// Run `writer` as the task whose sleeps shift the clock.
    async fn executing<T>(writer: impl Future<Output = T>) -> T {
        SHIFTS_SETTLEMENT_CLOCK.scope((), writer).await
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
        if SHIFTS_SETTLEMENT_CLOCK.try_with(|()| ()).is_ok() {
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
            ..crate::RuntimeSessionState::ambient_fixture(request.config.session_policy())
        });
    state.ensure_agent_frame_initialized();
    let host = crate::PluginHost::new(
        crate::testing::test_standard_protocol_factories(),
        lash_core::ExecutionBudgets::recommended(),
    );
    let plugins = match state.plugin_state() {
        Some(snapshot) => host.build_session(PluginSessionRequest::rematerialization(
            request.session_id.clone(),
            snapshot,
            crate::plugin::SessionAuthorityContext {
                plugin_config: state.admitted_plugin_config(),
                ..lash_core::plugin::SessionAuthorityContext::ambient_fixture()
            },
        )),
        None => host.build_session(PluginSessionRequest::creation(
            request.session_id.clone(),
            lash_core::plugin::SessionAuthorityContext::ambient_fixture(),
        )),
    }
    .expect("config-settlement plugins");
    let mut host = crate::RuntimeHostConfig::new(
        backend,
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
        lash_core::ToolSourcePolicy::Tolerate,
        lash_core::ExecutionBudgets::recommended(),
        lash_core::runtime::DeltaCoalescing::recommended(),
        crate::DataRetentionConfig::standard(),
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

/// The keys the config-settlement laws move the session to; the host's
/// models serve each.
const CONFIG_SETTLEMENT_PROFILE_KEYS: [&str; 3] =
    ["must-remain-pending", "must-be-cancelled", "first-settled"];

/// A recorded model as a host's models mint it for `key`.
fn config_command_model(key: &str) -> crate::LlmProfileConfig {
    crate::testing::test_llm_profile_config(key, crate::testing::test_llm_profile_metadata(key))
}

/// Submit a config transaction moving the session to the model `key` and
/// read how it settled, once: a transaction no shift applied yet answers
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
    let mut runtime = runtime_for_config_settlement(
        backend.clone(),
        Arc::clone(&store),
        &request,
        Arc::clone(&clock),
    )
    .await;
    let original_profile = runtime.export_persistence_state().policy.model.clone();
    let started = clock.now();
    let settlement = ConfigSettlementClock::executing(Box::pin(submit_config_settlement(
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
        "the submission returns the pending receipt without executing the command"
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
    let setter = crate::task::spawn(ConfigSettlementClock::executing(async move {
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
        .next()
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

    let setter = crate::task::spawn(ConfigSettlementClock::executing(async move {
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
            .next()
        {
            break batch;
        }
        tokio::task::yield_now().await;
    };

    // The second writer drains the facade writer's command...
    let run = store
        .open_session_command_run(&request.session_id)
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
    commit_session_command_run_with(&store, &request, run, move |state| {
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
