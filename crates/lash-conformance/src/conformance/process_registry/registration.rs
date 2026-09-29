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
    let key = crate::StartKey::for_tool_intent(
        crate::DERIVED_START_KEYS,
        &crate::derive_tool_intent_identity(
            &crate::SessionId::from("start-key-disposition"),
            "start-key-disposition",
            Some("call-1"),
            0,
        )
        .expect("the start's intent identity derives"),
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

/// A registration under the host key `bytes`, originated by `session` and
/// waking `wake`: the shape the host-key laws below vary one field of.
fn host_keyed(bytes: &str, session: &str, wake: Option<&str>) -> ProcessRegistration {
    let mut registration = registration("host-key")
        .with_start_key(Some(crate::StartKey::for_host(bytes)))
        .with_wake_session_id(wake.map(SessionId::from));
    registration.input = std::sync::Arc::new(ProcessInput::External {
        metadata: serde_json::json!({"report": "nightly", "secret": "input-metadata-of-a"}),
    });
    registration.provenance = ProcessProvenance::new(crate::ProcessOriginator::session(
        crate::SessionScope::new(session),
    ));
    registration
}

fn assert_start_key_conflict(error: &PluginError, bytes: &str) {
    assert!(
        matches!(
            error,
            PluginError::StartKeyConflict { start_key }
                if *start_key == crate::StartKey::for_host(bytes)
        ),
        "the refusal is the typed start-key conflict naming the key: {error:?}"
    );
}

/// ADR 0107 (FIG-4111): a host's start key is global and fences its start.
/// Lash mixes nothing into a host key, so the same bytes from another
/// originator are the same key; a start under it returns the retained process
/// only to the start that made it, and the other originator's start is a
/// typed [`PluginError::StartKeyConflict`] that leaves the retained row as it
/// was. The same originator with other content is the same conflict.
///
/// Red on the parent commit, where the key was scoped to its owner and
/// session B's bytes silently started a second process.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_host_start_key_is_global_and_fences_its_originator(
    registry: Arc<dyn ProcessRegistry>,
) {
    let bytes = "global-host-key";
    let first = registry
        .register_process_reporting_disposition(host_keyed(bytes, "host-key-session-a", None), &[])
        .await
        .expect("session A starts under the key");
    assert_eq!(
        first.disposition,
        crate::ProcessRegistrationDisposition::Created
    );

    let other_originator = registry
        .register_process_reporting_disposition(host_keyed(bytes, "host-key-session-b", None), &[])
        .await
        .expect_err("the same bytes from another originator are the same key, and fence it");
    assert_start_key_conflict(&other_originator, bytes);

    let repeat = registry
        .register_process_reporting_disposition(host_keyed(bytes, "host-key-session-a", None), &[])
        .await
        .expect("an identical retry from the originator is idempotent");
    assert_eq!(
        repeat.disposition,
        crate::ProcessRegistrationDisposition::Existing
    );
    assert_eq!(repeat.record.id, first.record.id);

    let mut changed = host_keyed(bytes, "host-key-session-a", None);
    changed.input = std::sync::Arc::new(ProcessInput::External {
        metadata: serde_json::json!({"suite": "changed-host-content"}),
    });
    let error = registry
        .register_process_reporting_disposition(changed, &[])
        .await
        .expect_err("a changed-content host retry under a retained key is refused");
    assert_start_key_conflict(&error, bytes);

    let retained = registry
        .get_process(&first.record.id)
        .await
        .expect("read the retained process")
        .expect("the retained process survives the refused starts");
    assert_eq!(retained, first.record, "A's row is unchanged");
    let rows = registry
        .list_processes(&crate::ProcessListFilter::default())
        .await
        .expect("list processes");
    assert_eq!(
        rows.iter()
            .filter(|row| row.start_key.as_ref() == Some(&crate::StartKey::for_host(bytes)))
            .count(),
        1,
        "one process holds the key"
    );
}

