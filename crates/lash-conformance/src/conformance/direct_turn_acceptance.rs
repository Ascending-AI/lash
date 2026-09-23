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

use crate::admit;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The session every conformance store in this suite is exercised under.
const SESSION_ID: &str = "root";

pub(super) fn text_response(text: &str) -> crate::LlmResponse {
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

pub(super) async fn acceptance_runtime(
    store: &Arc<dyn crate::RuntimePersistence>,
    effect_host: &Arc<dyn crate::EffectHost>,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    lease_owner: crate::LeaseOwnerIdentity,
) -> crate::LashRuntime {
    acceptance_runtime_for_session(
        SESSION_ID,
        store,
        effect_host,
        provider,
        plugin_factories,
        lease_owner,
    )
    .await
}

/// [`acceptance_runtime`] over an explicit session id.
pub(super) async fn acceptance_runtime_for_session(
    session_id: &str,
    store: &Arc<dyn crate::RuntimePersistence>,
    effect_host: &Arc<dyn crate::EffectHost>,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    lease_owner: crate::LeaseOwnerIdentity,
) -> crate::LashRuntime {
    acceptance_runtime_with_batching(
        session_id,
        store,
        effect_host,
        provider,
        plugin_factories,
        lease_owner,
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .await
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn acceptance_runtime_with_batching(
    session_id: &str,
    store: &Arc<dyn crate::RuntimePersistence>,
    effect_host: &Arc<dyn crate::EffectHost>,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    lease_owner: crate::LeaseOwnerIdentity,
    batching: crate::QueuedWorkBatchingConfig,
) -> crate::LashRuntime {
    let mut host = crate::RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        batching.clone(),
    );
    host = host.with_effect_host(Arc::clone(effect_host));
    host.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(provider));
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(SessionId::from(session_id.to_string()));
    let state = crate::RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    Box::pin(
        crate::LashRuntime::builder(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            batching,
            lease_owner,
        )
        .with_session_id(session_id)
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

pub(super) fn direct_input(turn_id: &TurnId, text: &str) -> crate::TurnInput {
    let mut input = crate::TurnInput::text(text);
    input.trace_turn_id = Some(TurnId::from(turn_id.to_string()));
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
/// is held by this turn's own claim. The ordinary pending listing still returns
/// that row with the factual held status and the matching live lease's exact
/// expiry. (The complementary ordering proof, that the row is durable before
/// anything executes, is
/// [`orphaned_direct_turn_input_is_drivable_by_another_worker`], where the drive
/// aborts before committing and the row is still there.)
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn direct_turn_accepts_before_driving(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-accept-before-drive"));
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
                        .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
                        .await
                        .expect("read the session's pending inputs mid-drive");
                    let lease = store
                        .get_session_execution_lease(&SessionId::from(SESSION_ID))
                        .await
                        .expect("read the matching session lease mid-drive");
                    *probe.lock().expect("probe lock") = Some((pending, lease));
                    Ok(text_response("accepted"))
                }
            })
            .build()
            .into_handle()
    };
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let mut runtime = acceptance_runtime(
        &store,
        &effect_host,
        provider,
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let scope = effect_host
        .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &turn_id)))
        .expect("scope the direct acceptance turn");
    let turn = runtime
        .stream_turn(
            direct_input(&turn_id, "direct turn under durable acceptance"),
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
        )
        .await
        .expect("run the direct acceptance conformance turn");

    let (pending, lease_observation) = probe
        .lock()
        .expect("probe lock")
        .clone()
        .expect("the provider must have run");
    assert_eq!(pending.len(), 1, "the held input must remain visible");
    let held = &pending[0];
    let lease = lease_observation
        .lease
        .expect("the executing turn must still hold its session lease");
    assert!(
        lease_observation.observed_at_epoch_ms < lease.expires_at_epoch_ms,
        "the control read must observe a still-live lease"
    );
    assert_eq!(
        held.status,
        crate::PendingTurnInputReadStatus::Held {
            lease_expires_at_ms: lease.expires_at_epoch_ms,
        },
        "the held marker must carry the exact matching session-lease expiry"
    );
    assert_eq!(
        held.input.state,
        crate::TurnInputState::DeferredNextTurn,
        "held is a read status, not a persisted TurnInputState"
    );

    let acceptance = turn
        .turn_input_acceptance
        .as_ref()
        .expect("a store-backed direct turn exposes its acceptance identity");
    let input_id = acceptance.input_id.clone();
    assert_eq!(
        held.input.input_id, input_id,
        "the held projection must name the acceptance this turn is driving"
    );
    assert_eq!(acceptance.session_id, SESSION_ID);
    assert_eq!(
        acceptance.source_key, None,
        "direct ingress mints no idempotency key of its own"
    );
    assert_eq!(acceptance.ingress, crate::TurnInputIngress::next_turn());

    let application = store
        .list_turn_input_applications(&SessionId::from(SESSION_ID))
        .await
        .expect("read settled applications")
        .into_iter()
        .find(|application| application.input_id == input_id)
        .expect("the accepted row settles as canonical conversation input");
    assert_eq!(application.turn_id.as_str(), turn_id);
    assert!(
        store
            .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
            .await
            .expect("read pending inputs")
            .iter()
            .all(|pending| pending.input.input_id != input_id),
        "a committed turn leaves no pending acceptance behind"
    );

    // The model-visible message the turn ran on is attributed to the accepted
    // row, so the drive consumed the acceptance rather than the caller's copy
    // of the same words.
    assert!(
        turn.state
            .read_view()
            .expect("accepted turn frame scope resolves")
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
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn orphaned_direct_turn_input_is_drivable_by_another_worker(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-orphaned-direct-turn"));
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
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let mut first_driver = acceptance_runtime(
        &store,
        &effect_host,
        fixed_text_provider("never reached"),
        vec![abort_plugin],
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let scope = effect_host
        .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &turn_id)))
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
        .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
        .await
        .expect("read pending inputs after the abort");
    let input_id = orphaned
        .first()
        .expect("an abandoned direct turn leaves its acceptance durable and rediscoverable")
        .input
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
        .scoped(admit(crate::ExecutionScope::queue_drain(
            SESSION_ID, &drain_id,
        )))
        .expect("scope the successor drain");
    let drain = Box::pin(successor.stream_next_queued_work(crate::TurnOptions::new(
        tokio_util::sync::CancellationToken::new(),
        drain_scope,
    )))
    .await
    .expect("the successor drain must run");
    let recovered = match drain {
        crate::QueuedTurnDrain::Ran(turn) => turn,
        crate::QueuedTurnDrain::Replayed(_) => {
            panic!("first successor drain cannot replay a queued receipt")
        }
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
        .list_turn_input_applications(&SessionId::from(SESSION_ID))
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
            .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
            .await
            .expect("read pending inputs after recovery")
            .iter()
            .all(|pending| pending.input.input_id != input_id),
        "recovery settles the row rather than leaving it claimable forever"
    );
}

