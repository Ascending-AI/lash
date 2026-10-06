//! Acceptance-before-shift laws for direct turns (ADR 0069).
//!
//! Every turn enters through one durable acceptance commit and is then executed,
//! so these belong to the store contract rather than to one backend's tests: a
//! backend that admits a direct turn without recording it, or records it in a
//! shape its own drains cannot recover, has a different ingress from its
//! siblings.
//!
//! A host's turn is a send the engine executes; the in-process entry these laws
//! shift is the one a child session's turn takes inside its parent's
//! execution, through the testing door
//! [`TestTurnExecution::execute_child_session_turn`](crate::testing::TestTurnExecution::execute_child_session_turn).
//!
//! The suites run a real runtime turn over the supplied durable store and read
//! it back only through surfaces every backend already owes:
//! `list_pending_turn_inputs`, `list_turn_input_applications`, and
//! `cancel_pending_turn_input`.

use crate::admit;
use lash_core::testing::TestTurnExecution as _;
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

pub(super) async fn acceptance_runtime(
    store: &Arc<dyn crate::RuntimeStore>,
    backend: &crate::Backend,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    lease_owner: crate::LeaseOwnerIdentity,
) -> crate::LashRuntime {
    acceptance_runtime_for_session(
        SESSION_ID,
        store,
        backend,
        provider,
        plugin_factories,
        lease_owner,
    )
    .await
}

/// [`acceptance_runtime`] over an explicit session id.
pub(super) async fn acceptance_runtime_for_session(
    session_id: &str,
    store: &Arc<dyn crate::RuntimeStore>,
    backend: &crate::Backend,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    lease_owner: crate::LeaseOwnerIdentity,
) -> crate::LashRuntime {
    acceptance_runtime_with_batching(
        session_id,
        store,
        backend,
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
    store: &Arc<dyn crate::RuntimeStore>,
    backend: &crate::Backend,
    provider: crate::ProviderHandle,
    plugin_factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    lease_owner: crate::LeaseOwnerIdentity,
    batching: crate::QueuedWorkBatchingConfig,
) -> crate::LashRuntime {
    let mut host = crate::RuntimeHostConfig::new(
        backend.clone(),
        crate::CommitBudget::bounded(1024 * 1024, 512),
        batching.clone(),
    );
    host.providers.models = crate::testing::standard_test_llm_profiles(provider);
    let policy = crate::testing::mock_session_policy();
    let state = crate::RuntimeSessionState {
        session_id: SessionId::fixture(session_id.to_string()),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    Box::pin(
        crate::LashRuntime::builder(host, lease_owner)
            .with_session_id(SessionId::fixture(session_id))
            .with_policy(policy)
            .with_initial_state(state)
            .with_plugin_factories(
                crate::testing::test_standard_protocol_factories()
                    .into_iter()
                    .chain(plugin_factories)
                    .collect(),
            )
            .with_store(crate::conformance::helpers::session_view(
                store,
                SessionId::fixture(session_id.to_string()),
            ))
            .build(),
    )
    .await
    .expect("build the direct-turn acceptance conformance runtime")
}

pub(super) fn direct_input(turn_id: &TurnId, text: &str) -> crate::TurnInput {
    let mut input = crate::TurnInput::text(text);
    input.trace_turn_id = Some(turn_id.clone());
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
/// Mid-shift the session offers *no* open input, because the accepted row is
/// bound to this turn's own run. The ordinary pending listing still returns
/// that row with the factual `Admitted{run}` status naming the run.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn direct_turn_accepts_before_executing(
    prefix: &str,
    backend: crate::Backend,
    store: Arc<dyn crate::RuntimeStore>,
) {
    let turn_id = TurnId::fixture(format!("{prefix}-accept-before-shift"));
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
                    // was already true before the shift began.
                    let pending = store
                        .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
                        .await
                        .expect("read the session's pending inputs mid-shift");
                    *probe.lock().expect("probe lock") = Some(pending);
                    Ok(text_response("accepted"))
                }
            })
            .build()
            .into_handle()
    };
    let effect_host = crate::ActorContext::detached(backend.clone());
    let mut runtime = acceptance_runtime(
        &store,
        &backend,
        provider,
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let scope = effect_host
        .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &turn_id)))
        .expect("scope the direct acceptance turn");
    let turn = runtime
        .execute_child_session_turn(
            direct_input(&turn_id, "direct turn under durable acceptance"),
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
        )
        .await
        .expect("run the direct acceptance conformance turn");

    let pending = probe
        .lock()
        .expect("probe lock")
        .clone()
        .expect("the provider must have run");
    assert_eq!(pending.len(), 1, "the admitted input must remain visible");
    let held = &pending[0];
    assert_eq!(
        held.status,
        crate::PendingTurnInputReadStatus::Admitted {
            run: turn_id.clone()
        },
        "the admitted marker must name the run executing it"
    );
    assert_eq!(
        held.input.state,
        crate::TurnInputState::DeferredNextTurn,
        "admitted is a read status, not a persisted TurnInputState"
    );

    let acceptance = turn
        .turn_input_acceptance
        .as_ref()
        .expect("a store-backed direct turn exposes its acceptance identity");
    let input_id = acceptance.input_id.clone();
    assert_eq!(
        held.input.input_id, input_id,
        "the held projection must name the acceptance this turn is executing"
    );
    assert_eq!(acceptance.session_id, SESSION_ID);
    assert_eq!(
        acceptance.source_key.as_deref(),
        Some(turn_id.as_str()),
        "direct ingress names its row by the turn id, the run the shift runs it under"
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
    // row, so the shift consumed the acceptance rather than the caller's copy
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

/// Direct ingress identity is the turn id: two direct turns carrying the same
/// content under two turn ids are two admissions, each keyed by its own turn
/// id (the run the shift runs it under), never by its content.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn direct_turn_acceptance_mints_no_idempotency_key(
    prefix: &str,
    backend: crate::Backend,
    store: Arc<dyn crate::RuntimeStore>,
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
    let effect_host = crate::ActorContext::detached(backend.clone());
    let mut runtime = acceptance_runtime(
        &store,
        &backend,
        provider,
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    let mut acceptances = Vec::new();
    for round in 0..2 {
        let turn_id = TurnId::fixture(format!("{prefix}-resubmit-{round}"));
        let scope = effect_host
            .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &turn_id)))
            .expect("scope a resubmitted direct turn");
        let turn = runtime
            .execute_child_session_turn(
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
    assert_eq!(
        acceptances
            .iter()
            .map(|acceptance| acceptance.source_key.clone())
            .collect::<Vec<_>>(),
        (0..2)
            .map(|round| Some(format!("{prefix}-resubmit-{round}")))
            .collect::<Vec<_>>(),
        "direct ingress keys each row by its turn id and by nothing else"
    );
}
