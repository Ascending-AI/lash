//! Queue-drain end laws (ADR 0094, FIG-3419): a queued-work drain is a durable
//! owner, and an owner ends through its own write.
//!
//! The end fact is the session-store receipt `{queue_drain scope}/final`; the
//! parent-end ledger row in the process registry follows it. A durable
//! `Failed` settlement is an end: it is terminal, so the drain that settled it
//! ends there (FIG-3559), and a host's abandonment of a pending run is such a
//! settlement (FIG-3560). An end the epilogue withheld after a settlement is
//! owed, and the parent-end recovery pass writes it once the owed closing
//! work settles (FIG-3563). What is *not* an end: a fresh empty poll, an
//! intermediate physical-turn commit, a run whose failure retains ownership,
//! an interrupted run, or the worker dying — a retry under the same
//! `drain_id` is the drain that ends. These laws drive real drains through
//! `LashRuntime::stream_next_queued_work` under an admitted `QueueDrain` scope
//! — and abandon one through `LashRuntime::abandon_queued_run`, the host API's
//! body — and read the outcome only through the surfaces a backend already owes:
//! `SessionCommitStore::drain_end_exists`, the parent-end ledger, and the
//! `Cancel`/`Abandon` split the work driver's sweep performs.
//!
//! # Tier shape
//!
//! Restate holds no Lash parent-end ledger of its own — it rides the SQL
//! registry and SQL session store, so the SQL protocol is its twin — and a
//! tier whose group seam is absent answers these laws the way
//! `tool_child_invocation` treats a `None` drain: the suite is simply not
//! registered there. On the in-memory tier the receipt, the ledger row and
//! the sweep are the same code in the same process, which is what the laws
//! assert; the protected-obligation law documents how the foreign-lease
//! `Pending` shape collapses there.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use lash_core::testing::conformance_support::{EffectGroupLifecycle, StoreEffectGroupClosing};

use super::helpers::admit;
use crate::facade_support::PluginFactory;
use crate::store::RuntimePersistence;
use crate::testing::store_fixtures::bind_conformance_session;
use crate::{
    EffectHost, LashRuntime, LeaseOwnerIdentity, OnParentEnd, ParentScope, PendingTurnInputDraft,
    PluginError, ProcessId, ProcessInput, ProcessLifecyclePolicy, ProcessProvenance, ProcessRecord,
    ProcessRegistration, ProcessRegistry, QueuedTurnDrain, RecoveryContract,
    ScopedEffectController, SessionCommitStore, SessionStoreFactory, TurnInput, TurnInputIngress,
    TurnOptions,
};

/// The session every drain-end law exercises.
pub(crate) const SESSION_ID: &str = "root";

/// What a law needs the next world to carry: the session store the drain
/// commits to, the process registry the runtime and the sweep share, the
/// factory that re-opens that session store for the sweep's end-evidence
/// read, the effect host the drain's admitted scope is minted from, and — for
/// the protected-obligation law — a second host whose group table the
/// runtime's host shares.
pub struct DrainEndWorld {
    pub store: Arc<dyn RuntimePersistence>,
    pub registry: Arc<dyn ProcessRegistry>,
    pub session_factory: Arc<dyn SessionStoreFactory>,
    pub effect_host: Arc<dyn EffectHost>,
    /// A second host over the same group table, so a closing group's live
    /// obligation is owed to a lease this runtime does not hold. `None` on a
    /// tier whose group seam is absent.
    pub group_host: Option<Arc<dyn EffectHost>>,
}

/// Wires a drain-end world's host: the product tool-child resolver, so the
/// drain's tool calls form real effect groups, with the laws' settling
/// resolver behind it for the synthetic groups the closing laws open. One
/// controller carries one resolver, so the synthetic one sits behind the
/// product one rather than beside it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a fresh host has no resolver yet"
)]
pub fn install_drain_end_executors(host: Arc<dyn EffectHost>) -> Arc<dyn EffectHost> {
    host.install_tool_child_host(crate::facade_support::ToolChildHost::new(
        &host,
        Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
    ))
    .expect("a fresh host takes the tool-child resolver")
    .with_law_fallback(
        super::effect_group_drain::RecordingExecutors::settling() as Arc<dyn crate::GroupExecutors>
    );
    host
}

/// Callable per law: each law gets a fresh world over the tier's durable
/// substrate, the same way [`super::effect_group_drain::DrainWorldFactory`]
/// hands each group law its own hosts.
pub type DrainEndWorldFactory = Arc<
    dyn Fn(
            &'static str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DrainEndWorld> + Send>>
        + Send
        + Sync,
>;

fn drain_parent(drain_id: &str) -> ParentScope {
    ParentScope::queue_drain(SESSION_ID, drain_id)
}

fn drain_scope(drain_id: &str) -> crate::ExecutionScope {
    crate::ExecutionScope::queue_drain(SESSION_ID, drain_id)
}

/// A `Cancel`/`Abandon` child of the drain owner, registered through the real
/// write path the sweep later reads.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn register_drain_child(
    registry: &Arc<dyn ProcessRegistry>,
    drain_id: &str,
    id: &str,
    on_parent_end: OnParentEnd,
) -> ProcessRecord {
    registry
        .register_process(ProcessRegistration::new(
            ProcessId::from(id),
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryContract::ExternallyOwned,
            ProcessProvenance::session(crate::SessionScope::new(SESSION_ID)),
            ProcessLifecyclePolicy::new(drain_parent(drain_id), on_parent_end),
        ))
        .await
        .expect("register a drain-scoped child")
}

fn text_response(text: &str) -> crate::LlmResponse {
    crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..crate::LlmResponse::default()
    }
}

fn fixed_text_provider(text: &str) -> crate::ProviderHandle {
    let text = text.to_string();
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_| {
            let text = text.clone();
            async move { Ok(text_response(&text)) }
        })
        .build()
        .into_handle()
}

/// The tool whose call forces L1's follow-on frame.
struct DrainEndTool {
    executed: Arc<AtomicUsize>,
}

fn drain_end_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:drain_end_probe",
        "drain_end_probe",
        "A tool whose call forces a second agent frame.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for DrainEndTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![drain_end_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "drain_end_probe").then(|| Arc::new(drain_end_tool().contract()))
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: non-empty frame material always derives"
    )]
    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        // A `SwitchAgentFrame` control makes the protocol close frame 0 with
        // `TurnOutcome::AgentFrameSwitch`, so the drain's logical run commits
        // a second physical turn under the same owner.
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({"done": true})).with_control(
                crate::ToolControl::SwitchAgentFrame {
                    frame_key: crate::FrameKey::from_caller_material("drain-end-follow-on")
                        .expect("non-empty frame material derives"),
                    initial_nodes: Vec::new(),
                    task: Some("drain-end follow-on frame".to_string()),
                },
            ),
        ))
    }
}

