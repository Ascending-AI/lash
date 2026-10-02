//! Run specs on the pending turn-input ledger (FIG-3838).
//!
//! A non-default spec is interned once per session and hash in the
//! transaction that admits its input, and its hash is part of the input's
//! submission digest. A next-turn admission never mixes specs: its prefix stops,
//! never skips, at the first input whose spec differs from its head's. An
//! input addressed to a running turn joins that turn's shape, so a differing
//! explicit spec is refused before anything is stored.

use super::*;
use pretty_assertions::assert_eq;

/// A spec whose protocol options state `shape`: two shapes are two specs.
fn spec_with_shape(shape: &str) -> crate::RunSpec {
    crate::RunSpec::overrides(crate::RunOverrides {
        protocol_turn_options: Some(crate::ProtocolTurnOptions::from_payload(
            serde_json::json!({ "shape": shape }),
        )),
        ..crate::RunOverrides::default()
    })
}

fn spec_with_llm_profile(profile_key: &str) -> crate::RunSpec {
    crate::RunSpec::overrides(crate::RunOverrides {
        model: Some(crate::LlmProfileKey::new(profile_key)),
        ..crate::RunOverrides::default()
    })
}

/// A spec's hash is part of its input's submission: an omitted spec and an
/// explicit default are one submission, a same-key retry under another spec
/// is a typed conflict whatever became of the row, and the spec is interned
/// once and read back exactly.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn run_specs_join_the_submission_digest_and_intern_once(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("run-specs");
    let draft = |text: &str, key: &str| {
        pending_next_turn_input_draft(&session_id, text).with_source_key(key)
    };

    let omitted = store
        .enqueue_pending_turn_input(draft("plain", "host:plain"))
        .await
        .expect("admit an input with no spec");
    assert_eq!(
        omitted.run_spec, None,
        "the default spec is stored as no spec"
    );
    let explicit_default = store
        .enqueue_pending_turn_input(
            draft("plain", "host:plain").with_run_spec(crate::RunSpec::default()),
        )
        .await
        .expect("an explicit default spec is the same submission");
    assert_eq!(explicit_default.input_id, omitted.input_id);

    let shaped = spec_with_shape("review carefully");
    let hash = shaped
        .hash()
        .expect("hash the spec")
        .expect("a non-default spec has a hash");
    let first = store
        .enqueue_pending_turn_input(draft("shaped", "host:shaped").with_run_spec(shaped.clone()))
        .await
        .expect("admit an input under a spec");
    assert_eq!(first.run_spec.as_ref(), Some(&hash));
    let retry = store
        .enqueue_pending_turn_input(draft("shaped", "host:shaped").with_run_spec(shaped.clone()))
        .await
        .expect("an identical retry is the same submission");
    assert_eq!(retry.input_id, first.input_id);
    let sibling = store
        .enqueue_pending_turn_input(draft("sibling", "host:sibling").with_run_spec(shaped.clone()))
        .await
        .expect("a second input under the same spec shares its interned row");
    assert_eq!(sibling.run_spec.as_ref(), Some(&hash));
    assert_eq!(
        store
            .load_run_spec(&session_id, &hash)
            .await
            .expect("read the interned spec"),
        Some(shaped.clone()),
        "the interned spec reads back exactly"
    );

    for (changed, context) in [
        (
            draft("shaped", "host:shaped"),
            "dropping the spec changes the submission",
        ),
        (
            draft("shaped", "host:shaped").with_run_spec(spec_with_shape("skim")),
            "a different spec changes the submission",
        ),
        (
            draft("plain", "host:plain").with_run_spec(spec_with_llm_profile("other-route")),
            "adding a spec changes the submission",
        ),
    ] {
        assert!(
            matches!(
                store.enqueue_pending_turn_input(changed).await,
                Err(StoreError::PendingTurnInputSourceKeyConflict { .. })
            ),
            "{context}"
        );
    }

    // The digest outlives the row's lifecycle: a settled input still refuses
    // a retry under another spec and still answers an identical one.
    store
        .cancel_pending_turn_input(&session_id, &first.input_id)
        .await
        .expect("withdraw the shaped input");
    assert!(matches!(
        store
            .enqueue_pending_turn_input(
                draft("shaped", "host:shaped").with_run_spec(spec_with_shape("skim"))
            )
            .await,
        Err(StoreError::PendingTurnInputSourceKeyConflict { .. })
    ));
    let settled_retry = store
        .enqueue_pending_turn_input(draft("shaped", "host:shaped").with_run_spec(shaped))
        .await
        .expect("an identical retry after settlement answers the original");
    assert_eq!(settled_retry.input_id, first.input_id);
}

