//! Acceptance-before-drive laws for direct turns (ADR 0069).
//!
//! Every turn enters through one durable acceptance commit and is then driven,
//! so these belong to the store contract rather than to one backend's tests: a
//! backend that admits a direct turn without recording it, or records it in a
//! shape its own drains cannot recover, has a different ingress from its
//! siblings.
//!
//! The suites run a real runtime turn over the supplied durable store and read
//! it back only through surfaces every backend already owes:
//! `list_pending_turn_inputs`, `list_turn_input_applications`, and
//! `cancel_pending_turn_input`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The session every conformance store in this suite is exercised under.
const SESSION_ID: &str = "root";

fn text_response(text: &str) -> crate::LlmResponse {
    crate::LlmResponse {
        full_text: text.to_string(),
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

async fn acceptance_runtime(
    store: &Arc<dyn crate::RuntimePersistence>,
    effect_host: &Arc<dyn crate::EffectHost>,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    lease_owner: crate::LeaseOwnerIdentity,
) -> crate::LashRuntime {
    let mut host = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    host.control.effect_host = Arc::clone(effect_host);
    host.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(provider));
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(SESSION_ID.to_string());
    let state = crate::RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    Box::pin(
        crate::LashRuntime::builder(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
            lease_owner,
        )
        .with_session_id(SESSION_ID)
        .with_policy(policy)
        .with_initial_state(state)
        .with_runtime_host(host)
        .with_plugin_factories(
            crate::testing::test_standard_protocol_factories()
                .into_iter()
                .chain(plugin_factories)
                .collect(),
        )
        .with_store(Arc::clone(store))
        .build(),
    )
    .await
    .expect("build the direct-turn acceptance conformance runtime")
}

fn direct_input(turn_id: &str, text: &str) -> crate::TurnInput {
    let mut input = crate::TurnInput::text(text);
    input.trace_turn_id = Some(turn_id.to_string());
    input
}

/// A direct turn commits its input as admission evidence *before* it executes,
/// and settles that exact row when it commits.
///
/// The settlement is what a backend can be held to: the acceptance identity the
/// handle reports names a row that settles as this turn's canonical input, the
/// committed conversation attributes the model-visible message to that row, and
/// nothing is left pending. A backend that drove the caller's copy of the words
/// instead of the accepted row settles no application for it.
///
/// Mid-drive the session offers *no* claimable input, because the accepted row
/// is held by this turn's own claim — the ordinary pending listing deliberately
/// hides rows a live claim owns. (The complementary ordering proof, that the row
/// is durable before anything executes, is
/// [`orphaned_direct_turn_input_is_drivable_by_another_worker`], where the drive
/// aborts before committing and the row is still there.)
pub async fn direct_turn_accepts_before_driving(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = format!("{prefix}-accept-before-drive");
    let probe = Arc::new(std::sync::Mutex::new(None));
    let provider = {
        let store = Arc::clone(&store);
        let probe = Arc::clone(&probe);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |_| {
                let store = Arc::clone(&store);
                let probe = Arc::clone(&probe);
                async move {
                    // The turn is executing right now, so whatever this reads
                    // was already true before the drive began.
                    let pending = store
                        .list_pending_turn_inputs(SESSION_ID)
                        .await
                        .expect("read the session's pending inputs mid-drive");
                    *probe.lock().expect("probe lock") = Some(pending.len());
                    Ok(text_response("accepted"))
                }
            })
            .build()
            .into_handle()
    };
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::InlineEffectHost::default());
    let mut runtime = acceptance_runtime(
        &store,
        &effect_host,
        provider,
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let scope = effect_host
        .scoped(crate::ExecutionScope::turn(SESSION_ID, &turn_id))
        .expect("scope the direct acceptance turn");
    let turn = runtime
        .stream_turn(
            direct_input(&turn_id, "direct turn under durable acceptance"),
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
        )
        .await
        .expect("run the direct acceptance conformance turn");

    let pending_count = probe
        .lock()
        .expect("probe lock")
        .expect("the provider must have run");
    assert_eq!(
        pending_count, 0,
        "the input a direct turn is driving is held by its own claim, so nothing is claimable \
         while it runs"
    );

    let acceptance = turn
        .turn_input_acceptance
        .as_ref()
        .expect("a store-backed direct turn exposes its acceptance identity");
    let input_id = acceptance.input_id.clone();
    assert_eq!(acceptance.session_id, SESSION_ID);
    assert_eq!(
        acceptance.source_key, None,
        "direct ingress mints no idempotency key of its own"
    );
    assert_eq!(acceptance.ingress, crate::TurnInputIngress::next_turn());

    let application = store
        .list_turn_input_applications(SESSION_ID)
        .await
        .expect("read settled applications")
        .into_iter()
        .find(|application| application.input_id == input_id)
        .expect("the accepted row settles as canonical conversation input");
    assert_eq!(application.turn_id.as_str(), turn_id);
    assert!(
        store
            .list_pending_turn_inputs(SESSION_ID)
            .await
            .expect("read pending inputs")
            .iter()
            .all(|pending| pending.input_id != input_id),
        "a committed turn leaves no pending acceptance behind"
    );

    // The model-visible message the turn ran on is attributed to the accepted
    // row, so the drive consumed the acceptance rather than the caller's copy
    // of the same words.
    assert!(
        turn.state
            .read_view()
            .messages()
            .iter()
            .any(|message| matches!(
                &message.origin,
                Some(crate::MessageOrigin::TurnInput { input_id: Some(id), .. }) if *id == input_id
            )),
        "the committed conversation must attribute its user input to the accepted row"
    );
}