/// Direct ingress inherits queued identity exactly: two direct turns carrying
/// the same content are two admissions, because neither named an identity Lash
/// could recognise them by.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::NativeEffectHost::default());
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
        let turn_id = TurnId::from(format!("{prefix}-resubmit-{round}"));
        let scope = effect_host
            .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &turn_id)))
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
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn busy_execution_lane_refuses_direct_turn_before_acceptance(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-busy-lane-refusal"));
    let successor_owner = crate::LeaseOwnerIdentity::opaque(
        format!("{prefix}-successor-owner"),
        format!("{prefix}-successor-incarnation"),
    );
    let successor_lease =
        crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
            store.as_ref(),
            &SessionId::from(SESSION_ID),
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

    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let mut loser = acceptance_runtime(
        &store,
        &effect_host,
        provider.clone(),
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let scope = effect_host
        .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &turn_id)))
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
        .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
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
        .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &turn_id)))
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
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn unclaimed_turn_input_settlement_is_a_conditional_write(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let mut state = crate::RuntimeSessionState {
        session_id: SessionId::from(SESSION_ID.to_string()),
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
        session_id: SessionId::from(SESSION_ID.to_string()),
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
        &SessionId::from(SESSION_ID),
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
        &SessionId::from(SESSION_ID),
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
        &SessionId::from(SESSION_ID),
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
        crate::store::TurnInputStore::list_pending_turn_inputs(
            store.as_ref(),
            &SessionId::from(SESSION_ID)
        )
        .await
        .expect("list pending inputs after the unclaimed settlement")
        .iter()
        .all(|pending| pending.input.input_id != open.input_id),
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

// ---------------------------------------------------------------------------
// Journaled initial drive set (ADR 0069 §6, FIG-3532)
// ---------------------------------------------------------------------------

/// A journal-owning effect controller: the first execution of an effect runs
/// the native local executor and records its outcome under the effect's replay
/// key, and every later execution of the same key returns the recorded outcome
/// without running anything — what a durable engine does on replay.
///
/// `crash_at` simulates a worker dying at an effect: the next effect of that
/// kind fails before it runs and is never journaled, so the redrive executes
/// it for real.
#[derive(Default)]
struct JournalController {
    native: crate::NativeRuntimeEffectController,
    outcomes: std::sync::Mutex<std::collections::HashMap<String, crate::RuntimeEffectOutcome>>,
    crash_at: std::sync::Mutex<Option<crate::RuntimeEffectKind>>,
}

impl JournalController {
    #[expect(clippy::expect_used, reason = "conformance fixture lock")]
    fn crash_at_next(&self, kind: crate::RuntimeEffectKind) {
        *self.crash_at.lock().expect("crash lock") = Some(kind);
    }

    #[expect(clippy::expect_used, reason = "conformance fixture lock")]
    fn journaled_drive(&self) -> Option<crate::AcceptedTurnInputDrive> {
        self.outcomes
            .lock()
            .expect("journal lock")
            .values()
            .find_map(|outcome| match outcome {
                crate::RuntimeEffectOutcome::ClaimAcceptedTurnInput { drive } => {
                    Some(drive.clone())
                }
                _ => None,
            })
    }
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for JournalController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(format!("conformance-journal-controller:{:p}", self))
    }

    async fn prepare_completion_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<crate::CompletionKeyPreparation, crate::RuntimeError> {
        self.native
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.native.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        self.native.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.native
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.native
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for JournalController {
    #[expect(clippy::expect_used, reason = "conformance fixture lock")]
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        // Keyed by replay key, the address a durable engine journals under:
        // effect ids alone repeat across turns.
        let effect_id = envelope.invocation.replay_key().to_string();
        if let Some(outcome) = self.outcomes.lock().expect("journal lock").get(&effect_id) {
            return Ok(outcome.clone());
        }
        let kind = envelope.command.kind();
        {
            let mut crash_at = self.crash_at.lock().expect("crash lock");
            if *crash_at == Some(kind) {
                *crash_at = None;
                return Err(crate::RuntimeEffectControllerError::foreign(
                    "conformance_worker_crash",
                    format!("the worker died at the {} effect", kind.as_str()),
                ));
            }
        }
        let outcome = self.native.execute_effect(envelope, local_executor).await?;
        self.outcomes
            .lock()
            .expect("journal lock")
            .insert(effect_id, outcome.clone());
        Ok(outcome)
    }

    async fn open_effect_group(
        &self,
        group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        self.native.open_effect_group(group).await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut crate::EffectGroupHandle,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        self.native.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: crate::EffectGroupHandle,
        disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.native.close_effect_group(handle, disposition).await
    }
}

/// A replaying worker resumes from the invocation's pre-commit resident state
/// while the store may already hold the first execution's commit, so the
/// redrive runtime sees no persisted session. It also counts every read a drive
/// could make of pending rows, so a replay can prove it made none.
struct RedriveStore {
    inner: Arc<dyn crate::RuntimePersistence>,
    pending_row_reads: Arc<AtomicUsize>,
}

impl RedriveStore {
    fn wrap(inner: &Arc<dyn crate::RuntimePersistence>) -> (Arc<Self>, Arc<AtomicUsize>) {
        let reads = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                inner: Arc::clone(inner),
                pending_row_reads: Arc::clone(&reads),
            }),
            reads,
        )
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for RedriveStore {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn load_session(
        &self,
    ) -> Result<Option<crate::store::PersistedSessionRead>, crate::StoreError> {
        Ok(None)
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::PendingTurnInputRead>, crate::StoreError> {
        self.pending_row_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.list_pending_turn_inputs(session_id).await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<crate::TurnInputClaim>, crate::StoreError> {
        self.pending_row_reads.fetch_add(1, Ordering::SeqCst);
        self.inner
            .claim_next_turn_inputs(session_id, session_execution_lease, owner, max_inputs)
            .await
    }
}

/// One journal and one effect host shared by a first execution and its
/// redrive, the way a durable engine's handler keeps its journal across
/// worker incarnations.
struct Journal {
    controller: Arc<JournalController>,
    effect_host: Arc<dyn crate::EffectHost>,
    batching: crate::QueuedWorkBatchingConfig,
}

impl Journal {
    fn new() -> Self {
        let controller = Arc::new(JournalController::default());
        let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::NativeEffectHost::new(
            Arc::clone(&controller) as Arc<dyn crate::RuntimeEffectController>,
        ));
        Self {
            controller,
            effect_host,
            batching: crate::QueuedWorkBatchingConfig::new(1),
        }
    }

    /// Bound every claim this journal's runtimes take to `max_inputs` rows.
    fn with_turn_input_claim(mut self, max_inputs: usize) -> Self {
        self.batching = self.batching.with_max_turn_input_claim(max_inputs);
        self
    }

    /// Run the direct turn `turn_id` against `store` on a fresh runtime.
    async fn run(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        provider: crate::ProviderHandle,
        turn_id: &TurnId,
        text: &str,
    ) -> Result<crate::AssembledTurn, crate::RuntimeError> {
        self.run_with_plugins(store, provider, Vec::new(), turn_id, text)
            .await
    }

    /// [`Self::run`] with extra plugins on the runtime.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn run_with_plugins(
        &self,
        store: &Arc<dyn crate::RuntimePersistence>,
        provider: crate::ProviderHandle,
        plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
        turn_id: &TurnId,
        text: &str,
    ) -> Result<crate::AssembledTurn, crate::RuntimeError> {
        let mut runtime = acceptance_runtime_with_batching(
            SESSION_ID,
            store,
            &self.effect_host,
            provider,
            plugin_factories,
            crate::testing::runtime_lease_owner(),
            self.batching.clone(),
        )
        .await;
        let scope = self
            .effect_host
            .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, turn_id)))
            .expect("scope the journaled direct turn");
        runtime
            .stream_turn(
                direct_input(turn_id, text),
                crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
            )
            .await
    }
}