/// A next-turn admission never mixes run specs: with every input pending and
/// a permissive bound, `A, A, B, A` admits `[A, A]`, then `[B]`, then `[A]`.
/// The prefix stops at the first differing spec and never reaches past it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_next_turn_admission_never_mixes_run_specs(store: Arc<dyn RuntimeStore>) {
    let session_id = SessionId::from("run-spec-admissions");
    let a = spec_with_shape("shape a");
    let mut enqueued = Vec::new();
    for (text, spec) in [
        ("a1", a.clone()),
        ("a2", a.clone()),
        ("b", crate::RunSpec::default()),
        ("a3", a.clone()),
    ] {
        enqueued.push(
            store
                .enqueue_pending_turn_input(
                    pending_next_turn_input_draft(&session_id, text).with_run_spec(spec),
                )
                .await
                .expect("admit the law's input")
                .input_id,
        );
    }
    let fence = seal_shift_fence_for_test(&store, &session_id, "run-spec-admission-owner").await;
    let mut compositions = Vec::new();
    for ordinal in 0.. {
        // Each run is headed by the earliest open input and, once it ends,
        // the next run starts at the row after its admission.
        let Some(head) = store
            .list_pending_turn_inputs(&session_id)
            .await
            .expect("list open inputs")
            .into_iter()
            .filter(|read| read.status == crate::PendingTurnInputReadStatus::Open)
            .min_by_key(|read| read.input.enqueue_seq)
        else {
            break;
        };
        let admission = execute_run_to_end(
            &store,
            &fence,
            &format!("run-spec-run-{ordinal}"),
            crate::store::AdmittedHead::Input(head.input.input_id.clone()),
        )
        .await;
        compositions.push(admission.input_ids());
    }
    assert_eq!(
        compositions,
        vec![
            vec![enqueued[0].clone(), enqueued[1].clone()],
            vec![enqueued[2].clone()],
            vec![enqueued[3].clone()],
        ],
        "four inputs in three ordered admissions: the prefix stops at each spec change"
    );
}

