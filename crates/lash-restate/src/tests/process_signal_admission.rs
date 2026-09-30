//! A signal's admission is its first recorded append (FIG-4298, FIG-4301).
//!
//! The append that admits a signal selects the wait it resolves, in the
//! store transaction that inserts its event, and the recorded step that ran
//! it retains the admitted signal beside the event. Every retry, redelivery
//! and partial replay is served from that admission:
//!
//! * a signal redelivered after the process moved on to its next wait for the
//!   name resolves the wait it was admitted to, never the next one, whether
//!   its first delivery finished or died between its append and its journal;
//! * a partial replay of a recorded admission under a changed signal is a
//!   typed divergence before anything resolves.
//!
//! Each law runs on the Restate double over SQLite memory and file stores,
//! and over PostgreSQL in the service leg (`postgres_ingress`).

use super::*;
use lash_core::TestProcessRegistryWriteExt as _;

const SIGNAL: &str = "ready";

fn signal_event_type() -> String {
    lash_core::runtime::process_signal_event_type(SIGNAL).expect("valid signal event type")
}

/// A registry over a SQLite file store in `dir`.
pub(super) async fn file_process_registry(dir: &tempfile::TempDir) -> Arc<dyn ProcessRegistry> {
    lash_sqlite_store::SqliteStoreSet::open(dir.path().join("signal-admission"))
        .await
        .expect("open the SQLite file store set")
        .process_registry()
}

/// A started process parked on its first wait for [`SIGNAL`].
async fn parked_signal_target(registry: &Arc<dyn ProcessRegistry>) -> ProcessId {
    let record = registry
        .register_process(executed_registration().with_extra_event_types([
            lash_core::ProcessEventType {
                name: signal_event_type(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            },
        ]))
        .await
        .expect("register the signal target");
    registry
        .record_first_started(
            &record.id,
            lash_core::ProcessStarted {
                owner: lash_core::LeaseOwnerIdentity::engine_process_execution(
                    &record.id,
                    "signal-admission",
                ),
                attempt: 1,
                started_at_ms: 1,
                generation: None,
                build_generation: None,
            },
        )
        .await
        .expect("start the signal target");
    park_at(registry, &record.id, 1).await;
    record.id
}

/// Park `process_id` on its `ordinal`th wait for [`SIGNAL`], as the process
/// does once it consumed the signal before it.
async fn park_at(registry: &Arc<dyn ProcessRegistry>, process_id: &ProcessId, ordinal: u64) {
    registry
        .set_process_wait(
            process_id,
            lash_core::WaitState {
                kind: lash_core::WaitKind::Signal {
                    name: SIGNAL.to_string(),
                    event_type: signal_event_type(),
                    key: lash_core::runtime::process_signal_wait_key(process_id, SIGNAL, ordinal),
                    ordinal,
                },
                since_ms: ordinal,
            },
        )
        .await
        .expect("park on the signal wait");
}

fn signal(
    process_id: &ProcessId,
    signal_id: &str,
    payload: serde_json::Value,
) -> lash_core::ProcessSignal {
    lash_core::ProcessSignal::new(
        lash_core::ProcessSignalIdentity::new(process_id.clone(), SIGNAL, signal_id)
            .expect("valid signal identity"),
        payload,
    )
}

fn signal_envelope(
    effect: &str,
    process_id: &ProcessId,
    signal_id: &str,
    payload: serde_json::Value,
) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::Process, effect),
        RuntimeEffectCommand::process(ProcessCommand::Signal {
            signal: signal(process_id, signal_id, payload),
        }),
    )
}

fn wait_key(process_id: &ProcessId, ordinal: u64) -> lash_core::AwaitEventKey {
    test_restate_await_event_key(
        &ExecutionScope::process(process_id.clone()),
        AwaitEventWaitIdentity::process_signal(process_id.clone(), SIGNAL, ordinal),
    )
    .expect("signal wait key")
}

