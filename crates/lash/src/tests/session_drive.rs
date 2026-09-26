//! Laws of the session drive's scheduling (FIG-3600, D5): one scheduled
//! drive is bounded per invocation and hands its remainder to a
//! continuation request, and the reconcile sweep's two standing callers
//! re-ask work whose drive schedule was lost.
//!
//! Every law runs twice: on the in-process engine of the interim SQLite
//! backend, and on lash-restate's engine over the Restate server double.

use super::*;

const SEED: u64 = 0x5e55_10ad;

/// Which engine drives a law's scheduled work.
#[derive(Clone, Copy, Debug)]
enum Engine {
    /// The interim SQLite backend's in-process engine.
    Sqlite,
    /// lash-restate's engine on the Restate server double.
    Restate,
}

/// A core over the law's engine, and whatever must outlive it: the double is
/// a local that lives to the end of the law (FIG-3723).
struct Fixture {
    core: LashCore,
    _double: Option<lash_restate_test::RestateTestBackend>,
    calls: Arc<AtomicUsize>,
}

/// A provider that answers `echo: <last user text>` and counts its calls.
fn counting_provider(calls: Arc<AtomicUsize>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("session-drive-laws")
        .complete(move |request| {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response(&format!(
                    "echo: {}",
                    last_user_text(&request)
                )))
            }
        })
        .build()
        .into_handle()
}

/// The law's engine over a fresh backend.
async fn engine_backend(
    engine: Engine,
) -> (
    lash_core::Backend,
    Option<lash_restate_test::RestateTestBackend>,
) {
    match engine {
        Engine::Sqlite => (memory_backend().await.into(), None),
        Engine::Restate => {
            let double = restate_double(SEED).await;
            (double.lash_backend(), Some(double))
        }
    }
}

async fn fixture(engine: Engine, batch: usize) -> Result<Fixture> {
    fixture_with_batching(engine, crate::QueuedWorkBatchingConfig::new(batch)).await
}

async fn fixture_with_batching(
    engine: Engine,
    batching: crate::QueuedWorkBatchingConfig,
) -> Result<Fixture> {
    let (backend, double) = engine_backend(engine).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let core = LashCore::standard_builder(backend, crate::TurnBudget::Unbounded)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(batching)
        .provider(counting_provider(Arc::clone(&calls)))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    Ok(Fixture {
        core,
        _double: double,
        calls,
    })
}

/// One scheduled drive is bounded per invocation and hands the rest to a
/// continuation request, so a session holding more pending inputs than one
/// invocation's root budget still drains — and a waiter on the ask observes
/// the chain's end, not the first leg's yield (review of #2290, HIGH-2 and
/// LOW-17). Rows committed through the store alone schedule nothing; the
/// law's own ask is the only deliberate drive, but the core's boot sweep
/// may legitimately ask again for the same rows (ADR 0104 O2), so sibling
/// drive chains are accounted, not assumed away.
async fn a_scheduled_drive_drains_more_roots_than_one_invocation_runs(
    engine: Engine,
) -> Result<()> {
    const INPUTS: usize = lash_core::engine::MAX_ROOTS_PER_DRIVE + 1;
    let fixture = fixture_with_batching(
        engine,
        crate::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_claim(1),
    )
    .await?;
    let session = fixture.core.session("send-root-budget").open().await?;
    let session_id = lash_core::SessionId::from("send-root-budget");
    let store = fixture
        .core
        .store_factory
        .open_existing_store_by_id(&session_id)
        .await?
        .expect("an opened session has a store");
    for index in 0..INPUTS {
        store
            .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text(format!("question {index}")),
            ))
            .await
            .expect("enqueue the input");
    }
    let engine_port = fixture.core.substrate_slot.ports().await.queued;
    let request = lash_core::engine::DriveRequestId::new("root-budget");
    engine_port.schedule_drive(&session_id, request.clone());
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        engine_port.await_drive(&session_id, &request),
    )
    .await
    .expect("the drive chain ends")
    .expect("the drive is not refused");

    assert_eq!(outcome.stop, lash_core::engine::DriveStop::Idle);
    let mut observed = outcome.ran.len();
    if matches!(engine, Engine::Restate) {
        // The waiter observes the roots of every leg in its own chain; a
        // reconcile sweep's sibling chain legitimately owns the roots it
        // claimed first. Await those chains before the global assertions:
        // the direct chain can report Idle while a sibling still runs the
        // roots it claimed, so completion is only settled once every
        // sibling's await resolves.
        for root in sibling_drive_roots(&fixture, &session_id, request.as_str()).await? {
            let sibling = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                engine_port.await_drive(&session_id, &lash_core::engine::DriveRequestId::new(root)),
            )
            .await
            .expect("a sibling drive chain ends")
            .expect("a sibling drive is not refused");
            observed += sibling.ran.len();
        }
    }
    assert!(
        session.durable().pending_turn_inputs().await?.is_empty(),
        "the chain drained the session"
    );
    assert_eq!(
        session.durable().turn_input_applications().await?.len(),
        INPUTS,
        "every pending input was applied"
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), INPUTS);
    if matches!(engine, Engine::Restate) {
        assert_eq!(
            observed, INPUTS,
            "the waiter observes every leg of its chain; sibling chains own the rest"
        );
    }
    Ok(())
}