/// A worker that dies after its drive and before its commit: the turn aborts in
/// its prepare phase, which leaves the claim pinned and writes nothing.
fn die_before_commit_plugin() -> Arc<dyn crate::facade_support::PluginFactory> {
    Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-die-before-commit",
        crate::facade_support::PluginSpec::new().with_before_turn(Arc::new(|_ctx| {
            Box::pin(async move {
                Err(crate::PluginError::Invoke(
                    "conformance worker died before its commit".to_string(),
                ))
            })
        })),
    ))
}

/// A provider that records the text of every request it answers.
fn recording_provider(answer: &str) -> (crate::ProviderHandle, Arc<std::sync::Mutex<Vec<String>>>) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let answer = answer.to_string();
    let provider = {
        let requests = Arc::clone(&requests);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |request| {
                let requests = Arc::clone(&requests);
                let answer = answer.clone();
                async move {
                    let text = request
                        .messages
                        .iter()
                        .flat_map(|message| message.blocks.iter())
                        .filter_map(|block| match block {
                            crate::llm::types::LlmContentBlock::Text { text, .. } => {
                                Some(text.clone())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    #[expect(clippy::expect_used, reason = "conformance fixture lock")]
                    requests.lock().expect("request lock").push(text);
                    Ok(text_response(&answer))
                }
            })
            .build()
            .into_handle()
    };
    (provider, requests)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn vacuum(store: &Arc<dyn crate::RuntimePersistence>) {
    crate::store::StoreMaintenance::vacuum(store.as_ref())
        .await
        .expect("vacuum the session's terminal rows");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn pending_input_ids(store: &Arc<dyn crate::RuntimePersistence>) -> Vec<crate::InputId> {
    store
        .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
        .await
        .expect("read pending inputs")
        .into_iter()
        .map(|read| read.input.input_id)
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn applications(
    store: &Arc<dyn crate::RuntimePersistence>,
) -> Vec<crate::TurnInputApplication> {
    store
        .list_turn_input_applications(&SessionId::from(SESSION_ID))
        .await
        .expect("read settled applications")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn enqueue_next_turn(
    store: &Arc<dyn crate::RuntimePersistence>,
    text: &str,
) -> crate::PendingTurnInput {
    store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            SESSION_ID,
            crate::TurnInputIngress::next_turn(),
            crate::TurnInput::text(text),
        ))
        .await
        .expect("enqueue a next-turn input")
}

/// A drain after a replayed commit finds nothing: the redrive left no row open
/// for a second turn to answer.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_nothing_left_to_answer(
    prefix: &str,
    store: &Arc<dyn crate::RuntimePersistence>,
    journal: &Journal,
) {
    let answered = Arc::new(AtomicUsize::new(0));
    let provider = {
        let answered = Arc::clone(&answered);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |_| {
                let answered = Arc::clone(&answered);
                async move {
                    answered.fetch_add(1, Ordering::SeqCst);
                    Ok(text_response("a second answer"))
                }
            })
            .build()
            .into_handle()
    };
    let mut drainer = acceptance_runtime(
        store,
        &journal.effect_host,
        provider,
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let drain_id = format!("{prefix}-after-redrive-drain");
    let scope = journal
        .effect_host
        .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &drain_id)))
        .expect("scope the post-redrive drain");
    let drain = drainer
        .stream_next_queued_work(crate::TurnOptions::new(
            tokio_util::sync::CancellationToken::new(),
            scope,
        ))
        .await
        .expect("the post-redrive drain runs");
    assert!(
        matches!(drain, crate::QueuedTurnDrain::Empty(_)),
        "a replayed commit must leave nothing for a second turn to answer"
    );
    assert_eq!(answered.load(Ordering::SeqCst), 0);
}

