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

/// FIG-2964, ADR 0107: registration reports whether *this* call created the
/// row, and a key lash derives is trusted.
///
/// A start under a retained key returns the process that key registered. So
/// "the registration succeeded" does not mean "this call owns the row", and a
/// caller that compensates a failed start — writing the row's `StartFailed`
/// terminal — must distinguish the two or a retry would cancel the work its
/// predecessor started. The disposition is the only thing that licenses that
/// write, so every backend must report it alike. A key lash derives from an
/// admitted operation is trusted: a retry under it returns the retained
/// process whatever it submitted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_start_key_reports_created_then_existing_and_is_trusted(
    registry: Arc<dyn ProcessRegistry>,
) {
    let key = crate::StartKey::for_orchestration_call(
        &crate::ExecutionScope::runtime_operation("start-key-disposition"),
        "call-1",
        0,
    );
    let first = registry
        .register_process_reporting_disposition(
            registration("start-key-disposition").with_start_key(Some(key.clone())),
            &[],
        )
        .await
        .expect("first registration");
    assert_eq!(
        first.disposition,
        crate::ProcessRegistrationDisposition::Created,
        "the call that inserted the row created it"
    );
    assert_eq!(first.record.start_key.as_ref(), Some(&key));

    let repeat = registry
        .register_process_reporting_disposition(
            registration("start-key-disposition").with_start_key(Some(key.clone())),
            &[],
        )
        .await
        .expect("a repeat under a retained key is idempotent");
    assert_eq!(
        repeat.disposition,
        crate::ProcessRegistrationDisposition::Existing,
        "a repeat returns the row the first call created"
    );
    assert_eq!(repeat.record.id, first.record.id);

    // A derived key is trusted: a retry whose content changed returns the
    // retained process untouched, never a refusal and never an overwrite.
    let mut changed = registration("start-key-disposition").with_start_key(Some(key.clone()));
    changed.input = std::sync::Arc::new(ProcessInput::External {
        metadata: serde_json::json!({"suite": "changed-content-retry"}),
    });
    let retried = registry
        .register_process_reporting_disposition(changed, &[])
        .await
        .expect("a changed-content retry under a derived key is not a refusal");
    assert_eq!(
        retried.disposition,
        crate::ProcessRegistrationDisposition::Existing
    );
    assert_eq!(retried.record.id, first.record.id);
    assert_eq!(
        retried.record.input, first.record.input,
        "the retained process keeps the content it was started with"
    );
}

/// ADR 0107: a host's start key is the host's claim, scoped to the start's
/// owner. The same raw key from two sessions starts two processes, never one
/// session's process returned to the other; and the same key from one owner
/// with different content is a typed durable-identity conflict, never a
/// silent return of the retained process.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_host_start_key_is_scoped_to_its_owner_and_fences_its_content(
    registry: Arc<dyn ProcessRegistry>,
) {
    let under = |session: &str| {
        let session_id = crate::SessionId::from(session);
        let key = crate::StartKey::for_host(
            crate::StartKeyOwner::Session {
                session_id: &session_id,
            },
            "nightly-report",
        );
        let mut registration = registration("host-key-owner").with_start_key(Some(key));
        registration.provenance = ProcessProvenance::new(crate::ProcessOriginator::session(
            crate::SessionScope::new(session_id.clone()),
        ));
        registration
    };
    let first = registry
        .register_process_reporting_disposition(under("host-key-session-a"), &[])
        .await
        .expect("session A starts under its key");
    let other_owner = registry
        .register_process_reporting_disposition(under("host-key-session-b"), &[])
        .await
        .expect("session B starts under the same raw key");
    assert_eq!(
        other_owner.disposition,
        crate::ProcessRegistrationDisposition::Created,
        "the same raw key under another owner is another key"
    );
    assert_ne!(other_owner.record.id, first.record.id);

    let repeat = registry
        .register_process_reporting_disposition(under("host-key-session-a"), &[])
        .await
        .expect("an identical host retry is idempotent");
    assert_eq!(
        repeat.disposition,
        crate::ProcessRegistrationDisposition::Existing
    );
    assert_eq!(repeat.record.id, first.record.id);

    let mut changed = under("host-key-session-a");
    changed.input = std::sync::Arc::new(ProcessInput::External {
        metadata: serde_json::json!({"suite": "changed-host-content"}),
    });
    let error = registry
        .register_process_reporting_disposition(changed, &[])
        .await
        .expect_err("a changed-content host retry under a retained key is refused");
    assert!(
        crate::is_durable_identity_conflict(&error),
        "the refusal is the typed durable-identity conflict: {error}"
    );
    let retained = registry
        .get_process(&first.record.id)
        .await
        .expect("read the retained process")
        .expect("the retained process survives the refused retry");
    assert_eq!(retained.input, first.record.input);
}