/// The request ids of `session`'s drive invocations that are not legs of
/// `request`'s chain: each is the root of a sibling chain (the reconcile
/// sweep asks for `reconcile:` requests). `drive-next:` invocations are
/// continuation legs and count through their chain's root, so they are
/// skipped here. On the SQLite engine there is no invocation journal to
/// query and no sibling assertion to feed.
async fn sibling_drive_roots(
    fixture: &Fixture,
    session: &lash_core::SessionId,
    request: &str,
) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct DriveKey {
        idempotency_key: Option<String>,
    }
    let Some(double) = fixture._double.as_ref() else {
        return Ok(Vec::new());
    };
    let session = session.as_str();
    let rows = lash_restate::RestateAdminClient::new(double.connection())
        .query_json::<DriveKey>(&format!(
            "SELECT idempotency_key FROM sys_invocation \
             WHERE target_service_key = '{session}' AND target_handler_name = 'drive'"
        ))
        .await
        .expect("sys_invocation query");
    Ok(rows
        .into_iter()
        .filter_map(|row| row.idempotency_key)
        .filter(|key| {
            key != request && !key.starts_with(lash_core::engine::DRIVE_CONTINUATION_PREFIX)
        })
        .collect())
}

/// The reconcile sweep's standing caller besides boot (ADR 0104 O2, review
/// HIGH-2): a row committed whose drive ask was lost — here committed
/// through the store alone so no ask was ever made — is driven by the next
/// `drain_status`.
async fn a_drain_status_sweeps_sessions_whose_drive_schedule_was_lost(
    engine: Engine,
) -> Result<()> {
    let fixture = fixture(engine, 1).await?;
    let session = fixture.core.session("send-drain-sweep").open().await?;
    let session_id = lash_core::SessionId::from("send-drain-sweep");
    let store = fixture
        .core
        .store_factory
        .open_existing_store_by_id(&session_id)
        .await?
        .expect("an opened session has a store");
    store
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
            session_id.clone(),
            lash_core::TurnInputIngress::NextTurn,
            TurnInput::text("heal the lost ask"),
        ))
        .await
        .expect("enqueue the input");

    fixture.core.drain_status(false).await?;

    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if session.durable().pending_turn_inputs().await?.is_empty() {
                return Ok::<_, EmbedError>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the reconciled drive drains the input")
    .expect("the pending reads succeed");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

/// The reconcile sweep's other caller (ADR 0104 O2, review HIGH-2): a core
/// that boots over a session holding open ingress nothing scheduled asks
/// for the drive itself, before any host sends anything.
async fn a_core_boot_sweeps_sessions_whose_drive_schedule_was_lost(engine: Engine) -> Result<()> {
    let (backend, double) = engine_backend(engine).await;
    let session_id = lash_core::SessionId::from("send-boot-sweep");
    let store = backend
        .session_store_factory()
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded),
        })
        .await?;
    store
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
            session_id.clone(),
            lash_core::TurnInputIngress::NextTurn,
            TurnInput::text("wake me at boot"),
        ))
        .await
        .expect("enqueue the input");

    let calls = Arc::new(AtomicUsize::new(0));
    let _core = LashCore::standard_builder(backend, crate::TurnBudget::Unbounded)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .provider(counting_provider(Arc::clone(&calls)))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if store
                .list_pending_turn_inputs(&session_id)
                .await
                .expect("list pending")
                .is_empty()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the boot sweep's drive drains the input");
    drop(double);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

macro_rules! session_drive_laws {
    ($engine:ident, $engine_variant:expr) => {
        mod $engine {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_scheduled_drive_drains_more_roots_than_one_invocation_runs() -> Result<()> {
                super::a_scheduled_drive_drains_more_roots_than_one_invocation_runs($engine_variant)
                    .await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_drain_status_sweeps_sessions_whose_drive_schedule_was_lost() -> Result<()> {
                super::a_drain_status_sweeps_sessions_whose_drive_schedule_was_lost($engine_variant)
                    .await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_core_boot_sweeps_sessions_whose_drive_schedule_was_lost() -> Result<()> {
                super::a_core_boot_sweeps_sessions_whose_drive_schedule_was_lost($engine_variant)
                    .await
            }
        }
    };
}

session_drive_laws!(sqlite, Engine::Sqlite);
session_drive_laws!(restate, Engine::Restate);