/// An accepted direct-turn input whose first driver never committed is
/// rediscoverable, claimable, and drivable by an unrelated worker.
///
/// The first driver aborts after its claim, leaving that claim pinned to a
/// session-lease generation that no longer holds the lane — the state a killed
/// worker leaves behind. The successor claims it under ADR 0029's generation
/// fence with no repair step, no TTL, and no knowledge that the input was ever
/// direct.
pub async fn orphaned_direct_turn_input_is_drivable_by_another_worker(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = format!("{prefix}-orphaned-direct-turn");
    let abort_plugin: Arc<dyn crate::facade_support::PluginFactory> =
        Arc::new(crate::plugin::StaticPluginFactory::new(
            "conformance-direct-turn-abort",
            crate::facade_support::PluginSpec::new().with_before_turn(Arc::new(|_ctx| {
                Box::pin(async move {
                    Err(crate::PluginError::Invoke(
                        "conformance abort before the first driver commits".to_string(),
                    ))
                })
            })),
        ));
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::InlineEffectHost::default());
    let mut first_driver = acceptance_runtime(
        &store,
        &effect_host,
        fixed_text_provider("never reached"),
        vec![abort_plugin],
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let scope = effect_host
        .scoped(crate::ExecutionScope::turn(SESSION_ID, &turn_id))
        .expect("scope the abandoned direct turn");
    let failure = first_driver
        .stream_turn(
            direct_input(&turn_id, "input the first driver never commits"),
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
        )
        .await
        .expect_err("the first driver must abort before committing");
    assert_eq!(failure.code, crate::RuntimeErrorCode::PluginPrepareTurn);
    let orphaned = store
        .list_pending_turn_inputs(SESSION_ID)
        .await
        .expect("read pending inputs after the abort");
    let input_id = orphaned
        .first()
        .expect("an abandoned direct turn leaves its acceptance durable and rediscoverable")
        .input_id
        .clone();
    drop(first_driver);

    // The successor is a different worker: its own lease owner, its own
    // runtime, and no handle on the future that accepted the input.
    let mut successor = acceptance_runtime(
        &store,
        &effect_host,
        fixed_text_provider("recovered by another worker"),
        Vec::new(),
        crate::LeaseOwnerIdentity::opaque(
            format!("{prefix}-successor-owner"),
            format!("{prefix}-successor-incarnation"),
        ),
    )
    .await;
    let drain_id = format!("{prefix}-successor-drain");
    let drain_scope = effect_host
        .scoped(crate::ExecutionScope::turn(SESSION_ID, &drain_id))
        .expect("scope the successor drain");
    let drain = successor
        .stream_next_queued_work(crate::TurnOptions::new(
            tokio_util::sync::CancellationToken::new(),
            drain_scope,
        ))
        .await
        .expect("the successor drain must run");
    let recovered = match drain {
        crate::QueuedTurnDrain::Ran(turn) => turn,
        crate::QueuedTurnDrain::Empty(reason) => panic!(
            "an orphaned direct-turn acceptance must be claimable by any worker; drain was empty: \
             {reason:?}"
        ),
    };
    assert!(
        matches!(recovered.outcome, crate::TurnOutcome::Finished(_)),
        "the successor must commit a complete turn: {:?}",
        recovered.outcome
    );

    let application = store
        .list_turn_input_applications(SESSION_ID)
        .await
        .expect("read settled applications after recovery")
        .into_iter()
        .find(|application| application.input_id == input_id)
        .expect("the recovered input settles as canonical input of the successor's turn");
    assert_ne!(
        application.turn_id.as_str(),
        turn_id,
        "the successor commits its own turn, not the abandoned driver's"
    );
    assert!(
        store
            .list_pending_turn_inputs(SESSION_ID)
            .await
            .expect("read pending inputs after recovery")
            .iter()
            .all(|pending| pending.input_id != input_id),
        "recovery settles the row rather than leaving it claimable forever"
    );
}