/// ADR 0107: a keyless start is always new, and every registration mints a
/// fresh, well-formed id.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn keyless_starts_are_always_new(registry: Arc<dyn ProcessRegistry>) {
    let mut ids = std::collections::BTreeSet::new();
    for _ in 0..3 {
        let outcome = registry
            .register_process_reporting_disposition(registration("keyless"), &[])
            .await
            .expect("keyless registration");
        assert_eq!(
            outcome.disposition,
            crate::ProcessRegistrationDisposition::Created,
            "a keyless start never finds an existing process"
        );
        assert_eq!(outcome.record.start_key, None);
        assert_eq!(
            ProcessId::parse(outcome.record.id.as_str()),
            Ok(outcome.record.id.clone()),
            "the registrar mints a well-formed id"
        );
        ids.insert(outcome.record.id);
    }
    assert_eq!(ids.len(), 3, "every keyless start mints its own id");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn registration_and_observers_are_atomic(registry: Arc<dyn ProcessRegistry>) {
    let key = crate::StartKey::for_host(crate::StartKeyOwner::HOST, "observer-registration");
    let record = registry
        .register_process_with_observers(
            registration("observer-registration").with_start_key(Some(key.clone())),
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

    // A retry under the key returns the retained process and adopts none of
    // its own initial observers: the atomic set is the first call's.
    let replay = registry
        .register_process_with_observers(
            registration("observer-registration").with_start_key(Some(key)),
            &[SessionId::from("observer-c")],
        )
        .await
        .expect("a retry under a retained key is idempotent");
    assert_eq!(replay.id, record.id);
    assert_eq!(
        registry
            .observers_for_process(&record.id)
            .await
            .expect("read process observers after the retry"),
        vec!["observer-a".to_string(), "observer-b".to_string()],
        "the retry's observers are not adopted"
    );

    let wake_only = registry
        .register_process(
            registration("wake-without-observer")
                .with_wake_session_id(Some(SessionId::from("wake-only-session"))),
        )
        .await
        .expect("register wake-only process");
    assert!(
        !registry
            .is_observer(&SessionId::from("wake-only-session"), &wake_only.id)
            .await
            .expect("wake target must not imply observer"),
        "no observer edge may be minted from an embedded wake target"
    );
}

/// FIG-3190, ADR 0107: concurrent starts under one key register one process.
///
/// [`a_start_key_reports_created_then_existing_and_is_trusted`] proves the
/// sequential law. On PostgreSQL the key lookup and the insert are two
/// statements under `READ COMMITTED` on two connections, so two callers
/// presenting the same key — a redelivered trigger occurrence is exactly that
/// — can both read "no row". SQLite serializes every writer through one write
/// flow, so only PostgreSQL can lose the race.
///
/// The law: however the calls interleave, exactly one registration creates the
/// process, and every other one reports that process — also when the racers'
/// content differs under a lash-derived key, which is trusted. (A host key
/// fences its content instead.)
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn concurrent_starts_under_one_key_register_one_process(
    registry: Arc<dyn ProcessRegistry>,
) {
    const RACERS: usize = 8;
    for (round, differing_content) in [("identical", false), ("differing", true)] {
        let key = if differing_content {
            crate::StartKey::for_trigger_delivery(
                &format!("concurrent-start-key-{round}"),
                "concurrent-start-subscription",
                "incarnation",
                1,
            )
        } else {
            crate::StartKey::for_host(
                crate::StartKeyOwner::HOST,
                format!("concurrent-start-key-{round}"),
            )
        };
        let start = Arc::new(tokio::sync::Barrier::new(RACERS));
        let mut racers = Vec::with_capacity(RACERS);
        for index in 0..RACERS {
            let registry = Arc::clone(&registry);
            let start = Arc::clone(&start);
            let key = key.clone();
            racers.push(crate::task::spawn(async move {
                let mut racer = registration("concurrent-start-key").with_start_key(Some(key));
                if differing_content {
                    racer.input = std::sync::Arc::new(ProcessInput::External {
                        metadata: serde_json::json!({"racer": index}),
                    });
                }
                start.wait().await;
                registry
                    .register_process_reporting_disposition(racer, &[])
                    .await
            }));
        }

        let mut created = 0usize;
        let mut ids = std::collections::BTreeSet::new();
        for racer in racers {
            let outcome = racer.await.expect("concurrent registration task").expect(
                "a concurrent start under one key is idempotent, never a raw constraint error",
            );
            if outcome.disposition == crate::ProcessRegistrationDisposition::Created {
                created += 1;
            }
            ids.insert(outcome.record.id);
        }
        assert_eq!(created, 1, "{round}: exactly one racer creates the process");
        assert_eq!(
            ids.len(),
            1,
            "{round}: every racer reports the one process: {ids:?}"
        );
    }
}

/// FIG-3611 L4, ADR 0107: a start key whose process was pruned starts a new
/// process with a new id, and the pruned id refuses as no longer retained —
/// never as the new process.
///
/// Red on the parent commit, where the key was the process's reusable name:
/// the second registration reused the name as a new incarnation of the pruned
/// process instead of minting a process of its own.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_start_key_after_prune_starts_a_new_process(registry: Arc<dyn ProcessRegistry>) {
    let key = crate::StartKey::for_host(crate::StartKeyOwner::HOST, "start-key-after-prune");
    let first = registry
        .register_process(registration("start-key-after-prune").with_start_key(Some(key.clone())))
        .await
        .expect("register the key's first process");
    registry
        .complete_process(
            &first.id,
            settled_success(serde_json::json!({"process": "first"})),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete the first process");
    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune the first process");

    let second = registry
        .register_process_reporting_disposition(
            registration("start-key-after-prune").with_start_key(Some(key)),
            &[],
        )
        .await
        .expect("start again under the pruned process's key");
    assert_eq!(
        second.disposition,
        crate::ProcessRegistrationDisposition::Created,
        "a pruned process no longer holds its key"
    );
    assert_ne!(second.record.id, first.id, "a minted id is never reused");
    assert!(
        matches!(
            registry.get_process(&first.id).await,
            Err(PluginError::ProcessNoLongerRetained { .. })
        ),
        "the pruned id refuses; it never resolves to the new process"
    );
}
