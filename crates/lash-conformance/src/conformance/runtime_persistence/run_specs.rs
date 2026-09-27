//! Run specs on the pending turn-input ledger (FIG-3838).
//!
//! A non-default spec is interned once per session and hash in the
//! transaction that admits its input, and its hash is part of the input's
//! submission digest. A next-turn claim never mixes specs: its prefix stops,
//! never skips, at the first input whose spec differs from its head's. An
//! input addressed to a running turn joins that turn's shape, so a differing
//! explicit spec is refused before anything is stored.

use super::*;
use pretty_assertions::assert_eq;

fn spec_with_prompt(guidance: &str) -> crate::RunSpec {
    crate::RunSpec::overrides(crate::RunOverrides {
        prompt: Some(crate::PromptLayer::new().with_contribution(
            crate::PromptContribution::guidance("Shape", guidance.to_string()),
        )),
        ..crate::RunOverrides::default()
    })
}

fn spec_with_provider(provider_id: &str) -> crate::RunSpec {
    crate::RunSpec::overrides(crate::RunOverrides {
        provider_id: Some(provider_id.to_string()),
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
pub async fn run_specs_join_the_submission_digest_and_intern_once(
    store: Arc<dyn RuntimePersistence>,
) {
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

    let shaped = spec_with_prompt("review carefully");
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
            draft("shaped", "host:shaped").with_run_spec(spec_with_prompt("skim")),
            "a different spec changes the submission",
        ),
        (
            draft("plain", "host:plain").with_run_spec(spec_with_provider("other-route")),
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
                draft("shaped", "host:shaped").with_run_spec(spec_with_prompt("skim"))
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

/// A next-turn claim never mixes run specs: with every input pending and a
/// permissive bound, `A, A, B, A` claims `[A, A]`, then `[B]`, then `[A]`.
/// The prefix stops at the first differing spec and never reaches past it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_next_turn_claim_never_mixes_run_specs(store: Arc<dyn RuntimePersistence>) {
    let session_id = SessionId::from("run-spec-claims");
    let a = spec_with_prompt("shape a");
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
    let lease =
        claim_session_execution_lease_for_test(&store, &session_id, "run-spec-claim-owner").await;
    let owner = lease_owner("run-spec-claim-owner");
    let mut compositions = Vec::new();
    loop {
        let Some(claim) = store
            .claim_next_turn_inputs(&session_id, &lease.fence(), &owner, 64)
            .await
            .expect("claim the next-turn prefix")
        else {
            break;
        };
        let ids = claim
            .inputs
            .iter()
            .map(|input| input.input_id.clone())
            .collect::<Vec<_>>();
        // Hand the claim back and withdraw its rows, so the next claim
        // starts at the row after it.
        store
            .abandon_turn_input_claim(&claim)
            .await
            .expect("hand the claim back");
        for id in &ids {
            store
                .cancel_pending_turn_input(&session_id, id)
                .await
                .expect("withdraw a claimed row");
        }
        compositions.push(ids);
    }
    assert_eq!(
        compositions,
        vec![
            vec![enqueued[0].clone(), enqueued[1].clone()],
            vec![enqueued[2].clone()],
            vec![enqueued[3].clone()],
        ],
        "four inputs in three ordered claims: the prefix stops at each spec change"
    );
}

/// An input addressed to a running turn joins that turn's recorded shape.
/// An omitted spec inherits it and an equal spec matches it; a differing
/// explicit spec is refused before anything is stored. Once the turn's own
/// input is delivered the turn has ended, and an input addressed to it is a
/// next-turn input under its own spec.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_steering_spec_that_differs_from_its_running_turn_is_refused(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = SessionId::from("run-spec-steering");
    let turn = TurnId::from("run-spec-steered-turn");
    let shape = spec_with_prompt("steered shape");
    let started = store
        .enqueue_pending_turn_input(
            pending_next_turn_input_draft(&session_id, "start the turn")
                .with_source_key(turn.as_str())
                .with_run_spec(shape.clone()),
        )
        .await
        .expect("admit the input that starts the turn");
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
        .enqueue_pending_turn_input(steer("other shape", spec_with_provider("elsewhere")))
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

    store
        .cancel_pending_turn_input(&session_id, &started.input_id)
        .await
        .expect("settle the turn's own input");
    store
        .enqueue_pending_turn_input(steer("after the turn", spec_with_provider("elsewhere")))
        .await
        .expect("an input addressed to an ended turn runs under its own spec");
}
