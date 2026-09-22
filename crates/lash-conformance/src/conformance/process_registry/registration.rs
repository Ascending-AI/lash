use super::*;
use pretty_assertions::assert_eq;

/// The backend's maker hands every registry law its own registry handle rather
/// than one shared instance.
pub async fn process_registry_fresh_instances<F>(make: &F)
where
    F: Fn(&str) -> Arc<dyn crate::ConformanceProcessRegistry>,
{
    let first = make("fresh-instance-probe");
    let second = make("fresh-instance-probe");
    assert_fresh_instances(&first, &second, "process_registry");
}

/// FIG-2964: registration reports whether *this* call created the row.
///
/// Registration is idempotent by fingerprint on every tier: an exact repeat
/// returns `Ok` with the row an earlier call created, and only a differing
/// fingerprint is refused. So "the registration succeeded" does not mean "this
/// call owns the row", and a caller that compensates a failed start — writing
/// the row's `StartFailed` terminal — must distinguish the two or a retry
/// would cancel the work its predecessor started. The disposition is the only
/// thing that licenses that write, so every backend must report it alike.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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

/// FIG-3190: the repeat may arrive concurrently, and it is still a repeat.
///
/// [`registration_reports_created_then_existing`] proves the sequential law:
/// read the row, and either return it or refuse the fingerprint. On
/// PostgreSQL that read and the insert are two statements under `READ
/// COMMITTED` on two connections, so two callers deriving the same
/// content-addressed process id — a redelivered trigger occurrence is exactly
/// that — both read "no row" and both insert. SQLite serializes every writer
/// through one write flow and the in-memory registry through one transaction
/// mutex, so only PostgreSQL can lose the race, and it lost it as a raw
/// `duplicate key value violates unique constraint "lash_processes_pkey"`
/// rather than as the idempotent success the sequential law promises.
///
/// The law: however the calls interleave, exactly one registration creates the
/// row, every other identical one reports the row it created, and a
/// conflicting registration under that id is the typed fingerprint refusal on
/// every backend.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn concurrent_identical_registrations_are_idempotent(registry: Arc<dyn ProcessRegistry>) {
    const RACERS: usize = 8;
    let id = "registration-concurrent-repeat";

    let start = Arc::new(tokio::sync::Barrier::new(RACERS));
    let mut racers = Vec::with_capacity(RACERS);
    for _ in 0..RACERS {
        let registry = Arc::clone(&registry);
        let start = Arc::clone(&start);
        racers.push(crate::task::spawn(async move {
            start.wait().await;
            registry
                .register_process_reporting_disposition(registration(id), &[])
                .await
        }));
    }

    let mut created = 0usize;
    let mut existing = 0usize;
    let mut fingerprints = std::collections::BTreeSet::new();
    let mut incarnations = std::collections::BTreeSet::new();
    for racer in racers {
        let outcome = racer
            .await
            .expect("concurrent registration task")
            .expect("a concurrent exact repeat is idempotent, never a raw duplicate-key error");
        match outcome.disposition {
            crate::ProcessRegistrationDisposition::Created => created += 1,
            crate::ProcessRegistrationDisposition::Existing => existing += 1,
        }
        fingerprints.insert(outcome.record.registration_fingerprint.clone());
        incarnations.insert(outcome.record.incarnation.registration_sequence());
    }

    assert_eq!(
        created, 1,
        "exactly one concurrent caller may own the row it inserted"
    );
    assert_eq!(
        existing,
        RACERS - 1,
        "every caller that lost the race reports the row the winner created"
    );
    assert_eq!(
        fingerprints.len(),
        1,
        "every racer returns the one recorded registration, untouched: {fingerprints:?}"
    );
    assert_eq!(
        incarnations.len(),
        1,
        "a losing racer must not mint a second incarnation: {incarnations:?}"
    );

    // The losing side of the race is not licensed to overwrite either: a
    // differing fingerprint under the same id stays the typed refusal, not a
    // raw backend constraint error.
    let mut conflicting_racers = Vec::with_capacity(RACERS);
    let conflict_start = Arc::new(tokio::sync::Barrier::new(RACERS));
    for index in 0..RACERS {
        let registry = Arc::clone(&registry);
        let conflict_start = Arc::clone(&conflict_start);
        conflicting_racers.push(crate::task::spawn(async move {
            let mut conflicting = registration(id);
            conflicting.input = std::sync::Arc::new(ProcessInput::External {
                metadata: serde_json::json!({"suite": "concurrent-conflict", "racer": index}),
            });
            conflict_start.wait().await;
            registry
                .register_process_reporting_disposition(conflicting, &[])
                .await
        }));
    }
    for racer in conflicting_racers {
        let refusal = racer
            .await
            .expect("conflicting registration task")
            .expect_err("a differing fingerprint is a conflict, not a silent overwrite");
        assert!(
            refusal.to_string().contains("registration fingerprint"),
            "the refusal names the fingerprint rather than the backend constraint: {refusal}"
        );
    }

    let settled = registry
        .get_process(&ProcessId::from(id))
        .await
        .expect("read the raced row")
        .expect("the raced row is durable");
    assert_eq!(
        settled.registration_fingerprint,
        fingerprints
            .iter()
            .next()
            .expect("one recorded fingerprint")
            .clone(),
        "the recorded row is the one every racer reported"
    );
}