/// A committed direct turn whose handler died before it was acknowledged is
/// redriven after `vacuum()` pruned its completed row. The redrive drives the
/// journaled drive set, finds the first commit's receipt, and replays it: no
/// row is re-admitted and nothing is answered twice.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn vacuum_then_redrive_replays_receipt_single_row(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-vacuum-redrive-single"));
    let journal = Journal::new();
    let (provider, requests) = recording_provider("deployed staging");
    let first = journal
        .run(&store, provider.clone(), &turn_id, "deploy staging")
        .await
        .expect("the first execution commits");
    let acceptance = first
        .turn_input_acceptance
        .clone()
        .expect("a store-backed direct turn exposes its acceptance");
    let committed = applications(&store).await;
    assert_eq!(committed.len(), 1);

    vacuum(&store).await;
    let (redrive_store, _) = RedriveStore::wrap(&store);
    let redrive_store: Arc<dyn crate::RuntimePersistence> = redrive_store;
    let replayed = journal
        .run(&redrive_store, provider, &turn_id, "deploy staging")
        .await
        .expect("the redrive replays the original commit's receipt");

    assert_eq!(
        replayed.turn_input_acceptance.as_ref(),
        Some(&acceptance),
        "the redrive keeps the journaled acceptance identity"
    );
    assert_eq!(
        requests.lock().expect("request lock").len(),
        1,
        "the provider answered the input once"
    );
    assert!(
        pending_input_ids(&store).await.is_empty(),
        "the redrive re-admitted nothing"
    );
    assert_eq!(
        applications(&store).await,
        committed,
        "the receipt replay writes no second application"
    );
    Box::pin(assert_nothing_left_to_answer(prefix, &store, &journal)).await;
}

