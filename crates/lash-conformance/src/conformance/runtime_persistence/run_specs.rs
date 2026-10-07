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
