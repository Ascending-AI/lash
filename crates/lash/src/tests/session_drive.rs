//! Laws of the session drive's scheduling (FIG-3600, D5): one scheduled
//! drive is bounded per invocation and hands its remainder to a
//! continuation request, and the engine driver's reconcile tick relays the
//! ingress obligation of work whose immediate delivery was lost (ADR 0109
//! §3).
//!
//! Every law runs on lash-restate's engine over the Restate server double.

use super::*;

const SEED: u64 = 0x5e55_10ad;

/// A core over the double's engine, and the double, which must outlive it:
/// a core built over `double.lash_backend()` does not hold it (FIG-3723).
struct Fixture {
    core: LashCore,
    _double: lash_restate_test::RestateTestBackend,
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

async fn fixture(batch: usize) -> Result<Fixture> {
    fixture_with_batching(crate::QueuedWorkBatchingConfig::new(batch)).await
}

async fn fixture_with_batching(batching: crate::QueuedWorkBatchingConfig) -> Result<Fixture> {
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();
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
/// law's own ask is the only deliberate drive, but the engine driver's
/// reconcile tick may legitimately ask again for the same rows (ADR 0104
/// O2) while no invocation holds the session's work, so sibling drive
/// chains are accounted, not assumed away.
async fn a_scheduled_drive_drains_more_roots_than_one_invocation_runs() -> Result<()> {
    const INPUTS: usize = lash_core::engine::MAX_ROOTS_PER_DRIVE + 1;
    let fixture = fixture_with_batching(
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
    // `await_drive` attaches to the drive's invocations and follows its
    // continuation legs, so the wait ends on the chain's own terminal
    // event; the timeout only detects a chain that wedged.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        engine_port.await_drive(&session_id, &request),
    )
    .await
    .expect("the drive chain ends")
    .expect("the drive is not refused");

    assert_eq!(outcome.stop, lash_core::engine::DriveStop::Idle);
    // The waiter observes the roots of every leg in its own chain; a
    // reconcile tick's sibling chain legitimately owns the roots it
    // claimed first. Which siblings exist is not settled by the direct
    // chain's end — the reconcile tick's ask can still be in flight while the
    // last roots run — so the law settles on the state its assertions
    // read: every drive ask it ever observes awaited once, the session's
    // ingress drained, and every input's turn counted. The timeout
    // again only detects a wedge.
    let mut observed = outcome.ran.len();
    let mut awaited = std::collections::HashSet::new();
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            for root in sibling_drive_roots(&fixture, &session_id, request.as_str()).await? {
                if !awaited.insert(root.clone()) {
                    continue;
                }
                let sibling = engine_port
                    .await_drive(&session_id, &lash_core::engine::DriveRequestId::new(root))
                    .await
                    .expect("a sibling drive is not refused");
                observed += sibling.ran.len();
            }
            if session.durable().pending_turn_inputs().await?.is_empty()
                && fixture.calls.load(Ordering::SeqCst) == INPUTS
            {
                return Ok::<_, EmbedError>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("every sibling drive chain ends and the session drains")
    .expect("the pending reads succeed");
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
    assert_eq!(
        observed, INPUTS,
        "the waiter observes every leg of its chain; sibling chains own the rest"
    );
    Ok(())
}

/// The request ids of `session`'s drive invocations that are not legs of
/// `request`'s chain: each is the root of a sibling chain (the reconcile
/// tick asks for `reconcile:` requests). `drive-next:` invocations are
/// continuation legs and count through their chain's root, so they are
/// skipped here.
async fn sibling_drive_roots(
    fixture: &Fixture,
    session: &lash_core::SessionId,
    request: &str,
) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct DriveKey {
        idempotency_key: Option<String>,
    }
    let double = &fixture._double;
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

/// The engine driver's reconcile tick (ADR 0104 O2, ADR 0109 §3): a row
/// committed whose immediate delivery was lost — here committed through the
/// store alone, so its obligation is armed but nothing delivered it — is
/// driven once the tick's relay delivers the obligation.
async fn a_lost_drive_schedule_is_healed_by_the_reconcile_tick() -> Result<()> {
    let fixture = fixture(1).await?;
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

/// The other half of the same guarantee (ADR 0104 O2, review HIGH-2): a
/// core that boots over a session holding open ingress nothing scheduled
/// drives it on the engine driver's first reconcile tick, before any host
/// sends anything.
async fn a_booted_core_drives_lost_work_on_its_first_reconcile_tick() -> Result<()> {
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();
    let session_id = lash_core::SessionId::from("send-boot-sweep");
    let store = backend
        .session_store_factory()
        .create_store(&lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
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
    .expect("the first reconcile tick's drive drains the input");
    drop(double);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

/// The wedge a session delete left behind (FIG-3822): a drive asked for a
/// session whose tombstone already committed used to run its admission body
/// against the retired store forever — the body's `SessionDeleted` was
/// stamped a live derivation fault, so an attempt never reached the journaled
/// step an earlier attempt had recorded and the invocation never ended. The
/// journaled step now records the session's settled answer — the recorded
/// `Idle` its closing epoch admits, or the retirement refusal a drive that
/// cannot open the retired store at all records — and the session holds no
/// open engine work afterward.
async fn a_drive_on_a_deleted_session_answers_its_retirement() -> Result<()> {
    let fixture = fixture(1).await?;
    let session_id = lash_core::SessionId::from("send-retired");
    drop(fixture.core.session("send-retired").open().await?);
    // The physical half of a deletion: the tombstone every store read and
    // store open of the session answers. The close's engine half is the
    // release of the session's live executions, and the law's session has
    // none to release.
    fixture
        .core
        .store_factory
        .delete_session(&session_id)
        .await
        .expect("delete the session");

    let engine_port = fixture.core.substrate_slot.ports().await.queued;
    let request = lash_core::engine::DriveRequestId::new("retired-drive");
    engine_port.schedule_drive(&session_id, request.clone());
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        engine_port.await_drive(&session_id, &request),
    )
    .await
    .expect("the retired session's drive is answered");
    match answer {
        Ok(outcome) => assert!(
            matches!(outcome.stop, lash_core::engine::DriveStop::Idle),
            "a retired session's drive idles on the close's recorded epoch: {outcome:?}"
        ),
        Err(abort) => assert!(
            matches!(
                abort,
                lash_core::engine::DriveAbort::Refused(ref error)
                    if error.code == lash_core::RuntimeErrorCode::SessionDeleted
            ),
            "a retired session's drive is refused with its retirement: {abort:?}"
        ),
    }

    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if open_session_invocations(&fixture, &session_id).is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the deleted session holds no open engine work");
    Ok(())
}

/// The lash invocations of `session` the engine has not completed: its
/// `LashSession` drives and its roots' `LashTurn` runs, as `target status`.
fn open_session_invocations(fixture: &Fixture, session: &lash_core::SessionId) -> Vec<String> {
    fixture
        ._double
        .server()
        .invocations()
        .into_iter()
        .filter(|view| {
            (view.target.starts_with("LashSession/") || view.target.starts_with("LashTurn/"))
                && view.target.contains(session.as_str())
                && view.status != "completed"
        })
        .map(|view| format!("{} {} {:?}", view.target, view.status, view.last_failure))
        .collect()
}

/// FIG-3881: a root's run replayed after its session was deleted replays the
/// journal its first attempt recorded. The first attempt journals the
/// root's start marker and dies before its seal; the session's close and its
/// storage delete commit before the replay, so the replay cannot open the
/// session. It still issues the start marker and the seal, whose recorded
/// body answers the retirement, instead of ending the run where the journal
/// holds the start marker, and the run and the drive that called it finish.
async fn a_root_replayed_after_its_session_was_deleted_replays_its_journal() -> Result<()> {
    let fixture = fixture(1).await?;
    let session_id = lash_core::SessionId::from("send-closed-replay");
    drop(fixture.core.session("send-closed-replay").open().await?);
    let root = lash_core::TurnId::from("closed-replay-root");
    let server = fixture._double.server();
    // The root's journal: its input, the generation sentinel, the start
    // marker, then the seal. The first attempt dies before the seal.
    server.crash_on(
        lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeCommand {
            index: 3,
        })
        .service(lash_restate_test::TURN_DRIVER_SERVICE)
        .key(lash_restate::turn_workflow_key(&session_id, &root))
        .within_attempts(1),
    );
    // The close and the storage delete commit while the dead attempt's
    // replay has not started: the listener runs before the server starts it,
    // and both are the store's alone, so they need nothing from the server.
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let factory = Arc::clone(&fixture.core.store_factory);
    let clock = fixture._double.lash_backend().clock();
    assert!(server.on_crash(Arc::new({
        let closed = Arc::clone(&closed);
        let session_id = session_id.clone();
        move |_target: &str| {
            let factory = Arc::clone(&factory);
            let session_id = session_id.clone();
            let at_ms = clock.timestamp_ms();
            let close = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime for the close")
                    .block_on(async {
                        factory
                            .begin_session_close(&session_id, at_ms)
                            .await
                            .expect("the close commits")
                            .expect("the session exists");
                        factory
                            .delete_session(&session_id)
                            .await
                            .expect("the storage delete commits");
                    });
            })
            .join();
            closed.store(close.is_ok(), Ordering::SeqCst);
        }
    })));

    let store = fixture
        .core
        .store_factory
        .open_existing_store_by_id(&session_id)
        .await?
        .expect("an opened session has a store");
    store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text("close me mid-root"),
            )
            .with_source_key(root.as_str()),
        )
        .await
        .expect("enqueue the input");
    let engine_port = fixture.core.substrate_slot.ports().await.queued;
    engine_port.schedule_drive(
        &session_id,
        lash_core::engine::DriveRequestId::new("closed-replay"),
    );