/// The same redrive when the first execution absorbed earlier queued rows into
/// its turn. The journaled drive set carries those rows' content, so the
/// redrive materializes the same words after `vacuum()` pruned every row, and
/// the receipt replays with identical applications.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn vacuum_then_redrive_replays_receipt_absorbed_rows(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-vacuum-redrive-absorbed"));
    enqueue_next_turn(&store, "queued first").await;
    enqueue_next_turn(&store, "queued second").await;
    let journal = Journal::new();
    let (provider, requests) = recording_provider("answered all three");
    journal
        .run(&store, provider.clone(), &turn_id, "direct third")
        .await
        .expect("the first execution commits");
    let committed = applications(&store).await;
    assert_eq!(
        committed.len(),
        3,
        "the direct turn absorbed both earlier rows: {committed:?}"
    );
    assert!(
        matches!(
            journal.controller.journaled_drive(),
            Some(crate::AcceptedTurnInputDrive::Claimed { claim }) if claim.inputs.len() == 3
        ),
        "the journaled drive carries all three rows"
    );

    vacuum(&store).await;
    let (redrive_store, _) = RedriveStore::wrap(&store);
    let redrive_store: Arc<dyn crate::RuntimePersistence> = redrive_store;
    journal
        .run(&redrive_store, provider, &turn_id, "direct third")
        .await
        .expect("the redrive replays the receipt of the absorbing turn");

    assert_eq!(requests.lock().expect("request lock").len(), 1);
    assert!(pending_input_ids(&store).await.is_empty());
    assert_eq!(
        applications(&store).await,
        committed,
        "the receipt replay keeps the original applications"
    );
    Box::pin(assert_nothing_left_to_answer(prefix, &store, &journal)).await;
}

