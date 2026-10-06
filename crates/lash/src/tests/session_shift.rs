//! Laws of the session shift's scheduling (FIG-3600, D5): one scheduled
//! shift is bounded per invocation and hands its remainder to a
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
        .kind("session-shift-laws")
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
    let core = LashCore::standard_builder(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(batching)
        .serve_test_llm_profile(
            counting_provider(Arc::clone(&calls)),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    Ok(Fixture {
        core,
        _double: double,
        calls,
    })
}

/// One scheduled shift is bounded per invocation and hands the rest to a
/// continuation request, so a session holding more pending inputs than one
/// invocation's run budget still drains — and a waiter on the ask observes
/// the chain's end, not the first leg's yield (review of #2290, HIGH-2 and
/// LOW-17). Rows committed through the store alone schedule nothing; the
/// law's own ask is the only deliberate shift, but the engine driver's
/// reconcile tick may legitimately ask again for the same rows (ADR 0104
/// O2) while no invocation holds the session's work, so sibling shift
/// chains are accounted, not assumed away.
async fn a_scheduled_shift_drains_more_runs_than_one_invocation_executes() -> Result<()> {
    const INPUTS: usize = lash_core::engine::MAX_RUNS_PER_SHIFT + 1;
    let fixture = fixture_with_batching(
        crate::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_admission(1),
    )
    .await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-run-budget").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let session_id = lash_core::SessionId::from("send-run-budget");
    let store = lash_core::runtime::live_session_view(&fixture.core.store_factory, &session_id)
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
    let request = lash_core::engine::ShiftRequestId::new("run-budget");
    engine_port.schedule_shift(&session_id, request.clone());
    // `await_shift` attaches to the shift's invocations and follows its
    // continuation legs, so the wait ends on the chain's own terminal
    // event; the timeout only detects a chain that wedged.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        engine_port.await_shift(&session_id, &request),
    )
    .await
    .expect("the shift chain ends")
    .expect("the shift is not refused");

    assert_eq!(outcome.stop, lash_core::engine::ShiftStop::Idle);
    // The waiter observes the runs of every leg in its own chain; a
    // reconcile tick's sibling chain legitimately owns the runs it
    // claimed first. Which siblings exist is not settled by the direct
    // chain's end — the reconcile tick's ask can still be in flight while the
    // last runs execute — so the law settles on the state its assertions
    // read: every shift ask it ever observes awaited once, the session's
    // ingress drained, and every input's turn counted. The timeout
    // again only detects a wedge.
    let mut observed = outcome.ran.len();
    let mut awaited = std::collections::HashSet::new();
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            for run in sibling_shift_runs(&fixture, &session_id, request.as_str()).await? {
                if !awaited.insert(run.clone()) {
                    continue;
                }
                let sibling = engine_port
                    .await_shift(&session_id, &lash_core::engine::ShiftRequestId::new(run))
                    .await
                    .expect("a sibling shift is not refused");
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
    .expect("every sibling shift chain ends and the session drains")
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

/// A core whose engine works a session no host holds open, over a session
/// catalog that records every store it hands out (FIG-3825). Each pending
/// input is its own run, so one shift admits several; the reconcile tick
/// is explicit, so the law's ask is the only shift.
struct HeldShiftFixture {
    core: LashCore,
    session: lash_core::SessionId,
    store: Arc<lash_core::testing::runtime_helpers::RecordingStore>,
}

impl HeldShiftFixture {
    async fn with_pending(session: &str, inputs: usize) -> Result<Self> {
        let catalog = Arc::new(std::sync::OnceLock::<
            Arc<lash_core::testing::runtime_helpers::RecordingDeploymentStore>,
        >::new());
        let installed = Arc::clone(&catalog);
        let backend = double_backend_over_explicit_reconcile(
            lash_restate_test::ServerConfig::default(),
            move |stores| {
                lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                    .map_session_store_factory(|inner| {
                        let recording = Arc::new(
                            lash_core::testing::runtime_helpers::RecordingDeploymentStore::over(
                                inner,
                            ),
                        );
                        let _ = installed.set(Arc::clone(&recording));
                        recording
                    })
                    .into_store_set()
            },
        )
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let core = LashCore::standard_builder(backend)
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(
                crate::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_admission(1),
            )
            .serve_test_llm_profile(
                counting_provider(Arc::clone(&calls)),
                mock_llm_profile_spec(),
            )
            .build(crate::testing::runtime_lease_owner())?;
        let session_id = lash_core::SessionId::fixture(session);
        drop(
            core.session(lash_core::SessionId::fixture(session.to_string()))
                .created()
                .await
                .open()
                .await?,
        );
        let store = catalog
            .get()
            .and_then(|catalog| catalog.store_for(&session_id))
            .expect("the open created the session's store");
        for index in 0..inputs {
            lash_core::TurnInputStore::enqueue_pending_turn_input(
                store.as_ref(),
                lash_core::PendingTurnInputDraft::new(
                    session_id.clone(),
                    lash_core::TurnInputIngress::NextTurn,
                    TurnInput::text(format!("question {index}")),
                ),
            )
            .await
            .expect("enqueue the input");
        }
        Ok(Self {
            core,
            session: session_id,
            store,
        })
    }

    /// Ask for one shift of the session and wait for it to end.
    async fn shift(&self, request: &str) -> lash_core::engine::ShiftOutcome {
        let engine_port = self.core.substrate_slot.ports().await.queued;
        let request = lash_core::engine::ShiftRequestId::new(request);
        engine_port.schedule_shift(&self.session, request.clone());
        tokio::time::timeout(
            std::time::Duration::from_secs(120),
            engine_port.await_shift(&self.session, &request),
        )
        .await
        .expect("the shift ends")
        .expect("the shift is not refused")
    }
}

/// FIG-3825: a run whose attempt failed on a live fault is retried on the
/// runtime its shift still holds, and starts from the durable session: the
/// failed attempt's residue is discarded first, as a redrive in a fresh
/// process would never see it. Every input is applied exactly once and the
/// shift runs on to every run.
async fn a_run_retried_on_a_held_runtime_starts_from_the_durable_session() -> Result<()> {
    const RUNS: usize = 2;
    let fixture = HeldShiftFixture::with_pending("shift-held-retry", RUNS).await?;
    fixture
        .store
        .fail_next_turn_terminal_commit(lash_core::StoreError::Backend(
            "the first run's commit meets a live fault".to_string(),
        ));

    let outcome = fixture.shift("held-retry").await;

    assert_eq!(outcome.stop, lash_core::engine::ShiftStop::Idle);
    assert_eq!(outcome.ran.len(), RUNS, "the retried run and the next ran");
    // Each run publishes its plugin transition, then commits its turn
    // (FIG-4857); the fault refused the first run's turn commit once.
    let commits = fixture.store.runtime_commits();
    assert!(
        fixture.store.commit_write_transaction_count() > commits.len(),
        "the first run's attempt met the fault"
    );
    assert!(
        outcome
            .ran
            .iter()
            .all(|run| matches!(run, lash_core::engine::RunOutcome::Committed { .. })),
        "every run committed: {outcome:?}"
    );
    assert_eq!(
        commits
            .iter()
            .filter(|commit| commit.outcome.is_some())
            .count(),
        RUNS,
        "each run committed its turn once: {:?}",
        commits
            .iter()
            .map(|commit| (
                commit.turn_commit.operation.key.clone(),
                commit.outcome.is_some()
            ))
            .collect::<Vec<_>>()
    );
    let session = fixture
        .core
        .session(crate::SessionId::parse("shift-held-retry").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    assert_eq!(
        session.durable().turn_input_applications().await?.len(),
        RUNS,
        "every input was applied exactly once"
    );
    Ok(())
}

/// The request ids of `session`'s shift invocations that are not legs of
/// `request`'s chain: each is the run of a sibling chain (the reconcile
/// tick asks for `reconcile:` requests). `shift-next:` invocations are
/// continuation legs and count through their chain's run, so they are
/// skipped here.
async fn sibling_shift_runs(
    fixture: &Fixture,
    session: &lash_core::SessionId,
    request: &str,
) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct ShiftKey {
        idempotency_key: Option<String>,
    }
    let double = &fixture._double;
    let session = session.as_str();
    let rows = lash_restate::RestateAdminClient::new(double.connection())
        .query_json::<ShiftKey>(&format!(
            "SELECT idempotency_key FROM sys_invocation \
             WHERE target_service_key = '{session}' AND target_handler_name = 'shift'"
        ))
        .await
        .expect("sys_invocation query");
    Ok(rows
        .into_iter()
        .filter_map(|row| row.idempotency_key)
        .filter(|key| {
            key != request && !key.starts_with(lash_core::engine::SHIFT_CONTINUATION_PREFIX)
        })
        .collect())
}

/// The engine driver's reconcile tick (ADR 0104 O2, ADR 0109 §3): a row
/// committed whose immediate delivery was lost — here committed through the
/// store alone, so its obligation is armed but nothing delivered it — is
/// executed once the tick's relay delivers the obligation.
async fn a_lost_shift_schedule_is_healed_by_the_reconcile_tick() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(crate::SessionId::parse("send-drain-sweep").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let session_id = lash_core::SessionId::from("send-drain-sweep");
    let store = lash_core::runtime::live_session_view(&fixture.core.store_factory, &session_id)
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
    .expect("the reconciled shift drains the input")
    .expect("the pending reads succeed");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

/// The other half of the same guarantee (ADR 0104 O2, review HIGH-2): a
/// core that boots over a session holding open ingress nothing scheduled
/// executes it on the engine driver's first reconcile tick, before any host
/// sends anything.
async fn a_booted_core_executes_lost_work_on_its_first_reconcile_tick() -> Result<()> {
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();
    let session_id = lash_core::SessionId::from("send-boot-sweep");
    let store = lash_core::runtime::admit_session_view(
        &backend.session_store_factory(),
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            // The session's creation recorded its config with the row; a
            // row with no head is never executed (FIG-4553).
            config: lash_core::SessionPolicy {
                model: Some(recorded_llm_profile(mock_llm_profile_spec())),
                ..lash_core::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                )
            }
            .into(),
            head: lash_core::SessionCreationHead::Config,
        },
    )
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
    let _core = LashCore::standard_builder(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(
            counting_provider(Arc::clone(&calls)),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;

    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if store
                .list_pending_turn_inputs()
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
    .expect("the first reconcile tick's shift drains the input");
    drop(double);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

/// The wedge a session delete left behind (FIG-3822): a shift asked for a
/// session whose tombstone already committed used to run its admission body
/// against the retired store forever — the body's `SessionDeleted` was
/// stamped a live derivation fault, so an attempt never reached the journaled
/// step an earlier attempt had recorded and the invocation never ended. The
/// journaled step now records the session's settled answer — the recorded
/// `Idle` its closing epoch admits, or the retirement refusal a shift that
/// cannot open the retired store at all records — and the session holds no
/// open engine work afterward.
async fn a_shift_on_a_deleted_session_answers_its_retirement() -> Result<()> {
    let fixture = fixture(1).await?;
    let session_id = lash_core::SessionId::from("send-retired");
    drop(
        fixture
            .core
            .session(crate::SessionId::parse("send-retired").expect("nonblank host identity"))
            .created()
            .await
            .open()
            .await?,
    );
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
    let request = lash_core::engine::ShiftRequestId::new("retired-shift");
    engine_port.schedule_shift(&session_id, request.clone());
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        engine_port.await_shift(&session_id, &request),
    )
    .await
    .expect("the retired session's shift is answered");
    match answer {
        Ok(outcome) => assert!(
            matches!(outcome.stop, lash_core::engine::ShiftStop::Idle),
            "a retired session's shift idles on the close's recorded epoch: {outcome:?}"
        ),
        Err(abort) => assert!(
            matches!(
                abort,
                lash_core::engine::ShiftAbort::Refused(ref error)
                    if error.code == lash_core::RuntimeErrorCode::SessionDeleted
            ),
            "a retired session's shift is refused with its retirement: {abort:?}"
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
/// `LashSession` shifts and its runs' `LashTurn` runs, as `target status`.
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

/// FIG-3881: a run's execution replayed after its session was deleted replays the
/// journal its first attempt recorded. The first attempt journals the
/// run's start marker and dies before its seal; the session's close and its
/// storage delete commit before the replay, so the replay cannot open the
/// session. It still issues the start marker and the seal, whose recorded
/// body answers the retirement, instead of ending the run where the journal
/// holds the start marker, and the run and the shift that called it finish.
async fn a_run_replayed_after_its_session_was_deleted_replays_its_journal() -> Result<()> {
    let fixture = fixture(1).await?;
    let session_id = lash_core::SessionId::from("send-closed-replay");
    drop(
        fixture
            .core
            .session(crate::SessionId::parse("send-closed-replay").expect("nonblank host identity"))
            .created()
            .await
            .open()
            .await?,
    );
    let run = lash_core::TurnId::from("closed-replay-run");
    let server = fixture._double.server();
    // The run records its start marker, then the atomic admission that seals
    // the shift (FIG-4848). The first attempt dies before the admission.
    server.crash_on(
        lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeRun {
            name: lash_restate::JournalStepKind::RecordedEffect
                .journal_name("lash:shift-admission:closed-replay#0"),
        })
        .service(lash_restate_test::TURN_DRIVER_SERVICE)
        .key(lash_restate::turn_invocation_key(
            &lash_core::engine::ShiftRequest {
                session: session_id.clone(),
                request: lash_core::engine::ShiftRequestId::new("closed-replay"),
                intended_lane: None,
            },
            0,
        ))
        .within_attempts(1),
    );
    // The close and the storage delete commit while the dead attempt's
    // replay has not started: the listener runs before the server starts it,
    // and both are the store's alone, so they need nothing from the server.
    let crashed_target = Arc::new(std::sync::Mutex::new(None));
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let factory = Arc::clone(&fixture.core.store_factory);
    let clock = fixture._double.lash_backend().clock();
    assert!(
        server.on_crash(lash_restate_test::CrashCount::new().listener_with({
            let crashed_target = Arc::clone(&crashed_target);
            let closed = Arc::clone(&closed);
            let session_id = session_id.clone();
            move |target: &str| {
                *crashed_target.lock().expect("crash target lock") = Some(target.to_owned());
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
        }))
    );

    let store = lash_core::runtime::live_session_view(&fixture.core.store_factory, &session_id)
        .await?
        .expect("an opened session has a store");
    store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text("close me mid-run"),
            )
            .with_source_key(run.as_str()),
        )
        .await
        .expect("enqueue the input");
    let engine_port = fixture.core.substrate_slot.ports().await.queued;
    engine_port.schedule_shift(
        &session_id,
        lash_core::engine::ShiftRequestId::new("closed-replay"),
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
        "the replayed run and its shift finish: deleted={}, crashes={}, open invocations {:?}, journals {:?}",
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
    // journaled and the sealing admission not, and the delete committed
    // before the replay.
    let executed = server
        .invocations()
        .into_iter()
        .find(|view| {
            Some(&view.target) == crashed_target.lock().expect("crash target lock").as_ref()
        })
        .expect("the run's execution is an invocation");
    let journal = server.journal(&executed.id).expect("the run's journal");
    let names: Vec<_> = journal
        .iter()
        .filter_map(|entry| entry.name.clone())
        .collect();
    assert!(
        names.iter().any(|name| name.contains("shift-run-start:"))
            && names.iter().any(|name| name.contains("shift-admission:")),
        "the replay issued the start marker and the sealing admission: {names:?}"
    );
    assert!(
        !matches!(&executed.last_failure, Some((570, _))),
        "the replay followed its journal: {:?}",
        executed.last_failure
    );
    assert!(server.stats().crashes >= 1, "the first attempt died");
    Ok(())
}

/// FIG-4965 / L13: an unreplicable acknowledgement keeps the old runtime's
/// writer alive. Both a fresh parent attempt and a run retry under the live
/// parent must make progress without that writer; its late write is fenced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_superseded_attempt_never_holds_its_successor_runtime_hostage() -> Result<()> {
    for replace_parent in [true, false] {
        let fixture = HeldShiftFixture::with_pending(
            if replace_parent {
                "held-parent-failover"
            } else {
                "held-run-redelivery"
            },
            0,
        )
        .await?;
        let registry = Arc::new(crate::core::held_shifts::HeldShifts::default());
        let mut parent = registry.hold(&fixture.session);
        let old_run = registry.run(&fixture.session);
        let old_handle = old_run
            .runtime(async {
                let session = fixture.core.session(fixture.session.clone()).open().await?;
                Ok::<_, EmbedError>(session.runtime.clone())
            })
            .await?;
        let (entered, reached) = tokio::sync::oneshot::channel();
        let (ack, released) = tokio::sync::oneshot::channel();
        let old_store = Arc::clone(&fixture.store);
        let old = tokio::spawn(async move {
            let writer = old_handle.writer();
            let runtime = writer.lock().await;
            let commit = lash_core::RuntimeCommit::persisted_state_for_test(runtime.state());
            entered.send(()).expect("announce the pinned writer");
            released.await.expect("the old acknowledgement is released");
            let result =
                lash_core::SessionCommitStore::commit_runtime_state(old_store.as_ref(), commit)
                    .await;
            drop(old_run);
            result
        });
        reached
            .await
            .expect("the old run reached its acknowledgement");
        if replace_parent {
            parent = registry.hold(&fixture.session);
        }
        let next_run = registry.run(&fixture.session);
        let next_handle = next_run
            .runtime(async {
                let session = fixture.core.session(fixture.session.clone()).open().await?;
                Ok::<_, EmbedError>(session.runtime.clone())
            })
            .await?;
        let writer = next_handle.writer();
        let runtime = tokio::time::timeout(std::time::Duration::from_secs(1), writer.lock())
            .await
            .expect("the successor never waits for the superseded writer");
        let commit = lash_core::RuntimeCommit::persisted_state_for_test(runtime.state());
        lash_core::SessionCommitStore::commit_runtime_state(fixture.store.as_ref(), commit).await?;
        let committed = lash_core::SessionCommitStore::load_session_head_meta(
            fixture.store.as_ref(),
            &fixture.session,
        )
        .await?
        .expect("the successor committed its head");
        drop(runtime);
        drop(next_run);
        ack.send(()).expect("release the old acknowledgement");
        let refused = old
            .await
            .expect("the old run ends")
            .expect_err("the late commit is refused");
        assert!(
            matches!(refused, lash_core::StoreError::HeadRevisionConflict { .. }),
            "L13 preserves the typed head ownership refusal: {refused:?}"
        );
        assert_eq!(
            lash_core::SessionCommitStore::load_session_head_meta(
                fixture.store.as_ref(),
                &fixture.session
            )
            .await?
            .expect("the head remains committed")
            .head_revision,
            committed.head_revision,
            "the late attempt never resurrects its head"
        );
        drop(parent);
    }
    Ok(())
}

macro_rules! session_shift_laws {
    ($engine:ident) => {
        mod $engine {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_scheduled_shift_drains_more_runs_than_one_invocation_executes() -> Result<()> {
                super::a_scheduled_shift_drains_more_runs_than_one_invocation_executes().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_run_retried_on_a_held_runtime_starts_from_the_durable_session()
            -> Result<()> {
                super::a_run_retried_on_a_held_runtime_starts_from_the_durable_session().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_lost_shift_schedule_is_healed_by_the_reconcile_tick() -> Result<()> {
                super::a_lost_shift_schedule_is_healed_by_the_reconcile_tick().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_booted_core_executes_lost_work_on_its_first_reconcile_tick() -> Result<()> {
                super::a_booted_core_executes_lost_work_on_its_first_reconcile_tick().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_shift_on_a_deleted_session_answers_its_retirement() -> Result<()> {
                super::a_shift_on_a_deleted_session_answers_its_retirement().await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_run_replayed_after_its_session_was_deleted_replays_its_journal()
            -> Result<()> {
                super::a_run_replayed_after_its_session_was_deleted_replays_its_journal().await
            }
        }
    };
}

session_shift_laws!(restate);