/// The commit budget every law's runtime runs under unless it needs a
/// terminal commit failure.
fn law_commit_budget() -> crate::CommitBudget {
    crate::CommitBudget::bounded(1024 * 1024, 512)
}

/// The runtime fixture every law shares: one queued-work-capable runtime over
/// the world's store with `registry` wired as its own, so the drain-end
/// epilogue writes to exactly the handles the law asserts on.
async fn drain_runtime(
    world: &DrainEndWorld,
    registry: Arc<dyn ProcessRegistry>,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn PluginFactory>>,
    lease_owner: LeaseOwnerIdentity,
) -> LashRuntime {
    drain_runtime_with_budget(
        world,
        registry,
        provider,
        plugin_factories,
        lease_owner,
        law_commit_budget(),
    )
    .await
}

/// [`drain_runtime`] under an explicit commit budget.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn drain_runtime_with_budget(
    world: &DrainEndWorld,
    registry: Arc<dyn ProcessRegistry>,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn PluginFactory>>,
    lease_owner: LeaseOwnerIdentity,
    commit_budget: crate::CommitBudget,
) -> LashRuntime {
    // `with_effect_host`, not a field overwrite: it installs the tool-child
    // resolver on the drain's host, where the drain's tool groups open.
    let mut host =
        crate::RuntimeHostConfig::in_memory(commit_budget, crate::QueuedWorkBatchingConfig::new(1))
            .with_effect_host(Arc::clone(&world.effect_host));
    host.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(provider));
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(SessionId::from(SESSION_ID));
    // No `with_initial_state`: the laws bind the session before any drain, so
    // the builder restores the committed state — a retried drain's runtime
    // must reopen the session its crashed predecessor committed, not fabricate
    // a fresh frame the drain-end receipt commit would refuse.
    Box::pin(
        crate::LashRuntime::builder(
            commit_budget,
            crate::QueuedWorkBatchingConfig::new(1),
            lease_owner,
        )
        .with_session_id(SESSION_ID)
        .with_policy(policy)
        .with_runtime_host(host)
        .with_plugin_factories(
            crate::testing::test_standard_protocol_factories()
                .into_iter()
                .chain(plugin_factories)
                .collect(),
        )
        .with_process_registry(registry)
        .with_store(Arc::clone(&world.store))
        .build(),
    )
    .await
    .expect("build the drain-end conformance runtime")
}

/// Admit the drain scope and drive one real queued-work drain.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a queue_drain scope always admits unpinned"
)]
async fn drive_drain(
    runtime: &mut LashRuntime,
    effect_host: &Arc<dyn EffectHost>,
    drain_id: &str,
) -> Result<QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError> {
    let scope: ScopedEffectController<'_> = effect_host
        .scoped(admit(drain_scope(drain_id)))
        .expect("scope the drain");
    runtime
        .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), scope))
        .await
}

/// The worker half of the laws: the same registry, the factory that re-opens
/// the session's store so `drain_end_exists` answers, and no other work to
/// drive.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the worker over the world's registry always builds"
)]
fn drain_sweep(world: &DrainEndWorld) -> lash_core_worker::DurableProcessWorker {
    let watched = crate::facade_support::watch_process_registry(Arc::clone(&world.registry));
    let host = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_effect_host(Arc::clone(&world.effect_host));
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(SessionId::from(SESSION_ID));
    lash_core_worker::DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            Arc::new(crate::facade_support::PluginHost::new(
                crate::testing::test_standard_protocol_factories(),
            )),
            host,
            Arc::clone(&world.session_factory),
            lash_core_worker::WorkerProcessWork::SelfNative(watched),
            Arc::new(crate::NoQueuedWork::new()),
            crate::testing::runtime_lease_owner(),
        )
        .with_session_policy(policy),
    )
    .expect("build the drain-end sweep worker")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the sweep over the world's registry always runs"
)]
async fn run_sweep(world: &DrainEndWorld) {
    let _ = drain_sweep(world)
        .drive_pending_processes()
        .await
        .expect("the parent-end sweep runs");
}

/// The drain's own end evidence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each read is established by the setup"
)]
async fn drain_ended(store: &Arc<dyn RuntimePersistence>, drain_id: &str) -> bool {
    SessionCommitStore::drain_end_exists(store.as_ref(), drain_id)
        .await
        .expect("read the drain-end receipt")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each read is established by the setup"
)]
async fn drain_ledger_row(
    registry: &Arc<dyn ProcessRegistry>,
    drain_id: &str,
) -> Option<crate::ParentEndPlan> {
    registry
        .get_parent_end_plan(&drain_parent(drain_id))
        .await
        .expect("read the drain's parent-end ledger row")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each read is established by the setup"
)]
async fn child(registry: &Arc<dyn ProcessRegistry>, id: &str) -> ProcessRecord {
    registry
        .get_process(&ProcessId::from(id))
        .await
        .expect("read the child")
        .expect("the child exists")
}

fn cancel_origin(record: &ProcessRecord) -> Option<crate::CancelOrigin> {
    record.cancel_request.as_ref().map(|request| request.origin)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each write is established by the setup"
)]
async fn seed_turn_input(store: &Arc<dyn RuntimePersistence>, text: &str) {
    store
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            SessionId::from(SESSION_ID),
            TurnInputIngress::NextTurn,
            TurnInput::text(text),
        ))
        .await
        .expect("seed the drain's turn input");
}