/// A worker dies after its turn's acceptance was journaled and before the
/// drive was; the host cancels the accepted input and `vacuum()` prunes it.
/// The redrive runs the drive for the first time, finds the row gone, and
/// cedes: the cancelled input is not re-admitted, not answered, and its turn
/// never reaches the provider.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn cancelled_vacuumed_acceptance_is_not_resurrected(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-cancelled-vacuumed"));
    let journal = Journal::new();
    journal
        .controller
        .crash_at_next(crate::RuntimeEffectKind::ClaimAcceptedTurnInput);
    let (provider, requests) = recording_provider("never answered");
    journal
        .run(&store, provider.clone(), &turn_id, "withdrawn later")
        .await
        .expect_err("the worker dies before the drive is journaled");
    let accepted = pending_input_ids(&store)
        .await
        .into_iter()
        .next()
        .expect("the journaled acceptance left its row open");

    let cancelled = store
        .cancel_pending_turn_input(&SessionId::from(SESSION_ID), &accepted)
        .await
        .expect("the host cancels the accepted input");
    assert!(cancelled.is_cancelled(), "{cancelled:?}");
    vacuum(&store).await;

    let (redrive_store, _) = RedriveStore::wrap(&store);
    let redrive_store: Arc<dyn crate::RuntimePersistence> = redrive_store;
    let error = journal
        .run(&redrive_store, provider, &turn_id, "withdrawn later")
        .await
        .expect_err("the redrive must not answer a cancelled input");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::AcceptedTurnInputCeded,
        "{error:?}"
    );
    assert!(
        matches!(
            journal.controller.journaled_drive(),
            Some(crate::AcceptedTurnInputDrive::Refused {
                refusal: crate::AcceptedTurnInputRefusal::SettledOrRemoved
            })
        ),
        "the redrive journals the refusal it ceded with"
    );
    assert!(
        pending_input_ids(&store).await.is_empty(),
        "the cancelled input is not re-admitted"
    );
    assert!(applications(&store).await.is_empty());
    assert!(
        requests.lock().expect("request lock").is_empty(),
        "the cancelled input never reaches the provider"
    );
}

/// A first execution that died after its drive and before its commit is
/// redriven after a new input was admitted. The redrive drives the journaled
/// set, not a live claim: the committed turn holds only the first execution's
/// rows, and the new input waits for the next turn.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn uncommitted_redrive_drives_journaled_set_not_live_claim(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-uncommitted-redrive"));
    let journal = Journal::new();
    let (provider, requests) = recording_provider("answered the journaled set");
    journal
        .run_with_plugins(
            &store,
            provider.clone(),
            vec![die_before_commit_plugin()],
            &turn_id,
            "the accepted words",
        )
        .await
        .expect_err("the worker dies before the turn commits");
    let journaled = match journal.controller.journaled_drive() {
        Some(crate::AcceptedTurnInputDrive::Claimed { claim }) => claim,
        other => panic!("the first execution claimed its accepted row: {other:?}"),
    };
    let late = enqueue_next_turn(&store, "admitted after the crash").await;

    let (redrive_store, reads) = RedriveStore::wrap(&store);
    let redrive_store: Arc<dyn crate::RuntimePersistence> = redrive_store;
    journal
        .run(&redrive_store, provider, &turn_id, "the accepted words")
        .await
        .expect("the redrive commits the journaled drive set");
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "the redrive drives the journaled set and never claims or reads a pending row"
    );

    let requests = requests.lock().expect("request lock").clone();
    assert_eq!(requests.len(), 1, "only the redrive reached the provider");
    assert!(requests[0].contains("the accepted words"), "{requests:?}");
    assert!(
        !requests[0].contains("admitted after the crash"),
        "a row admitted after the drive must not join the redriven turn: {requests:?}"
    );
    let applied = applications(&store).await;
    assert_eq!(
        applied
            .iter()
            .map(|application| application.input_id.clone())
            .collect::<Vec<_>>(),
        journaled
            .inputs
            .iter()
            .map(|input| input.input_id.clone())
            .collect::<Vec<_>>(),
        "the redrive settles exactly the journaled rows"
    );
    assert!(
        applied
            .iter()
            .all(|application| application.turn_id.as_str() == turn_id),
        "{applied:?}"
    );
    assert_eq!(
        pending_input_ids(&store).await,
        vec![late.input_id],
        "the late input waits for the next turn"
    );
}