/// ADR 0107 (FIG-4111): a start-key conflict names the key and nothing else.
/// A host key is global, so the process it is bound to may be another
/// originator's: the refusal's `Display`, its `Debug` and the runtime error it
/// becomes at the effect boundary omit the retained process's id, originator
/// and input metadata.
///
/// Red on the parent commit, whose durable-identity conflict named the
/// retained process's id.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_start_key_conflict_names_no_retained_process(registry: Arc<dyn ProcessRegistry>) {
    let bytes = "content-free-conflict";
    let retained = registry
        .register_process(host_keyed(bytes, "conflict-owner-session", None))
        .await
        .expect("the owner starts under the key");
    let error = registry
        .register_process_reporting_disposition(
            host_keyed(bytes, "conflict-other-session", None),
            &[],
        )
        .await
        .expect_err("another originator's start under the key conflicts");
    assert_start_key_conflict(&error, bytes);
    let runtime = crate::RuntimeEffectControllerError::from(error.clone()).into_runtime_error();
    assert_eq!(runtime.code.as_str(), "process_start_key_conflict");
    assert!(runtime.is_terminal() && !runtime.is_retryable());
    let turn_failure = error
        .clone()
        .into_turn_failure(crate::RuntimeErrorCode::Plugin);
    for rendered in [
        error.to_string(),
        format!("{error:?}"),
        runtime.to_string(),
        format!("{runtime:?}"),
        turn_failure.to_string(),
        format!("{turn_failure:?}"),
    ] {
        for leaked in [
            retained.id.as_str(),
            "conflict-owner-session",
            "input-metadata-of-a",
        ] {
            assert!(
                !rendered.contains(leaked),
                "the conflict leaks `{leaked}` of the retained process: {rendered}"
            );
        }
    }
}

/// ADR 0107 (FIG-4111): the wake target is part of the start a host key
/// fences. A retry from the same originator naming another wake session would
/// otherwise be told its start exists while the retained process's work
/// lands on the first session.
///
/// Red on the parent commit, which never compared the wake target and
/// answered the retry `Existing`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_host_retry_with_another_wake_target_conflicts(registry: Arc<dyn ProcessRegistry>) {
    let bytes = "wake-target-fence";
    let first = registry
        .register_process(host_keyed(bytes, "wake-owner", Some("wake-first")))
        .await
        .expect("start waking the first session");
    let repeat = registry
        .register_process_reporting_disposition(
            host_keyed(bytes, "wake-owner", Some("wake-first")),
            &[],
        )
        .await
        .expect("an identical retry is idempotent");
    assert_eq!(repeat.record.id, first.id);
    for other_wake in [Some("wake-second"), None] {
        let error = registry
            .register_process_reporting_disposition(
                host_keyed(bytes, "wake-owner", other_wake),
                &[],
            )
            .await
            .expect_err("a retry naming another wake target is refused");
        assert_start_key_conflict(&error, bytes);
    }
}

/// ADR 0107 (FIG-4111): once a host key's process is pruned, the key starts a
/// new process with a new id for any originator, and the pruned process's
/// handle refuses as no longer retained, never as the new process.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_host_start_key_after_prune_starts_new_for_any_originator(
    registry: Arc<dyn ProcessRegistry>,
) {
    let bytes = "global-key-after-prune";
    let first = registry
        .register_process(host_keyed(bytes, "prune-owner-a", None))
        .await
        .expect("A starts under the key");
    registry
        .complete_process(
            &first.id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"process": "a"}),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete A's process");
    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune A's process");

    let second = registry
        .register_process_reporting_disposition(host_keyed(bytes, "prune-other-b", None), &[])
        .await
        .expect("B starts under the pruned process's key");
    assert_eq!(
        second.disposition,
        crate::ProcessRegistrationDisposition::Created,
        "a pruned process no longer holds its key, for any originator"
    );
    assert_ne!(second.record.id, first.id, "a minted id is never reused");
    assert!(
        matches!(
            registry.get_process(&first.id).await,
            Err(PluginError::ProcessNoLongerRetained { .. })
        ),
        "A's handle refuses; it never resolves to B's process"
    );
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
    let key = crate::StartKey::for_host("observer-registration");
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
                crate::DERIVED_START_KEYS,
                &format!("concurrent-start-key-{round}"),
                "concurrent-start-subscription",
                "incarnation",
                1,
            )
        } else {
            crate::StartKey::for_host(format!("concurrent-start-key-{round}"))
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
    let key = crate::StartKey::for_host("start-key-after-prune");
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