/// Direct ingress inherits queued identity exactly: two direct turns carrying
/// the same content are two admissions, because neither named an identity Lash
/// could recognise them by.
pub async fn direct_turn_acceptance_mints_no_idempotency_key(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let seen = Arc::new(AtomicUsize::new(0));
    let provider = {
        let seen = Arc::clone(&seen);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |_| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Ok(text_response("ok"))
                }
            })
            .build()
            .into_handle()
    };
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::InlineEffectHost::default());
    let mut runtime = acceptance_runtime(
        &store,
        &effect_host,
        provider,
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let mut acceptances = Vec::new();
    for round in 0..2 {
        let turn_id = format!("{prefix}-resubmit-{round}");
        let scope = effect_host
            .scoped(crate::ExecutionScope::turn(SESSION_ID, &turn_id))
            .expect("scope a resubmitted direct turn");
        let turn = runtime
            .stream_turn(
                direct_input(&turn_id, "the very same words"),
                crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
            )
            .await
            .expect("run a resubmitted direct turn");
        acceptances.push(
            turn.turn_input_acceptance
                .expect("a store-backed direct turn exposes its acceptance identity"),
        );
    }
    assert_eq!(seen.load(Ordering::SeqCst), 2, "both submissions execute");
    assert_ne!(
        acceptances[0].input_id, acceptances[1].input_id,
        "identical content is two admissions, not one deduplicated retry"
    );
    assert!(
        acceptances
            .iter()
            .all(|acceptance| acceptance.source_key.is_none()),
        "direct ingress never invents a source key on the caller's behalf"
    );
}