/// A first execution whose drive was refused journals the refusal, and the
/// redrive replays that same refusal without reading a pending row.
///
/// The refusal here is the host withdrawing the accepted input between its
/// acceptance and its drive; the turn cedes before any provider work.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn drive_effect_refusal_is_journaled(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-refused-drive"));
    let journal = Journal::new();
    let (provider, requests) = recording_provider("never reached");

    // The acceptance mints its id inside the effect, so the withdrawal targets
    // the only open row in the session, which is the accepted one.
    let withdrawing: Arc<dyn crate::RuntimePersistence> = Arc::new(WithdrawBeforeClaim {
        inner: Arc::clone(&store),
    });
    let refused = journal
        .run(
            &withdrawing,
            provider.clone(),
            &turn_id,
            "withdrawn in flight",
        )
        .await
        .expect_err("a withdrawn acceptance cedes");
    assert_eq!(
        refused.code,
        crate::RuntimeErrorCode::AcceptedTurnInputCeded,
        "{refused:?}"
    );
    assert!(
        matches!(
            journal.controller.journaled_drive(),
            Some(crate::AcceptedTurnInputDrive::Refused {
                refusal: crate::AcceptedTurnInputRefusal::SettledOrRemoved
            })
        ),
        "the refusal is journaled"
    );

    let (redrive_store, reads) = RedriveStore::wrap(&store);
    let redrive_store: Arc<dyn crate::RuntimePersistence> = redrive_store;
    let replayed = journal
        .run(&redrive_store, provider, &turn_id, "withdrawn in flight")
        .await
        .expect_err("the redrive replays the refusal");
    assert_eq!(
        replayed.code,
        crate::RuntimeErrorCode::AcceptedTurnInputCeded
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "a replayed drive never claims or reads a pending row"
    );
    assert!(requests.lock().expect("request lock").is_empty());
    assert!(pending_input_ids(&store).await.is_empty());
}

/// Withdraws the session's open next-turn row right before the first claim.
struct WithdrawBeforeClaim {
    inner: Arc<dyn crate::RuntimePersistence>,
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for WithdrawBeforeClaim {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<crate::TurnInputClaim>, crate::StoreError> {
        for open in self.inner.list_pending_turn_inputs(session_id).await? {
            self.inner
                .cancel_pending_turn_input(session_id, &open.input.input_id)
                .await?;
        }
        self.inner
            .claim_next_turn_inputs(session_id, session_execution_lease, owner, max_inputs)
            .await
    }
}

/// A direct turn whose accepted input sits behind more earlier admissions than
/// one claim absorbs drives nothing and drops nothing: the call succeeds with a
/// `Queued` outcome naming the inputs ahead, a replay reports the same queue
/// position without reading a row, and the queued-work drain then answers
/// every input in arrival order, each exactly once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn queued_direct_turn_input_is_answered_in_order_by_the_drain(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-queued-direct-turn"));
    let first = enqueue_next_turn(&store, "earliest admission").await;
    let second = enqueue_next_turn(&store, "second admission").await;
    let journal = Journal::new().with_turn_input_claim(2);
    let (provider, requests) = recording_provider("answered in order");

    let queued = journal
        .run(&store, provider.clone(), &turn_id, "the direct input")
        .await
        .expect("a direct turn past the claim bound succeeds with its input queued");
    assert!(
        matches!(queued.outcome, crate::TurnOutcome::Queued { ahead: 2 }),
        "the call reports the queue position as its outcome: {:?}",
        queued.outcome
    );
    let input_id = queued
        .turn_input_acceptance
        .as_ref()
        .expect("a queued call reports its acceptance")
        .input_id
        .clone();
    assert!(
        queued.llm_calls.is_empty() && queued.tool_calls.is_empty(),
        "a queued call ran no turn"
    );
    assert!(
        matches!(
            journal.controller.journaled_drive(),
            Some(crate::AcceptedTurnInputDrive::Queued { ahead: 2 })
        ),
        "the queued outcome is journaled"
    );
    assert!(
        requests.lock().expect("request lock").is_empty(),
        "a queued direct turn reaches no provider"
    );
    assert_eq!(
        pending_input_ids(&store).await,
        vec![
            first.input_id.clone(),
            second.input_id.clone(),
            input_id.clone()
        ],
        "nothing is dropped: every input stays queued in arrival order"
    );

