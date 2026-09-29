//! Consumer holds (ADR 0116 §3.4, §3.6): the hold a parked call's declared
//! start registers its child under, and the abandonment that fences it.

use super::*;
use pretty_assertions::assert_eq;

fn held(
    session: &SessionId,
    owner: &lash_core::ScopeId,
    key: &str,
    cancels: bool,
) -> ProcessRegistration {
    crate::started_detached(
        ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            ProcessProvenance::session(SessionScope::new(session.as_str())),
            lash_core::Lifetime::Detached,
        ),
        owner.clone(),
    )
    .with_consumer_hold(Some(lash_core::ConsumerHold {
        key: key.to_string(),
        owner: owner.clone(),
        cancels,
    }))
}

/// Abandoning a hold returns the processes it holds that its call owes a
/// cancel, and from then on a start under the hold is refused: a launch
/// racing the cancel either registers first and is returned, or never
/// registers. Other holds are untouched, and abandoning again keeps the
/// answer.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn an_abandoned_consumer_hold_fences_registration(
    registry: Arc<dyn ProcessRegistry>,
) {
    let session = SessionId::from("consumer-hold-session");
    let owner =
        lash_core::ScopeId::turn(session.clone(), crate::TurnId::from("consumer-hold-turn"));
    let owed = registry
        .register_process(held(&session, &owner, "hold-owed", true))
        .await
        .expect("register a child its call owes a cancel")
        .id;
    registry
        .register_process(held(&session, &owner, "hold-ignored", false))
        .await
        .expect("register a child its call owes nothing");

    assert_eq!(
        registry
            .abandon_consumer_hold("hold-owed", &owner)
            .await
            .expect("abandon a hold that owes a cancel"),
        vec![owed.clone()],
        "the abandoned hold returns the child its call owes a cancel"
    );
    assert_eq!(
        registry
            .abandon_consumer_hold("hold-owed", &owner)
            .await
            .expect("abandon the hold again"),
        vec![owed],
        "abandoning again keeps the answer"
    );
    assert!(
        registry
            .abandon_consumer_hold("hold-ignored", &owner)
            .await
            .expect("abandon a hold that owes nothing")
            .is_empty(),
        "a hold under an ignored cancel owes nothing"
    );

    let refused = registry
        .register_process(held(&session, &owner, "hold-owed", true))
        .await
        .expect_err("a start under an abandoned hold is refused");
    assert!(
        matches!(
            &refused,
            PluginError::Runtime(error)
                if error.code == crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided
        ),
        "the refusal is the abandoned call's cancel: {refused:?}"
    );
    registry
        .register_process(held(&session, &owner, "hold-live", true))
        .await
        .expect("a start under another hold registers");
}