/// **L1 — multi-physical-turn drain.** A `Cancel` child named by the drain
/// owner is not swept by the drain's intermediate physical-turn commits — the
/// owner has not ended — and is `ParentEnded`-cancelled only once the drain's
/// own end lands and the sweep runs.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_multi_frame_drain_sweeps_children_only_at_its_own_end(
    prefix: &str,
    world: DrainEndWorld,
) {
    let drain_id = format!("{prefix}-l1-drain");
    let child_id = format!("{prefix}-l1-child");
    register_drain_child(&world.registry, &drain_id, &child_id, OnParentEnd::Cancel).await;
    bind_conformance_session(&world.store, &SessionId::from(SESSION_ID)).await;
    seed_turn_input(&world.store, "run a two-frame drain").await;

    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let (frame_one_tx, frame_one_rx) = tokio::sync::oneshot::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(Some(release_rx)));
    let frame_one_tx = Arc::new(std::sync::Mutex::new(Some(frame_one_tx)));
    let calls = Arc::new(AtomicUsize::new(0));
    let tool_executed = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let calls = Arc::clone(&calls);
            let release_rx = Arc::clone(&release_rx);
            let frame_one_tx = Arc::clone(&frame_one_tx);
            move |_| {
                let calls = Arc::clone(&calls);
                let release_rx = Arc::clone(&release_rx);
                let frame_one_tx = Arc::clone(&frame_one_tx);
                async move {
                    match calls.fetch_add(1, Ordering::SeqCst) {
                        0 => Ok(crate::LlmResponse {
                            parts: vec![crate::LlmOutputPart::ToolCall {
                                call_id: "drain-end-probe-call".to_string(),
                                tool_name: "drain_end_probe".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            response_metadata: Default::default(),
                            ..crate::LlmResponse::default()
                        }),
                        1 => {
                            // Frame 0 has committed by the time this call
                            // runs; hold frame 1 open until the law has read
                            // the mid-drain state. Take both channels before
                            // the await: a `MutexGuard` is not `Send`.
                            if let Some(sender) = frame_one_tx.lock_recover().take() {
                                let _ = sender.send(());
                            }
                            let release = release_rx.lock_recover().take();
                            if let Some(release) = release {
                                release.await.expect("frame-one release");
                            }
                            Ok(text_response("drain ends after frame one"))
                        }
                        index => panic!("unexpected drain-end model call {index}"),
                    }
                }
            }
        })
        .build();
    let tool_plugin: Arc<dyn PluginFactory> = Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-drain-end-probe",
        crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(DrainEndTool {
            executed: Arc::clone(&tool_executed),
        })),
    ));

    let mut runtime = drain_runtime(
        &world,
        Arc::clone(&world.registry),
        provider.into_handle(),
        vec![tool_plugin],
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let drain = crate::task::spawn({
        let effect_host = Arc::clone(&world.effect_host);
        let drain_id = drain_id.clone();
        async move { drive_drain(&mut runtime, &effect_host, &drain_id).await }
    });

    match tokio::time::timeout(std::time::Duration::from_secs(5), frame_one_rx).await {
        Ok(Ok(())) => {}
        other => {
            let result = drain.await;
            let outcome = match result {
                Ok(Ok(QueuedTurnDrain::Ran(turn))) => format!("{:?}", turn.outcome),
                other_result => format!("{other_result:?}"),
            };
            panic!(
                "the follow-on frame was never entered ({other:?}); calls={} tool_executed={} outcome={outcome:?}",
                calls.load(Ordering::SeqCst),
                tool_executed.load(Ordering::SeqCst),
            )
        }
    }
    // Frame 0's commit wrote its *turn* parent-end row, not the drain's: the
    // drain owner has not ended, so its child carries no cancel request.
    assert!(
        cancel_origin(&child(&world.registry, &child_id).await).is_none(),
        "a mid-drain physical-turn commit must not sweep the drain's children"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_none(),
        "the drain's ledger row lands only at the drain's own end"
    );
    release_tx.send(()).expect("release the follow-on frame");
    let result = drain
        .await
        .expect("the drain task completes")
        .expect("the two-frame drain runs");
    assert!(
        matches!(result, QueuedTurnDrain::Ran(_)),
        "the drain ran a turn"
    );
    assert!(
        drain_ended(&world.store, &drain_id).await,
        "a completed drain writes its end receipt"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_some(),
        "a completed drain writes its parent-end ledger row"
    );

    run_sweep(&world).await;
    assert_eq!(
        cancel_origin(&child(&world.registry, &child_id).await),
        Some(crate::CancelOrigin::ParentEnded),
        "the drain's end sweeps its Cancel child"
    );
}

/// **L2 — Cancel vs Abandon.** At the drain's end the sweep requests
/// `ParentEnded` on its `Cancel` children; `Abandon` children stay
/// host-managed — live rows with no cancel request.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_drain_end_cancels_cancel_children_and_leaves_abandon_ones(
    prefix: &str,
    world: DrainEndWorld,
) {
    let drain_id = format!("{prefix}-l2-drain");
    let cancel_id = format!("{prefix}-l2-cancel");
    let abandon_id = format!("{prefix}-l2-abandon");
    for (id, on_parent_end) in [
        (cancel_id.clone(), OnParentEnd::Cancel),
        (abandon_id.clone(), OnParentEnd::Abandon),
    ] {
        register_drain_child(&world.registry, &drain_id, &id, on_parent_end).await;
    }
    bind_conformance_session(&world.store, &SessionId::from(SESSION_ID)).await;
    seed_turn_input(&world.store, "run a one-frame drain").await;

    let mut runtime = drain_runtime(
        &world,
        Arc::clone(&world.registry),
        fixed_text_provider("done"),
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let result = drive_drain(&mut runtime, &world.effect_host, &drain_id)
        .await
        .expect("the drain runs");
    assert!(matches!(result, QueuedTurnDrain::Ran(_)), "the drain ran");
    assert!(
        drain_ended(&world.store, &drain_id).await
            && drain_ledger_row(&world.registry, &drain_id).await.is_some(),
        "the drain wrote its end"
    );

    run_sweep(&world).await;
    assert_eq!(
        cancel_origin(&child(&world.registry, &cancel_id).await),
        Some(crate::CancelOrigin::ParentEnded),
        "the sweep requests ParentEnded on the drain's Cancel child"
    );
    let abandon = child(&world.registry, &abandon_id).await;
    assert!(
        abandon.cancel_request.is_none(),
        "an Abandon child owes the ended drain nothing and stays host-managed"
    );
    assert!(
        abandon.status.is_live(),
        "the Abandon child is still a live process row"
    );
}

/// **L3 — crash after the receipt, before the ledger row.** The two writes
/// are on separate stores; a crash between them leaves the receipt durable
/// and the row missing. The sweep confirms the receipt through
/// `drain_end_exists`, re-derives the row, and cancels the child.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_crash_after_the_drain_receipt_recovers_its_ledger_row(
    prefix: &str,
    world: DrainEndWorld,
) {
    let drain_id = format!("{prefix}-l3-drain");
    let child_id = format!("{prefix}-l3-child");
    register_drain_child(&world.registry, &drain_id, &child_id, OnParentEnd::Cancel).await;
    bind_conformance_session(&world.store, &SessionId::from(SESSION_ID)).await;
    seed_turn_input(&world.store, "run under a crash-injected ledger").await;

    // The injected failure IS the crash window: the epilogue's receipt commit
    // has already landed when `record_parent_end` answers Err, which is the
    // exact durable shape a crash between the two writes leaves.
    let faulting =
        crate::fail_parent_end_once(Arc::clone(&world.registry), drain_parent(&drain_id));
    let mut runtime = drain_runtime(
        &world,
        faulting,
        fixed_text_provider("done"),
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    drive_drain(&mut runtime, &world.effect_host, &drain_id)
        .await
        .expect("an epilogue gap does not fail the committed run");
    assert!(
        drain_ended(&world.store, &drain_id).await,
        "the receipt committed before the injected crash"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_none(),
        "the crash took the ledger-row write"
    );
    assert!(
        cancel_origin(&child(&world.registry, &child_id).await).is_none(),
        "nothing has swept the child yet"
    );

    run_sweep(&world).await;
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_some(),
        "the sweep re-derived the missing ledger row from the durable receipt"
    );
    assert_eq!(
        cancel_origin(&child(&world.registry, &child_id).await),
        Some(crate::CancelOrigin::ParentEnded),
        "the re-derived row sweeps the drain's Cancel child"
    );
}