/// A direct turn may not accept or drive while another owner holds the session
/// execution lane (ADR 0077).
///
/// Refusal precedes provider execution and durable input acceptance. After the
/// holder releases the lane, the identical input can be admitted and driven by
/// a successor exactly once.
pub async fn busy_execution_lane_refuses_direct_turn_before_acceptance(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = format!("{prefix}-busy-lane-refusal");
    let successor_owner = crate::LeaseOwnerIdentity::opaque(
        format!("{prefix}-successor-owner"),
        format!("{prefix}-successor-incarnation"),
    );
    let successor_lease =
        crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
            store.as_ref(),
            SESSION_ID,
            &successor_owner,
            &format!("{prefix}-successor-executor"),
            60_000,
        )
        .await
        .expect("the successor claims the session execution lease")
        .acquired()
        .expect("the session execution lease is free in this law");

    let drives = Arc::new(AtomicUsize::new(0));
    let provider = {
        let drives = Arc::clone(&drives);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |_| {
                let drives = Arc::clone(&drives);
                async move {
                    drives.fetch_add(1, Ordering::SeqCst);
                    Ok(text_response("the admitted successor commits these words"))
                }
            })
            .build()
            .into_handle()
    };

    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::InlineEffectHost::default());
    let mut loser = acceptance_runtime(
        &store,
        &effect_host,
        provider.clone(),
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let scope = effect_host
        .scoped(crate::ExecutionScope::turn(SESSION_ID, &turn_id))
        .expect("scope the refused direct turn");
    let failure = loser
        .stream_turn(
            direct_input(&turn_id, "words admitted only after takeover"),
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
        )
        .await
        .expect_err("a direct turn cannot bypass a live execution lane");
    assert_eq!(
        failure.code,
        crate::RuntimeErrorCode::SessionExecutionLaneBusy,
        "the refusal names the live lane: {failure:?}"
    );
    assert_eq!(
        drives.load(Ordering::SeqCst),
        0,
        "lane refusal must precede provider execution"
    );
    let pending = store
        .list_pending_turn_inputs(SESSION_ID)
        .await
        .expect("read pending inputs after lane refusal");
    assert!(
        pending.is_empty(),
        "lane refusal must precede durable input acceptance: {pending:?}"
    );
    crate::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &successor_lease.completion(),
    )
    .await
    .expect("release the successor's session execution lease");

    let mut successor =
        acceptance_runtime(&store, &effect_host, provider, Vec::new(), successor_owner).await;
    let scope = effect_host
        .scoped(crate::ExecutionScope::turn(SESSION_ID, &turn_id))
        .expect("scope the successor direct turn");
    successor
        .stream_turn(
            direct_input(&turn_id, "words admitted only after takeover"),
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
        )
        .await
        .expect("the successor admits and drives after release");
    assert_eq!(
        drives.load(Ordering::SeqCst),
        1,
        "the provider runs exactly once after takeover"
    );
}