    let (redrive_store, reads) = RedriveStore::wrap(&store);
    let redrive_store: Arc<dyn crate::RuntimePersistence> = redrive_store;
    let replayed = journal
        .run(
            &redrive_store,
            provider.clone(),
            &turn_id,
            "the direct input",
        )
        .await
        .expect("a replay succeeds with the same queue position");
    assert!(
        matches!(replayed.outcome, crate::TurnOutcome::Queued { ahead: 2 }),
        "{:?}",
        replayed.outcome
    );
    assert_eq!(
        replayed
            .turn_input_acceptance
            .as_ref()
            .map(|acceptance| acceptance.input_id.clone()),
        Some(input_id.clone())
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    let mut drains = 0;
    loop {
        let mut drainer = acceptance_runtime_with_batching(
            SESSION_ID,
            &store,
            &journal.effect_host,
            provider.clone(),
            Vec::new(),
            crate::testing::runtime_lease_owner(),
            journal.batching.clone(),
        )
        .await;
        let drain_id = format!("{prefix}-queued-drain-{drains}");
        let scope = journal
            .effect_host
            .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &drain_id)))
            .expect("scope a queued drain");
        let drain = drainer
            .stream_next_queued_work(crate::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scope,
            ))
            .await
            .expect("the drain runs");
        match drain {
            crate::QueuedTurnDrain::Ran(_) => drains += 1,
            crate::QueuedTurnDrain::Empty(_) => break,
            crate::QueuedTurnDrain::Replayed(_) => {
                panic!("a fresh drain of queued inputs never replays a committed run")
            }
        }
        assert!(drains <= 3, "the queue drains in a bounded number of turns");
    }

    let answered = applications(&store).await;
    assert_eq!(
        answered
            .iter()
            .map(|application| application.input_id.clone())
            .collect::<Vec<_>>(),
        vec![first.input_id, second.input_id, input_id.clone()],
        "the drain answers every input once, in arrival order"
    );
    let direct_answer = answered
        .iter()
        .find(|application| application.input_id == input_id)
        .expect("the queued direct input is answered");
    assert_ne!(
        direct_answer.turn_id.as_str(),
        turn_id,
        "a drain turn answers the queued input, not the direct call"
    );
    assert!(pending_input_ids(&store).await.is_empty());
    let requests = requests.lock().expect("request lock").clone();
    assert_eq!(requests.len(), drains, "one provider call per drained turn");
    assert!(
        requests
            .last()
            .is_some_and(|last| last.contains("the direct input")),
        "the queued direct input is answered last: {requests:?}"
    );
}

/// The worker dies after its drive is journaled and before its commit; while
/// it is down, a recovery drain under a newer lease generation reclaims the
/// same rows and answers them. The redrive replays the journaled claim, finds
/// its settlement superseded, and cedes: it never commits the same words a
/// second time without a settlement (ADR 0069 §6).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn uncommitted_redrive_cedes_when_a_drain_answered_its_rows(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-redrive-after-drain"));
    let journal = Journal::new();
    let (provider, _) = recording_provider("answered by the first driver to commit");
    journal
        .run_with_plugins(
            &store,
            provider.clone(),
            vec![die_before_commit_plugin()],
            &turn_id,
            "answer me once",
        )
        .await
        .expect_err("the worker dies after its drive is journaled");
    let journaled = match journal.controller.journaled_drive() {
        Some(crate::AcceptedTurnInputDrive::Claimed { claim }) => claim,
        other => panic!("the first execution claimed its accepted row: {other:?}"),
    };
    let accepted = journaled.inputs[0].input_id.clone();

    let mut drainer = acceptance_runtime(
        &store,
        &journal.effect_host,
        provider.clone(),
        Vec::new(),
        crate::LeaseOwnerIdentity::opaque(
            format!("{prefix}-recovery-owner"),
            format!("{prefix}-recovery-incarnation"),
        ),
    )
    .await;
    let drain_id = format!("{prefix}-recovery-drain");
    let drain_scope = journal
        .effect_host
        .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &drain_id)))
        .expect("scope the recovery drain");
    let drain = drainer
        .stream_next_queued_work(crate::TurnOptions::new(
            tokio_util::sync::CancellationToken::new(),
            drain_scope,
        ))
        .await
        .expect("the recovery drain runs");
    assert!(
        matches!(drain, crate::QueuedTurnDrain::Ran(_)),
        "the recovery drain reclaims and answers the orphaned rows"
    );
    drop(drainer);

    // The redrive resumes the session as the store now holds it, the drain's
    // answer included: nothing about the head refuses it, so only the
    // settlement can.
    let ceded = journal
        .run(&store, provider, &turn_id, "answer me once")
        .await
        .expect_err("a redrive whose rows another driver answered must not commit them again");
    assert_eq!(
        ceded.code,
        crate::RuntimeErrorCode::AcceptedTurnInputCeded,
        "{ceded:?}"
    );
    let applied = applications(&store).await;
    assert_eq!(
        applied
            .iter()
            .filter(|application| application.input_id == accepted)
            .count(),
        1,
        "the input is answered exactly once: {applied:?}"
    );
    assert!(
        applied
            .iter()
            .all(|application| application.turn_id.as_str() == drain_id),
        "the recovery drain's turn is the one that answered it: {applied:?}"
    );
    assert!(pending_input_ids(&store).await.is_empty());
}