/// **L4 — empty drains.** A fresh empty poll owns nothing and writes neither
/// receipt nor row; a drain whose children already exist ends on an empty
/// retry, because the children prove the run once did work.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_empty_drain_writes_nothing_and_a_retried_one_ends(
    prefix: &str,
    world: DrainEndWorld,
) {
    bind_conformance_session(&world.store, &SessionId::from(SESSION_ID)).await;
    let mut runtime = drain_runtime(
        &world,
        Arc::clone(&world.registry),
        fixed_text_provider("unreached"),
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;

    let fresh = format!("{prefix}-l4-fresh");
    let empty = drive_drain(&mut runtime, &world.effect_host, &fresh)
        .await
        .expect("an empty poll is not an error");
    assert!(
        matches!(empty, QueuedTurnDrain::Empty(_)),
        "the fresh drain found nothing: {empty:?}"
    );
    assert!(
        !drain_ended(&world.store, &fresh).await,
        "a fresh empty poll writes no end receipt"
    );
    assert!(
        drain_ledger_row(&world.registry, &fresh).await.is_none(),
        "a fresh empty poll writes no ledger row"
    );

    let retried = format!("{prefix}-l4-retried");
    register_drain_child(
        &world.registry,
        &retried,
        &format!("{prefix}-l4-child"),
        OnParentEnd::Cancel,
    )
    .await;
    let empty = drive_drain(&mut runtime, &world.effect_host, &retried)
        .await
        .expect("the resumed drain still finds an empty queue");
    assert!(
        matches!(empty, QueuedTurnDrain::Empty(_)),
        "the retried drain had no work left: {empty:?}"
    );
    assert!(
        drain_ended(&world.store, &retried).await,
        "a drain that already owns children ends on its empty retry"
    );
    assert!(
        drain_ledger_row(&world.registry, &retried).await.is_some(),
        "the empty retry writes the ledger row"
    );
}

/// **L5 — failed run that retains ownership.** A drain whose run fails with
/// an unclassified error — a `before_turn` refusal, which leaves the run
/// pending rather than settling it — writes neither receipt nor row
/// (interrupted, not ended), and its children stay live. The retry under the
/// same `drain_id` is the drain that ends. A terminal failure is L8's.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_failed_drain_writes_no_end_and_its_retry_ends_it(
    prefix: &str,
    world: DrainEndWorld,
) {
    let drain_id = format!("{prefix}-l5-drain");
    let child_id = format!("{prefix}-l5-child");
    register_drain_child(&world.registry, &drain_id, &child_id, OnParentEnd::Cancel).await;
    bind_conformance_session(&world.store, &SessionId::from(SESSION_ID)).await;
    seed_turn_input(&world.store, "fail once, then commit").await;

    let fail = Arc::new(AtomicBool::new(true));
    let abort_plugin: Arc<dyn PluginFactory> = Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-drain-end-abort",
        crate::facade_support::PluginSpec::new().with_before_turn(Arc::new({
            let fail = Arc::clone(&fail);
            move |_ctx| {
                let fail = Arc::clone(&fail);
                Box::pin(async move {
                    if fail.swap(false, Ordering::SeqCst) {
                        Err(PluginError::Invoke(
                            "conformance drain failure before the run commits".to_string(),
                        ))
                    } else {
                        Ok(Vec::new())
                    }
                })
            }
        })),
    ));
    let mut runtime = drain_runtime(
        &world,
        Arc::clone(&world.registry),
        fixed_text_provider("the retry commits"),
        vec![abort_plugin],
        crate::testing::runtime_lease_owner(),
    )
    .await;

    drive_drain(&mut runtime, &world.effect_host, &drain_id)
        .await
        .expect_err("the armed failure aborts the first drain");
    assert!(
        !drain_ended(&world.store, &drain_id).await,
        "a failed run writes no end receipt"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_none(),
        "a failed run writes no ledger row"
    );
    let live = child(&world.registry, &child_id).await;
    assert!(
        live.status.is_live() && live.cancel_request.is_none(),
        "the interrupted drain's children stay live"
    );

    let retried = drive_drain(&mut runtime, &world.effect_host, &drain_id)
        .await
        .expect("the retry under the same drain_id runs");
    assert!(matches!(retried, QueuedTurnDrain::Ran(_)), "the retry ran");
    assert!(
        drain_ended(&world.store, &drain_id).await,
        "the retry completes the interrupted drain's end"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_some(),
        "the retry writes the ledger row"
    );
}

/// **L6 — interruption before the receipt.** A drain killed after its
/// terminal physical commit but before the epilogue's receipt is *not* ended;
/// the retry under the same `drain_id` writes both end facts. The phase probe
/// standing at the epilogue's entry is the deterministic stand-in for the
/// kill.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_interrupted_drain_is_ended_by_its_retry(prefix: &str, world: DrainEndWorld) {
    let drain_id = format!("{prefix}-l6-drain");
    register_drain_child(
        &world.registry,
        &drain_id,
        &format!("{prefix}-l6-child"),
        OnParentEnd::Cancel,
    )
    .await;
    bind_conformance_session(&world.store, &SessionId::from(SESSION_ID)).await;
    seed_turn_input(&world.store, "commit, then die before the epilogue").await;

    let armed = Arc::new(AtomicBool::new(true));
    let mut runtime = drain_runtime(
        &world,
        Arc::clone(&world.registry),
        fixed_text_provider("committed before the crash"),
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(EpilogueKillProbe {
        armed: Arc::clone(&armed),
    }));

    let task = crate::task::spawn({
        let effect_host = Arc::clone(&world.effect_host);
        let drain_id = drain_id.clone();
        async move { drive_drain(&mut runtime, &effect_host, &drain_id).await }
    });
    // A panic unwinds the drain future to a join error; a runtime that instead
    // converts the abort into a turn error is the same durable outcome.
    let outcome = task.await;
    assert!(
        matches!(outcome, Err(_) | Ok(Err(_))),
        "the killed drain cannot report a completed run: {outcome:?}"
    );
    assert!(
        !drain_ended(&world.store, &drain_id).await,
        "the interrupted drain wrote no end receipt"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_none(),
        "the interrupted drain wrote no ledger row"
    );

    armed.store(false, Ordering::SeqCst);
    // The killed drain's session lane is released by its dropped lease guard
    // on a spawned best-effort task, not by the unwind itself — and a real
    // kill releases nothing, leaving the lane to its expiry. Either way the
    // retry is the worker that finds the lane claimable, so the law waits for
    // that durable fact: building the retry against a lane the dead drain
    // still holds is refused as `Contended` (FIG-3566).
    until_lane_claimable(&world.store).await;
    let mut retry_runtime = drain_runtime(
        &world,
        Arc::clone(&world.registry),
        fixed_text_provider("the retry commits"),
        Vec::new(),
        crate::LeaseOwnerIdentity::opaque(
            format!("{prefix}-l6-retry-owner"),
            format!("{prefix}-l6-retry-incarnation"),
        ),
    )
    .await;
    drive_drain(&mut retry_runtime, &world.effect_host, &drain_id)
        .await
        .expect("the retry under the same drain_id runs");
    assert!(
        drain_ended(&world.store, &drain_id).await,
        "the retry completes the interrupted drain's end"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_some(),
        "the retry writes the ledger row"
    );
}