/// Every wait `context` resolved, in order, with its resolution.
fn resolutions(
    context: &ReplayableRecordingContext,
) -> Vec<(lash_core::AwaitEventKey, Resolution)> {
    context
        .events
        .resolved_events
        .lock_recover()
        .iter()
        .map(|resolved| (resolved.key.clone(), resolved.resolution.clone()))
        .collect()
}

async fn signal_events(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> Vec<lash_core::ProcessEvent> {
    lash_core::ProcessEventLogTestSupport::full_event_window(registry.as_ref(), process_id, 0)
        .await
        .expect("read the event log")
        .into_iter()
        .filter(|event| event.event_type == signal_event_type())
        .collect()
}

/// Signal A reaches the ordinal-1 wait; the process consumes it and parks
/// on ordinal 2; A is redelivered by another owning invocation, with a
/// journal of its own. The redelivery is served A's admitted event and wait:
/// ordinal 2 stays unresolved until a distinct signal B arrives, and the log
/// holds one A.
pub(super) async fn duplicate_signal_after_wait_advances_law(registry: Arc<dyn ProcessRegistry>) {
    let process_id = parked_signal_target(&registry).await;
    let a = serde_json::json!({ "signal": "a" });
    let b = serde_json::json!({ "signal": "b" });

    let first = Arc::new(ReplayableRecordingContext::default());
    RestateRuntimeEffectController::new_for_test(first.clone())
        .execute_effect(
            signal_envelope("signal-a", &process_id, "a", a.clone()),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("deliver A");
    assert_eq!(
        resolutions(&first),
        vec![(wait_key(&process_id, 1), Resolution::Ok(a.clone()))]
    );

    park_at(&registry, &process_id, 2).await;

    let redelivery = Arc::new(ReplayableRecordingContext::default());
    RestateRuntimeEffectController::new_for_test(redelivery.clone())
        .execute_effect(
            signal_envelope("signal-a-redelivered", &process_id, "a", a.clone()),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("redeliver A");
    assert_eq!(
        resolutions(&redelivery),
        vec![(wait_key(&process_id, 1), Resolution::Ok(a.clone()))],
        "a redelivered A resolves only the wait it was admitted to"
    );
    let events = signal_events(&registry, &process_id).await;
    assert_eq!(events.len(), 1, "the log holds one A: {events:#?}");
    assert_eq!(
        events[0].semantics.signal_wait,
        Some(lash_core::ProcessSignalWaitBinding { ordinal: 1 })
    );

    let second = Arc::new(ReplayableRecordingContext::default());
    RestateRuntimeEffectController::new_for_test(second.clone())
        .execute_effect(
            signal_envelope("signal-b", &process_id, "b", b.clone()),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("deliver B");
    assert_eq!(
        resolutions(&second),
        vec![(wait_key(&process_id, 2), Resolution::Ok(b))],
        "B, a distinct signal, is what resolves the ordinal-2 wait"
    );
    assert_eq!(signal_events(&registry, &process_id).await.len(), 2);
}

/// A's append commits while the process waits on ordinal 1, and the attempt
/// dies before the append's result reaches the journal, so nothing resolves.
/// The wait moves to ordinal 2 before the retry; the retry is served A's
/// admitted event and resolves the ordinal-1 wait its append selected.
pub(super) async fn duplicate_signal_after_append_to_journal_crash_law(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = parked_signal_target(&registry).await;
    let a = serde_json::json!({ "signal": "a" });
    registry
        .append_event(
            &process_id,
            signal(&process_id, "a", a.clone()).append_request(),
        )
        .await
        .expect("the lost attempt's append");
    park_at(&registry, &process_id, 2).await;

    let retry = Arc::new(ReplayableRecordingContext::default());
    RestateRuntimeEffectController::new_for_test(retry.clone())
        .execute_effect(
            signal_envelope("signal-a", &process_id, "a", a.clone()),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("retry A");
    assert_eq!(
        resolutions(&retry),
        vec![(wait_key(&process_id, 1), Resolution::Ok(a))],
        "the retry resolves the wait A's first append was admitted to"
    );
    assert_eq!(signal_events(&registry, &process_id).await.len(), 1);
}

/// Signal S with payload A records its append; the attempt dies before its
/// resolution and the outer effect entry are journaled. S is reconstructed
/// at the same effect address with payload B: the replay refuses it as a
/// divergence before anything resolves, and A's admission, event and wait
/// are untouched. The unchanged A replays and resolves as recorded.
pub(super) async fn partial_signal_replay_rejects_changed_request_law(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = parked_signal_target(&registry).await;
    let a = serde_json::json!({ "signal": "a" });
    let b = serde_json::json!({ "signal": "b" });

    let live = Arc::new(ReplayableRecordingContext::default());
    RestateRuntimeEffectController::new_for_test(live.clone())
        .execute_effect(
            signal_envelope("partial-signal", &process_id, "s", a.clone()),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("deliver A");
    let admission = live.recorded_process_command_facts();
    assert_eq!(admission.len(), 1, "the append admission is recorded");
    let recorded = admission.values().next().expect("the admission");
    assert_eq!(
        recorded["Ok"]["Ok"]["signal"],
        serde_json::to_value(signal(&process_id, "s", a.clone())).expect("encode the signal"),
        "the admission retains the signal it admitted: {recorded}"
    );
    let events_before = format!("{:?}", signal_events(&registry, &process_id).await);

    // The append is recorded; its resolution and the outer effect entry are not.
    let cut = Arc::new(ReplayableRecordingContext::default());
    cut.install_recorded_process_command_facts(admission.clone());
    cut.start_replay_allowing_journal_extension();
    let changed = RestateRuntimeEffectController::new_for_test(cut.clone())
        .execute_effect(
            signal_envelope("partial-signal", &process_id, "s", b),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect_err("a changed signal under the admitted identity diverges");
    assert_eq!(
        changed.code,
        lash_core::RuntimeErrorCode::EffectReplayDivergence,
        "{changed:?}"
    );
    assert!(
        resolutions(&cut).is_empty(),
        "nothing resolves before the admitted signal is checked"
    );
    assert_eq!(
        cut.recorded_process_command_facts(),
        admission,
        "the admission is unchanged"
    );
    assert_eq!(
        format!("{:?}", signal_events(&registry, &process_id).await),
        events_before,
        "A's event is unchanged"
    );

    let unchanged = Arc::new(ReplayableRecordingContext::default());
    unchanged.install_recorded_process_command_facts(admission);
    unchanged.start_replay_allowing_journal_extension();
    RestateRuntimeEffectController::new_for_test(unchanged.clone())
        .execute_effect(
            signal_envelope("partial-signal", &process_id, "s", a.clone()),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("the unchanged signal replays");
    assert_eq!(
        resolutions(&unchanged),
        vec![(wait_key(&process_id, 1), Resolution::Ok(a))]
    );
}

#[tokio::test]
pub(super) async fn duplicate_signal_after_wait_advances_does_not_resolve_next_wait() {
    duplicate_signal_after_wait_advances_law(process_registry()).await;
    duplicate_signal_after_append_to_journal_crash_law(process_registry()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    duplicate_signal_after_wait_advances_law(file_process_registry(&dir).await).await;
    let dir = tempfile::tempdir().expect("tempdir");
    duplicate_signal_after_append_to_journal_crash_law(file_process_registry(&dir).await).await;
}

#[tokio::test]
pub(super) async fn partial_signal_replay_rejects_changed_request_before_resolution() {
    partial_signal_replay_rejects_changed_request_law(process_registry()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    partial_signal_replay_rejects_changed_request_law(file_process_registry(&dir).await).await;
}
