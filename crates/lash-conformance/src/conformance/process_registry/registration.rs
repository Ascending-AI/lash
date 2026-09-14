use super::*;
use pretty_assertions::assert_eq;

/// FIG-2964: registration reports whether *this* call created the row.
///
/// Registration is idempotent by fingerprint on every tier: an exact repeat
/// returns `Ok` with the row an earlier call created, and only a differing
/// fingerprint is refused. So "the registration succeeded" does not mean "this
/// call owns the row", and a caller that compensates a failed start — writing
/// the row's `StartFailed` terminal — must distinguish the two or a retry
/// would cancel the work its predecessor started. The disposition is the only
/// thing that licenses that write, so every backend must report it alike.
pub async fn registration_reports_created_then_existing(registry: Arc<dyn ProcessRegistry>) {
    let id = "registration-disposition";
    let first = registry
        .register_process_reporting_disposition(registration(id), &[])
        .await
        .expect("first registration");
    assert_eq!(
        first.disposition,
        crate::ProcessRegistrationDisposition::Created,
        "the call that inserted the row created it"
    );

    let repeat = registry
        .register_process_reporting_disposition(registration(id), &[])
        .await
        .expect("an exact repeat is idempotent, not a refusal");
    assert_eq!(
        repeat.disposition,
        crate::ProcessRegistrationDisposition::Existing,
        "an exact repeat returns the row the first call created"
    );
    assert_eq!(
        repeat.record.registration_fingerprint, first.record.registration_fingerprint,
        "the returned row is the recorded one, untouched"
    );

    // A differing fingerprint is neither Created nor Existing: it is a refusal.
    // Identity is a derivation and no longer part of the preimage (FIG-2992), so
    // the conflicting registration differs in its recorded input instead.
    let mut conflicting = registration(id);
    conflicting.input = std::sync::Arc::new(ProcessInput::External {
        metadata: serde_json::json!({"suite": "disposition-conflict"}),
    });
    let conflict = registry
        .register_process_reporting_disposition(conflicting, &[])
        .await
        .expect_err("a differing fingerprint is a conflict, not a silent overwrite");
    assert!(
        conflict.to_string().contains("registration fingerprint"),
        "the refusal names the fingerprint: {conflict}"
    );
}

pub async fn registration_and_observers_are_atomic(registry: Arc<dyn ProcessRegistry>) {
    let record = registry
        .register_process_with_observers(
            registration("observer-registration"),
            &[SessionId::from("observer-a"), SessionId::from("observer-b")],
        )
        .await
        .expect("register process with observers");
    assert_eq!(record.status, ProcessStatus::Running);
    assert_eq!(
        registry
            .observers_for_process(&record.id)
            .await
            .expect("read process observers"),
        vec!["observer-a".to_string(), "observer-b".to_string()]
    );
    assert!(
        registry
            .is_observer(&SessionId::from("observer-a"), &record.id)
            .await
            .expect("check observer")
    );

    let replay = registry
        .register_process_with_observers(
            registration("observer-registration"),
            &[SessionId::from("observer-b"), SessionId::from("observer-a")],
        )
        .await
        .expect("replay registration");
    assert_eq!(
        record.registration_fingerprint,
        replay.registration_fingerprint
    );
    assert!(
        registry
            .register_process_with_observers(
                registration("observer-registration"),
                &[SessionId::from("observer-a")],
            )
            .await
            .is_err(),
        "changing the atomic initial observer set must conflict"
    );

    registry
        .register_process(
            registration("wake-without-observer")
                .with_wake_session_id(Some(SessionId::from("wake-only-session"))),
        )
        .await
        .expect("register wake-only process");
    assert!(
        !registry
            .is_observer(
                &SessionId::from("wake-only-session"),
                &ProcessId::from("wake-without-observer")
            )
            .await
            .expect("wake target must not imply observer"),
        "no observer edge may be minted from an embedded wake target"
    );
}