/// Poll interval for [`until_lane_claimable`]'s durable read.
const POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Hang detector for a law's wait on a durable fact, not a latency
/// expectation.
const AWAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// Wait until no live session execution lease holds the conformance
/// session's lane: released, or past its expiry at the store's own
/// observation time. Bounded by [`AWAIT_BUDGET`] as a hang detector.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each read is established by the setup"
)]
async fn until_lane_claimable(store: &Arc<dyn RuntimePersistence>) {
    tokio::time::timeout(AWAIT_BUDGET, async {
        loop {
            let observation = store
                .get_session_execution_lease(&SessionId::from(SESSION_ID))
                .await
                .expect("read the session's execution lease");
            if observation
                .lease
                .is_none_or(|lease| lease.expires_at_epoch_ms <= observation.observed_at_epoch_ms)
            {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the killed drain's session lane becomes claimable");
}

/// The probe that stands in for a worker kill at the epilogue's entry: the
/// physical turn's commit has already landed, so unwinding here leaves
/// exactly the durable shape a crash before the receipt does.
struct EpilogueKillProbe {
    armed: Arc<AtomicBool>,
}

impl crate::RuntimeTurnPhaseProbe for EpilogueKillProbe {
    fn begin(&self, _phase: crate::RuntimeTurnPhase) {}

    fn end(&self, _phase: crate::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == "queue_drain.parent_end" && self.armed.load(Ordering::SeqCst) {
            panic!("injected kill between the terminal commit and the drain-end receipt");
        }
    }
}

/// **L7 — protected obligation.** A `closing` group under the drain's scope
/// that still owes a settlement withholds the drain's end — no receipt, no
/// row — and once the group settles the drain's retry ends.
///
/// On the SQL tiers the group is opened on a *second* host over the same
/// journal, so its blocked loser's lease is foreign to the draining runtime
/// and `resume_closing_groups` reports `Pending` — the drain declines
/// promptly. On the in-memory tier both hosts share one controller, so the
/// same shape reads as a locally running obligation the finalizer waits on:
/// the drain parks inside its epilogue instead of declining. The law carries
/// both shapes — the drain must not have ended while the obligation stands,
/// however the tier reports it — and the release is what ends it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_closing_group_under_the_drain_scope_withholds_its_end(
    prefix: &str,
    world: DrainEndWorld,
) {
    let Some(group_host) = world.group_host.clone() else {
        // The tier's embedding carries no group seam: nothing to assert, the
        // way the Restate tier lacks the suite entirely — see the module doc.
        return;
    };
    let drain_id = format!("{prefix}-l7-drain");
    bind_conformance_session(&world.store, &SessionId::from(SESSION_ID)).await;
    seed_turn_input(&world.store, "a drain gated on a closing group").await;
    // The drain owns a child, so the retry that ends it is a resumed drain,
    // not a fresh empty poll: the epilogue distinguishes the two by the
    // registry, and an `Abandon` child is exactly the child a drain can own
    // while a closing group withholds its end.
    register_drain_child(
        &world.registry,
        &drain_id,
        &format!("{prefix}-l7-abandon-child"),
        OnParentEnd::Abandon,
    )
    .await;

    // Open and close a group under the drain's scope on the group host: one
    // child settles, the loser holds a release gate, so `closing` reports an
    // obligation.
    let loser_release = CancellationToken::new();
    let loser_entered = Arc::new(AtomicUsize::new(0));
    let scoped = group_host
        .scoped(admit(drain_scope(&drain_id)))
        .expect("scope the closing group's opener");
    let group_key = super::effect_group_drain::group_key(prefix, "l7");
    let mut handle = super::effect_group_drain::open(
        &scoped,
        &group_key,
        2,
        crate::LoserPolicy::RunToCompletion,
        vec![
            super::effect_group_drain::settles(0),
            gated_executor(&loser_entered, loser_release.clone()),
        ],
    )
    .await;
    let _winner = super::effect_group_drain::next(&scoped, &mut handle).await;
    super::effect_group_drain::close(&scoped, handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the group records closing");
    super::effect_group_drain::until(|| loser_entered.load(Ordering::SeqCst) == 1).await;

    let epilogue_entered = Arc::new(tokio::sync::Notify::new());
    let mut runtime = drain_runtime(
        &world,
        Arc::clone(&world.registry),
        fixed_text_provider("ran while the group was still closing"),
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(EpilogueSignal {
        entered: Arc::clone(&epilogue_entered),
    }));

    let drain = crate::task::spawn({
        let effect_host = Arc::clone(&world.effect_host);
        let drain_id = drain_id.clone();
        async move { drive_drain(&mut runtime, &effect_host, &drain_id).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        epilogue_entered.notified(),
    )
    .await
    .expect("the drain reaches its end epilogue");

    // The withheld shape: give the drain a grace window to either decline
    // (Pending, the SQL-tier answer) or stay parked in the finalizer's wait
    // (the in-memory answer). It must not have ended either way.
    let mut drain = Some(drain);
    let mut declined = false;
    for _ in 0..200 {
        if drain.as_ref().expect("the drain task").is_finished() {
            declined = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    if declined {
        drain
            .take()
            .expect("the drain task")
            .await
            .expect("the withheld drain task joins")
            .expect("a withheld drain still returns its committed run");
    } else {
        assert!(
            !drain.as_ref().expect("the drain task").is_finished(),
            "the drain stays parked on the closing group's obligation"
        );
    }
    assert!(
        !drain_ended(&world.store, &drain_id).await,
        "a closing group's unsettled obligation withholds the drain's end"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_none(),
        "a withheld drain writes no ledger row"
    );

    loser_release.cancel();
    let closing = group_host
        .effect_group_closing()
        .expect("the group host answers the closing seam");
    until_group_settled(&closing, &group_key).await;

    if !declined {
        // The in-memory shape: the parked drain's resume now completes.
        drain
            .take()
            .expect("the drain task")
            .await
            .expect("the released drain task joins")
            .expect("the released drain ends");
    } else {
        let mut retry_runtime = drain_runtime(
            &world,
            Arc::clone(&world.registry),
            fixed_text_provider("the retry ends the drain"),
            Vec::new(),
            crate::LeaseOwnerIdentity::opaque(
                format!("{prefix}-l7-retry-owner"),
                format!("{prefix}-l7-retry-incarnation"),
            ),
        )
        .await;
        drive_drain(&mut retry_runtime, &world.effect_host, &drain_id)
            .await
            .expect("the retry under the same drain_id runs");
    }
    assert!(
        drain_ended(&world.store, &drain_id).await,
        "the drain ends once its protected obligation settles"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_some(),
        "the end writes the ledger row"
    );
}

/// **L8 — a durably `Failed` drain ends.** A terminal error settles the run
/// durably `Failed`, which nothing retries, so the settlement is the drain's
/// end: the epilogue runs under the lane the failed drain still holds. Here a
/// tool child is still live under a `closing` group of the drain's scope when
/// the run fails; the epilogue waits out that protected obligation, then
/// writes the receipt and the ledger row, and the sweep cancels the drain's
/// `Cancel` child. No retry is involved — without the epilogue on the
/// `Failed` path the drain never reaches it.
///
/// The group runs on the drain's own host, so on every tier its loser is this
/// host's running obligation and the finalizer waits for it rather than
/// reporting `Pending`. The terminal error is a turn commit over a one-node
/// budget; the end receipt appends no node, so it fits.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_durably_failed_drain_settles_its_closing_group_and_ends(
    prefix: &str,
    world: DrainEndWorld,
) {
    let Some(closing) = world.effect_host.effect_group_closing() else {
        // No group seam on this tier's embedding — see the module doc.
        return;
    };
    let drain_id = format!("{prefix}-l8-drain");
    let cancel_id = format!("{prefix}-l8-cancel");
    register_drain_child(&world.registry, &drain_id, &cancel_id, OnParentEnd::Cancel).await;
    bind_conformance_session(&world.store, &SessionId::from(SESSION_ID)).await;
    seed_turn_input(&world.store, "a drain that fails terminally").await;

    // A group under the drain's scope, closed while its loser — the live tool
    // child — still runs.
    let loser_release = CancellationToken::new();
    let loser_entered = Arc::new(AtomicUsize::new(0));
    let scoped = world
        .effect_host
        .scoped(admit(drain_scope(&drain_id)))
        .expect("scope the closing group's opener");
    let group_key = super::effect_group_drain::group_key(prefix, "l8");
    let mut handle = super::effect_group_drain::open(
        &scoped,
        &group_key,
        2,
        crate::LoserPolicy::RunToCompletion,
        vec![
            super::effect_group_drain::settles(0),
            gated_executor(&loser_entered, loser_release.clone()),
        ],
    )
    .await;
    let _winner = super::effect_group_drain::next(&scoped, &mut handle).await;
    super::effect_group_drain::close(&scoped, handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the group records closing");
    super::effect_group_drain::until(|| loser_entered.load(Ordering::SeqCst) == 1).await;

    let epilogue_entered = Arc::new(tokio::sync::Notify::new());
    let mut runtime = drain_runtime_with_budget(
        &world,
        Arc::clone(&world.registry),
        fixed_text_provider("a turn too large to commit"),
        Vec::new(),
        crate::testing::runtime_lease_owner(),
        crate::CommitBudget::bounded(1024 * 1024, 1),
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(EpilogueSignal {
        entered: Arc::clone(&epilogue_entered),
    }));
    let drain = crate::task::spawn({
        let effect_host = Arc::clone(&world.effect_host);
        let drain_id = drain_id.clone();
        async move { drive_drain(&mut runtime, &effect_host, &drain_id).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        epilogue_entered.notified(),
    )
    .await
    .expect("the durably Failed drain reaches its end epilogue");

    assert!(
        world
            .store
            .pending_queued_run(&SessionId::from(SESSION_ID))
            .await
            .expect("read the pending queued run")
            .is_none(),
        "the terminal error settled the run: nothing is left to retry"
    );
    assert!(
        !drain_ended(&world.store, &drain_id).await,
        "the live tool child's obligation withholds the end until it settles"
    );

    loser_release.cancel();
    let error = drain
        .await
        .expect("the failed drain task joins")
        .expect_err("the drain's run failed");
    assert!(
        error.is_terminal(),
        "the drain failed terminally, not retryably: {error:?}"
    );
    until_group_settled(&closing, &group_key).await;
    assert!(
        drain_ended(&world.store, &drain_id).await,
        "the durably Failed drain wrote its end receipt"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_some(),
        "the durably Failed drain wrote its ledger row"
    );

    run_sweep(&world).await;
    assert_eq!(
        cancel_origin(&child(&world.registry, &cancel_id).await),
        Some(crate::CancelOrigin::ParentEnded),
        "the ended drain's Cancel child is swept"
    );
}

/// **L9 — an abandoned drain ends.** A host that no longer wants a pending
/// run recovered abandons it through `abandon_queued_run`, which settles it
/// durably `Failed`; that settlement is terminal, so it is the drain's end
/// exactly as L8's is. Here the drain's run is left pending by an
/// unclassified failure while a tool child is still live under a `closing`
/// group of its scope; the abandonment waits out that protected obligation,
/// then writes the receipt and the ledger row, and the sweep cancels the
/// drain's `Cancel` child. No drain under the abandoned `drain_id` ever runs
/// again — without the end on the abandonment path nothing would reach it.
///
/// As in L8 the group runs on the drain's own host, so on every tier its
/// loser is this host's running obligation and the finalizer waits for it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_abandoned_drain_settles_its_closing_group_and_ends(
    prefix: &str,
    world: DrainEndWorld,
) {
    let Some(closing) = world.effect_host.effect_group_closing() else {
        // No group seam on this tier's embedding — see the module doc.
        return;
    };
    let drain_id = format!("{prefix}-l9-drain");
    let cancel_id = format!("{prefix}-l9-cancel");
    register_drain_child(&world.registry, &drain_id, &cancel_id, OnParentEnd::Cancel).await;
    bind_conformance_session(&world.store, &SessionId::from(SESSION_ID)).await;
    seed_turn_input(&world.store, "a drain its host abandons").await;

    // A `before_turn` refusal is unclassified: the run stays pending, owned
    // by the drain, for a recovery the host then declines.
    let refuse: Arc<dyn PluginFactory> = Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-drain-end-refuse",
        crate::facade_support::PluginSpec::new().with_before_turn(Arc::new(|_ctx| {
            Box::pin(async {
                Err(PluginError::Invoke(
                    "conformance drain refused before the run commits".to_string(),
                ))
            })
        })),
    ));
    let mut runtime = drain_runtime(
        &world,
        Arc::clone(&world.registry),
        fixed_text_provider("unreached"),
        vec![refuse],
        crate::testing::runtime_lease_owner(),
    )
    .await;
    drive_drain(&mut runtime, &world.effect_host, &drain_id)
        .await
        .expect_err("the refused run fails");
    let pending = world
        .store
        .pending_queued_run(&SessionId::from(SESSION_ID))
        .await
        .expect("read the pending queued run")
        .expect("the refused run stays pending for recovery");
    assert_eq!(
        pending.scope,
        drain_scope(&drain_id),
        "the pending run is the drain's"
    );

    // A group under the drain's scope, closed while its loser — the live tool
    // child — still runs.
    let loser_release = CancellationToken::new();
    let loser_entered = Arc::new(AtomicUsize::new(0));
    let scoped = world
        .effect_host
        .scoped(admit(drain_scope(&drain_id)))
        .expect("scope the closing group's opener");
    let group_key = super::effect_group_drain::group_key(prefix, "l9");
    let mut handle = super::effect_group_drain::open(
        &scoped,
        &group_key,
        2,
        crate::LoserPolicy::RunToCompletion,
        vec![
            super::effect_group_drain::settles(0),
            gated_executor(&loser_entered, loser_release.clone()),
        ],
    )
    .await;
    let _winner = super::effect_group_drain::next(&scoped, &mut handle).await;
    super::effect_group_drain::close(&scoped, handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the group records closing");
    super::effect_group_drain::until(|| loser_entered.load(Ordering::SeqCst) == 1).await;

    let epilogue_entered = Arc::new(tokio::sync::Notify::new());
    runtime.set_turn_phase_probe(Arc::new(EpilogueSignal {
        entered: Arc::clone(&epilogue_entered),
    }));
    let abandonment = crate::task::spawn(async move {
        runtime
            .abandon_queued_run(
                pending.scope,
                pending.revision,
                "the host declines recovery".to_string(),
            )
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        epilogue_entered.notified(),
    )
    .await
    .expect("the abandonment reaches the drain-end epilogue");

    assert!(
        world
            .store
            .pending_queued_run(&SessionId::from(SESSION_ID))
            .await
            .expect("read the pending queued run")
            .is_none(),
        "the abandonment settled the run: nothing is left to recover"
    );
    assert!(
        !drain_ended(&world.store, &drain_id).await,
        "the live tool child's obligation withholds the end until it settles"
    );

    loser_release.cancel();
    let receipt = abandonment
        .await
        .expect("the abandonment task joins")
        .expect("the abandonment settles the run");
    assert!(
        matches!(
            receipt.terminal,
            Some(crate::store::QueuedRunTerminal::Failed { .. })
        ),
        "the abandoned run keeps failed-terminal evidence: {:?}",
        receipt.terminal
    );
    until_group_settled(&closing, &group_key).await;
    assert!(
        drain_ended(&world.store, &drain_id).await,
        "the abandoned drain wrote its end receipt"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_some(),
        "the abandoned drain wrote its ledger row"
    );

    run_sweep(&world).await;
    assert_eq!(
        cancel_origin(&child(&world.registry, &cancel_id).await),
        Some(crate::CancelOrigin::ParentEnded),
        "the abandoned drain's Cancel child is swept"
    );
}

/// **L10 — an owed end is written when the owed work settles.** A drain's
/// run fails terminally while a `closing` group of its scope owes work leased
/// to another host. The settlement is the drain's end, but the epilogue that
/// follows it withholds the receipt while that obligation stands — and a
/// durably `Failed` run is never retried, so no drain under the same id asks
/// again. Once the other host's work settles, the parent-end recovery pass
/// finds the drain's `Cancel` child naming an owner with no end, reads the
/// drain's run settled, and runs the same epilogue: the receipt and the
/// ledger row land and the child is swept. No input is enqueued and the
/// drain id is never replayed.
///
/// As in L7 the group is opened on the second host, so on the SQL tiers its
/// loser's lease is foreign and the epilogue declines at once. On the
/// in-memory tier both hosts share one controller: the obligation reads as
/// this host's running work, the epilogue parks on it, and the release ends
/// the drain there — the owed end never arises, and the law holds the same
/// end state.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_failed_drain_ends_once_its_foreign_closing_work_settles(
    prefix: &str,
    world: DrainEndWorld,
) {
    let Some(group_host) = world.group_host.clone() else {
        // No group seam on this tier's embedding — see the module doc.
        return;
    };
    let drain_id = format!("{prefix}-l10-drain");
    let cancel_id = format!("{prefix}-l10-cancel");
    register_drain_child(&world.registry, &drain_id, &cancel_id, OnParentEnd::Cancel).await;
    bind_conformance_session(&world.store, &SessionId::from(SESSION_ID)).await;
    seed_turn_input(
        &world.store,
        "a drain that fails while another host owes work",
    )
    .await;

    // A group under the drain's scope, opened and closed on the second host
    // while its loser still runs there.
    let loser_release = CancellationToken::new();
    let loser_entered = Arc::new(AtomicUsize::new(0));
    let scoped = group_host
        .scoped(admit(drain_scope(&drain_id)))
        .expect("scope the closing group's opener");
    let group_key = super::effect_group_drain::group_key(prefix, "l10");
    let mut handle = super::effect_group_drain::open(
        &scoped,
        &group_key,
        2,
        crate::LoserPolicy::RunToCompletion,
        vec![
            super::effect_group_drain::settles(0),
            gated_executor(&loser_entered, loser_release.clone()),
        ],
    )
    .await;
    let _winner = super::effect_group_drain::next(&scoped, &mut handle).await;
    super::effect_group_drain::close(&scoped, handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the group records closing");
    super::effect_group_drain::until(|| loser_entered.load(Ordering::SeqCst) == 1).await;

    // The terminal error is a turn commit over a one-node budget, as in L8.
    let epilogue_entered = Arc::new(tokio::sync::Notify::new());
    let mut runtime = drain_runtime_with_budget(
        &world,
        Arc::clone(&world.registry),
        fixed_text_provider("a turn too large to commit"),
        Vec::new(),
        crate::testing::runtime_lease_owner(),
        crate::CommitBudget::bounded(1024 * 1024, 1),
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(EpilogueSignal {
        entered: Arc::clone(&epilogue_entered),
    }));
    let drain = crate::task::spawn({
        let effect_host = Arc::clone(&world.effect_host);
        let drain_id = drain_id.clone();
        async move { drive_drain(&mut runtime, &effect_host, &drain_id).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        epilogue_entered.notified(),
    )
    .await
    .expect("the durably Failed drain reaches its end epilogue");

    // The withheld shape, as L7 reads it: declined (SQL) or parked
    // (in-memory). Either way the run is settled and the drain has not ended.
    let mut drain = Some(drain);
    let mut declined = false;
    for _ in 0..200 {
        if drain.as_ref().expect("the drain task").is_finished() {
            declined = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let failed = |result: Result<QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError>| {
        let error = result.expect_err("the drain's run failed");
        assert!(
            error.is_terminal(),
            "the drain failed terminally, not retryably: {error:?}"
        );
    };
    if declined {
        failed(
            drain
                .take()
                .expect("the drain task")
                .await
                .expect("the withheld drain task joins"),
        );
    }
    assert!(
        world
            .store
            .pending_queued_run(&SessionId::from(SESSION_ID))
            .await
            .expect("read the pending queued run")
            .is_none(),
        "the terminal error settled the run: nothing is left to retry"
    );
    assert!(
        !drain_ended(&world.store, &drain_id).await,
        "the foreign obligation withholds the settled drain's end"
    );
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_none(),
        "a withheld drain writes no ledger row"
    );

    // The other host's work settles. Nothing touches the drain.
    loser_release.cancel();
    let closing = group_host
        .effect_group_closing()
        .expect("the group host answers the closing seam");
    until_group_settled(&closing, &group_key).await;
    if let Some(drain) = drain.take() {
        failed(drain.await.expect("the released drain task joins"));
    }

    // The recovery pass writes the owed end. The worker stays alive while it
    // does: the write runs on its own task, which the worker's shutdown ends.
    let sweep = drain_sweep(&world);
    let _ = sweep
        .drive_pending_processes()
        .await
        .expect("the parent-end pass runs");
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !drain_ended(&world.store, &drain_id).await {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the settled drain's owed end is written once its closing work settles");
    assert!(
        drain_ledger_row(&world.registry, &drain_id).await.is_some(),
        "the owed end writes the ledger row"
    );

    let _ = sweep
        .drive_pending_processes()
        .await
        .expect("the parent-end sweep runs");
    assert_eq!(
        cancel_origin(&child(&world.registry, &cancel_id).await),
        Some(crate::CancelOrigin::ParentEnded),
        "the ended drain's Cancel child is swept"
    );
}

/// The probe a law parks on: signals that the drain reached its epilogue, so
/// "still running" below means "parked on the closing group's obligation",
/// not "still running the turn".
struct EpilogueSignal {
    entered: Arc<tokio::sync::Notify>,
}

impl crate::RuntimeTurnPhaseProbe for EpilogueSignal {
    fn begin(&self, _phase: crate::RuntimeTurnPhase) {}

    fn end(&self, _phase: crate::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == "queue_drain.parent_end" {
            self.entered.notify_one();
        }
    }
}

/// Poll the durable lifecycle until the group is settled.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the released group settles"
)]
async fn until_group_settled(closing: &Arc<dyn StoreEffectGroupClosing>, group_key: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            // `None` is the settled answer on tiers whose settle reaps the
            // group state outright (the native supervisor removes the table
            // entry at step 4); the SQL tiers keep the row and answer
            // `Settled`.
            if matches!(
                closing.read_group_lifecycle(group_key).await,
                Ok(None | Some(EffectGroupLifecycle::Settled { .. }))
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the closing group settles");
}

/// The releasable loser the L7 group runs: records entry, then parks on the
/// release token so the group's `closing` report stays outstanding.
fn gated_executor(
    entered: &Arc<AtomicUsize>,
    release: CancellationToken,
) -> crate::RuntimeEffectLocalExecutor<'static> {
    let entered = Arc::clone(entered);
    crate::RuntimeEffectLocalExecutor::testing(move |_| {
        let entered = Arc::clone(&entered);
        let release = release.clone();
        async move {
            entered.fetch_add(1, Ordering::SeqCst);
            release.cancelled().await;
            Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!({"released": true}),
            })
        }
    })
}

/// Register one independently reported test per queue-drain end law.
///
/// The fixture yields `(guard, prefix, make)` where `make` is a
/// [`DrainEndWorldFactory`](crate::DrainEndWorldFactory): each law opens a
/// fresh world over the tier's durable substrate.
#[macro_export]
macro_rules! drain_end_tests {
    ($fixture:block) => {
        $crate::drain_end_tests!(@catalogue $fixture; [
            (
                a_multi_frame_drain_sweeps_children_only_at_its_own_end,
                "drain-end-multi-frame"
            ),
            (
                a_drain_end_cancels_cancel_children_and_leaves_abandon_ones,
                "drain-end-cancel-abandon"
            ),
            (
                a_crash_after_the_drain_receipt_recovers_its_ledger_row,
                "drain-end-crash-window"
            ),
            (
                an_empty_drain_writes_nothing_and_a_retried_one_ends,
                "drain-end-empty"
            ),
            (
                a_failed_drain_writes_no_end_and_its_retry_ends_it,
                "drain-end-failed-run"
            ),
            (
                an_interrupted_drain_is_ended_by_its_retry,
                "drain-end-interruption"
            ),
            (
                a_closing_group_under_the_drain_scope_withholds_its_end,
                "drain-end-protected-obligation"
            ),
            (
                a_durably_failed_drain_settles_its_closing_group_and_ends,
                "drain-end-durably-failed"
            ),
            (
                an_abandoned_drain_settles_its_closing_group_and_ends,
                "drain-end-abandoned"
            ),
            (
                a_failed_drain_ends_once_its_foreign_closing_work_settles,
                "drain-end-owed-after-failed"
            ),
        ]);
    };
    (@catalogue $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $law() {
                let (_guard, prefix, make) = $fixture;
                let _ = $label;
                $crate::registration_macro_support::$law(
                    prefix,
                    make($label).await,
                )
                .await;
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
            }
        )*
    };
}