/// An input addressed to a running turn joins that turn's recorded shape.
/// An omitted spec inherits it and an equal spec matches it; a differing
/// explicit spec is refused before anything is stored. Once the turn's run
/// has ended, an input addressed to it is a next-turn input under its own
/// spec.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_steering_spec_that_differs_from_its_running_turn_is_refused(
    store: Arc<dyn RuntimeStore>,
) {
    let session_id = SessionId::from("run-spec-steering");
    let turn = TurnId::from("run-spec-steered-turn");
    let shape = spec_with_shape("steered shape");
    let started = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&session_id, "start the turn")
                .with_source_key(turn.as_str())
                .with_run_spec(shape.clone()),
        )
        .await
        .expect("admit the input that starts the turn");
    let fence = seal_shift_fence_for_test(&store, &session_id, "run-spec-steering-owner").await;
    let admission = admitted_run(
        &store,
        &fence,
        turn.as_str(),
        crate::store::AdmittedHead::Input(started.input_id.clone()),
    )
    .await;
    let steer = |text: &str, spec: crate::RunSpec| {
        pending_active_turn_input_draft(
            &session_id,
            &turn,
            crate::TurnInputCheckpointBoundary::AfterWork,
            text,
        )
        .with_run_spec(spec)
    };

    let refused = store
        .enqueue_pending_turn_input(steer("other shape", spec_with_llm_profile("elsewhere")))
        .await;
    assert!(
        matches!(
            &refused,
            Err(StoreError::PendingTurnInputRunSpecMismatch { session_id: refused_session, turn_id })
                if *refused_session == session_id && *turn_id == turn
        ),
        "a differing explicit spec is refused: {refused:?}"
    );
    let inherited = store
        .enqueue_pending_turn_input(steer("inherit", crate::RunSpec::default()))
        .await
        .expect("an omitted spec inherits the running turn's shape");
    assert_eq!(inherited.run_spec, None);
    let matching = store
        .enqueue_pending_turn_input(steer("same shape", shape))
        .await
        .expect("the turn's own spec matches");
    assert_eq!(matching.run_spec, started.run_spec);
    assert_eq!(
        store
            .list_pending_turn_inputs(&session_id)
            .await
            .expect("list the session's inputs")
            .len(),
        3,
        "the refused input stored nothing"
    );

    end_run(
        &store,
        &fence,
        completing_admission(turn.as_str(), &admission),
    )
    .await;
    store
        .enqueue_pending_turn_input(steer("after the turn", spec_with_llm_profile("elsewhere")))
        .await
        .expect("an input addressed to an ended turn runs under its own spec");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn steering_commit(
    state: &RuntimeSessionState,
    turn_id: &str,
    pending_follow_on: Option<crate::store::PendingFollowOn>,
) -> RuntimeCommit {
    let mut graph = state.pending_graph_commit();
    let operation = crate::OperationId::turn(
        state.session_id.clone(),
        TurnId::fixture(turn_id.to_string()),
        "final",
    );
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("derive commit node ids");
    let mut commit =
        RuntimeCommit::persisted_state_with_graph_commit_and_operation(state, graph, operation)
            .expect("build the commit");
    commit.pending_follow_on = pending_follow_on;
    commit
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn commit_switch_owing(
    store: &Arc<dyn RuntimeStore>,
    session_id: &SessionId,
    switching_turn: &str,
    resolved_run: crate::ResolvedRun,
) -> crate::store::PendingFollowOn {
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    let owed = crate::store::PendingFollowOn {
        continuation: None,
        follow_on_turn_id: crate::store::PhysicalTurn::derive_turn_id(
            &TurnId::fixture(switching_turn),
            1,
        ),
        frame_id: state
            .current_frame_node_id
            .clone()
            .expect("the initial frame is current"),
        task: "run in the switched frame".to_string(),
        resolved_run: Box::new(resolved_run),
        chain_depth: 1,
        attempts: 0,
    };
    store
        .commit_runtime_state(steering_commit(&state, switching_turn, Some(owed.clone())))
        .await
        .expect("the switch commit writes its follow-on");
    owed
}

/// A follow-on the head owes is a running run under the shape its fact
/// recorded at the switch (FIG-3877): an input steered into the follow-on
/// turn must match that recorded spec, not the spec of the input that started
/// the parent run. An omitted spec inherits the recorded shape.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_steering_spec_must_match_a_pending_follow_ons_shape(store: Arc<dyn RuntimeStore>) {
    // The fact carries the shape the parent run recorded: steering joins it,
    // a differing spec — even the parent input's own — is refused.
    let session_id = SessionId::from("run-spec-follow-on-steering");
    let parent_shape = spec_with_shape("the parent input's shape");
    let recorded_shape = spec_with_shape("the recorded shape");
    let recorded_hash = recorded_shape
        .hash()
        .expect("hash the spec")
        .expect("a non-default spec has a hash");
    store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&session_id, "start the parent")
                .with_source_key("switching-turn")
                .with_run_spec(parent_shape.clone()),
        )
        .await
        .expect("admit the parent's starting input");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    let recorded = crate::ResolvedRun {
        base: RuntimeCommit::persisted_state_for_test(&state).config,
        spec: Some(recorded_hash),
        resolved: None,
        capabilities: std::collections::BTreeMap::new(),
        render: None,
        termination: crate::TerminationPolicy::default(),
        follow_on_recoveries: crate::store::DEFAULT_MAX_FOLLOW_ON_RECOVERIES,
    };
    let owed = commit_switch_owing(&store, &session_id, "switching-turn", recorded).await;
    let follow_on = owed.follow_on_turn_id.clone();
    let steer = |text: &str, spec: crate::RunSpec| {
        pending_active_turn_input_draft(
            &session_id,
            &follow_on,
            crate::TurnInputCheckpointBoundary::AfterWork,
            text,
        )
        .with_run_spec(spec)
    };

    assert!(
        matches!(
            store
                .enqueue_pending_turn_input(steer("the parent's shape", parent_shape.clone()))
                .await,
            Err(StoreError::PendingTurnInputRunSpecMismatch { turn_id, .. })
                if turn_id == follow_on
        ),
        "even the parent input's own spec is refused against the recorded shape"
    );
    store
        .enqueue_pending_turn_input(steer("the recorded shape", recorded_shape))
        .await
        .expect("the recorded spec matches");
    store
        .enqueue_pending_turn_input(steer("inherit", crate::RunSpec::default()))
        .await
        .expect("an omitted spec inherits the recorded shape");
}

/// A queued-headed run executes under the default spec (FIG-3877): its members
/// are queued work, which carries none, so an input steered into its physical
/// turn joins the default shape and any other spec is refused.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_steering_spec_must_match_a_queued_headed_runs_default_shape(
    store: Arc<dyn RuntimeStore>,
) {
    let session_id = SessionId::from("run-spec-queued-steering");
    let batch = store
        .enqueue_queued_work(checkpoint_admissions::queued_draft(
            &session_id,
            "the run's work",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue the head batch");
    let lease = seal_shift_fence_for_test(&store, &session_id, "queued-owner").await;
    super::run_admissions::admitted_on(
        &store,
        &lease,
        &session_id,
        "queued-run",
        crate::store::AdmittedHead::Batch(batch.batch_id),
    )
    .await;
    let turn = crate::store::PhysicalTurn::derive_turn_id(&TurnId::from("queued-run"), 0);
    let steer = |text: &str, spec: crate::RunSpec| {
        pending_active_turn_input_draft(
            &session_id,
            &turn,
            crate::TurnInputCheckpointBoundary::AfterWork,
            text,
        )
        .with_run_spec(spec)
    };

    assert!(matches!(
        store
            .enqueue_pending_turn_input(steer("another shape", spec_with_shape("skim")))
            .await,
        Err(StoreError::PendingTurnInputRunSpecMismatch { turn_id, .. })
            if turn_id == turn
    ));
    store
        .enqueue_pending_turn_input(steer("inherit", crate::RunSpec::default()))
        .await
        .expect("the default spec matches the run's shape");
}