/// Unclaimed settlement is a conditional write on every backend (ADR 0069 §5).
///
/// A turn that drove the acceptance it minted may settle that row without
/// holding a claim on it, fenced by the head CAS. That makes the settlement a
/// predicate, not a blind update, and the predicate has to be *observable*: a
/// settlement that matched no row must surface as a typed supersession error
/// rather than as a silent success. Backends that discard their affected-row
/// count report "settled" for work they never did, which is precisely the defect
/// this law exists to catch.
///
/// Three rows, three predicates:
///
/// * an open, unclaimed row settles, and the row is gone afterwards;
/// * a row a live claim owns fails the `claim IS NULL` half, because an
///   unclaimed settlement may never reach through another driver's fence;
/// * a cancelled row fails the terminal-state half, because a withdrawn
///   admission is not settleable by the turn that once drove it.
///
/// The losing settlements carry no lease generation, so they are never dropped
/// and retried the way a superseded *claimed* settlement is: they are their own
/// error, and the driver that raises one retires at its first commit attempt.
pub async fn unclaimed_turn_input_settlement_is_a_conditional_write(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let mut state = crate::RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let accept = async |text: String| {
        crate::store::TurnInputStore::enqueue_pending_turn_input(
            store.as_ref(),
            crate::PendingTurnInputDraft::new(
                SESSION_ID,
                crate::TurnInputIngress::next_turn(),
                crate::TurnInput::text(text),
            ),
        )
        .await
        .expect("accept a turn input for the unclaimed-settlement law")
    };
    let unclaimed = |input: &crate::PendingTurnInput| crate::TurnInputCompletion {
        session_id: SESSION_ID.to_string(),
        claim: None,
        data: crate::TurnInputCompletionData {
            input_ids: vec![input.input_id.clone()],
            applications: Vec::new(),
        },
    };

    // (a) A row another driver's claim owns is out of reach: the claim half of
    // the predicate is what stops a lane-less turn from settling through a live
    // fence.
    let claimed = accept(format!(
        "{prefix}: a claimed row is not settleable unclaimed"
    ))
    .await;
    let lease = crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        SESSION_ID,
        &crate::testing::runtime_lease_owner(),
        "unclaimed-settlement-law-executor",
        60_000,
    )
    .await
    .expect("claim the session execution lease")
    .acquired()
    .expect("the session execution lease is free in this law");
    let claim = crate::store::TurnInputStore::claim_next_turn_inputs(
        store.as_ref(),
        SESSION_ID,
        &lease.fence(),
        &crate::testing::runtime_lease_owner(),
        10,
    )
    .await
    .expect("claim the next turn inputs")
    .expect("the accepted row is claimable");
    assert!(
        claim
            .inputs
            .iter()
            .any(|input| input.input_id == claimed.input_id)
    );
    let err = crate::store::SessionCommitStore::commit_runtime_state(
        store.as_ref(),
        crate::store::RuntimeCommit::persisted_state_for_test(&state, &[])
            .completing_turn_input_claim(unclaimed(&claimed)),
    )
    .await
    .expect_err("an unclaimed settlement must not reach through a live claim");
    assert!(
        matches!(
            err,
            crate::store::StoreError::UnclaimedTurnInputSettlementSuperseded { .. }
        ),
        "a lost unclaimed settlement is its own typed error, not a claim supersession \
         and never a silent success: {err:?}"
    );
    crate::store::TurnInputStore::abandon_turn_input_claim(store.as_ref(), &claim)
        .await
        .expect("abandon the claim");
    crate::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &lease.completion(),
    )
    .await
    .expect("release the session execution lease");

    // (b) A withdrawn admission is terminal. The turn that accepted it does not
    // get to settle it anyway.
    let cancelled = accept(format!(
        "{prefix}: a cancelled row is not settleable unclaimed"
    ))
    .await;
    crate::store::TurnInputStore::cancel_pending_turn_input(
        store.as_ref(),
        SESSION_ID,
        &cancelled.input_id,
    )
    .await
    .expect("cancel the acceptance");
    let err = crate::store::SessionCommitStore::commit_runtime_state(
        store.as_ref(),
        crate::store::RuntimeCommit::persisted_state_for_test(&state, &[])
            .completing_turn_input_claim(unclaimed(&cancelled)),
    )
    .await
    .expect_err("an unclaimed settlement must not resurrect a cancelled admission");
    assert!(
        matches!(
            err,
            crate::store::StoreError::UnclaimedTurnInputSettlementSuperseded { .. }
        ),
        "a terminal row loses the unclaimed predicate with the same typed error: {err:?}"
    );

    // (c) The open row settles, and settling it is the only thing that removes
    // it: the driver that accepted it is the driver that retired it. It runs
    // last because it is the only commit here that publishes, and a published
    // commit moves the head every later commit would have to be rebased onto.
    let open = accept(format!("{prefix}: an open acceptance settles unclaimed")).await;
    crate::store::SessionCommitStore::commit_runtime_state(
        store.as_ref(),
        crate::store::RuntimeCommit::persisted_state_for_test(&state, &[])
            .completing_turn_input_claim(unclaimed(&open)),
    )
    .await
    .expect("an unclaimed settlement of an open row commits");
    assert!(
        crate::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), SESSION_ID)
            .await
            .expect("list pending inputs after the unclaimed settlement")
            .iter()
            .all(|pending| pending.input_id != open.input_id),
        "an unclaimed settlement retires the row it named"
    );

    // (d) A settled row is terminal in the other direction: the state the
    // replay path meets. A replayed acceptance whose turn already committed
    // finds its own row `Completed`, and settling it a second time must lose
    // the same way a cancelled row does — at-most-once settlement is what stops
    // a redrive from writing a second durable record.
    state.head_revision += 1;
    let err = crate::store::SessionCommitStore::commit_runtime_state(
        store.as_ref(),
        crate::store::RuntimeCommit::persisted_state_for_test(&state, &[])
            .completing_turn_input_claim(unclaimed(&open)),
    )
    .await
    .expect_err("an unclaimed settlement must not settle an already-settled row twice");
    assert!(
        matches!(
            err,
            crate::store::StoreError::UnclaimedTurnInputSettlementSuperseded { .. }
        ),
        "a completed row loses the unclaimed predicate with the same typed error: {err:?}"
    );
}