    let settled = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if closed.load(Ordering::SeqCst)
                && open_session_invocations(&fixture, &session_id).is_empty()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "the replayed root and its drive finish: deleted={}, crashes={}, open invocations {:?}, journals {:?}",
        closed.load(Ordering::SeqCst),
        server.stats().crashes,
        open_session_invocations(&fixture, &session_id),
        server
            .invocations()
            .into_iter()
            .filter(|view| view.target.contains(session_id.as_str()))
            .map(|view| {
                let names: Vec<_> = server
                    .journal(&view.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|entry| format!("{:?}:{:?}", entry.ty, entry.name))
                    .collect();
                format!("{} attempts={} {names:?}", view.target, view.attempts)
            })
            .collect::<Vec<_>>()
    );
    // The precondition: the first attempt died with the start marker
    // journaled and the seal not, and the delete committed before the replay.
    let run = server
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with("LashTurn/") && view.target.contains(root.as_str()))
        .expect("the root's run is an invocation");
    let journal = server.journal(&run.id).expect("the run's journal");
    let names: Vec<_> = journal
        .iter()
        .filter_map(|entry| entry.name.clone())
        .collect();
    assert!(
        names.iter().any(|name| name.contains("drive-root-start:"))
            && names.iter().any(|name| name.contains("drive-seal:")),
        "the replay issued the start marker and the seal: {names:?}"
    );
    assert!(
        !matches!(&run.last_failure, Some((570, _))),
        "the replay followed its journal: {:?}",
        run.last_failure
    );
    assert!(server.stats().crashes >= 1, "the first attempt died");
    Ok(())
}

macro_rules! session_drive_laws {
    ($engine:ident) => {
        mod $engine {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_scheduled_drive_drains_more_roots_than_one_invocation_runs() -> Result<()> {
                super::a_scheduled_drive_drains_more_roots_than_one_invocation_runs().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_lost_drive_schedule_is_healed_by_the_reconcile_tick() -> Result<()> {
                super::a_lost_drive_schedule_is_healed_by_the_reconcile_tick().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_booted_core_drives_lost_work_on_its_first_reconcile_tick() -> Result<()> {
                super::a_booted_core_drives_lost_work_on_its_first_reconcile_tick().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_drive_on_a_deleted_session_answers_its_retirement() -> Result<()> {
                super::a_drive_on_a_deleted_session_answers_its_retirement().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_root_replayed_after_its_session_was_deleted_replays_its_journal()
            -> Result<()> {
                super::a_root_replayed_after_its_session_was_deleted_replays_its_journal().await
            }
        }
    };
}

session_drive_laws!(restate);
