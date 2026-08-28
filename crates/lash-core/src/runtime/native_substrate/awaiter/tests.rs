//! Unit tests for the process awaiter, watched registry, and work driver.

use std::sync::{Arc, Mutex};

use super::*;
use crate::{
    AbandonRequest, ProcessEventAppendRequest, ProcessEventSink, ProcessExternalRef, ProcessInput,
    ProcessProvenance, ProcessRegistration, ProcessStarted, ProjectionWatermark,
    TestLocalProcessRegistry, TestProcessRegistryWriteExt, WaitState, WatchedRegistry,
    watch_process_registry, watch_process_registry_with_sink,
};
use lash_sansio::sync::MutexExt;

fn watched_parts(watched: WatchedRegistry) -> (Arc<dyn ProcessRegistry>, ProcessChangeHub) {
    (Arc::clone(watched.registry()), watched.hub().clone())
}

fn registration(process_id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        process_id,
        ProcessInput::External {
            metadata: serde_json::json!({}),
        },
        crate::RecoveryContract::ExternallyOwned,
        ProcessProvenance::host(),
    )
}

fn plain_event_type(name: &str) -> crate::ProcessEventType {
    crate::ProcessEventType {
        name: name.to_string(),
        payload_schema: crate::LashSchema::any(),
        semantics: crate::ProcessEventSemanticsSpec::default(),
    }
}

fn registration_with_events(process_id: &str, event_types: &[&str]) -> ProcessRegistration {
    registration(process_id)
        .with_extra_event_types(event_types.iter().map(|name| plain_event_type(name)))
}

/// Records `(event_type, sequence)` in emit order for sink assertions.
#[derive(Clone, Default)]
struct CollectingSink {
    events: Arc<Mutex<Vec<(String, u64)>>>,
}

impl CollectingSink {
    fn collected(&self) -> Vec<(String, u64)> {
        self.events.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl ProcessEventSink for CollectingSink {
    async fn emit(&self, event: &ProcessEvent) {
        self.events
            .lock_recover()
            .push((event.event_type.clone(), event.sequence));
    }
}

fn success(value: serde_json::Value) -> ProcessAwaitOutput {
    ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(value))
}

/// ADR 0016 pins the awaiter's polling cadence: a 25ms floor, doubling
/// backoff, and a 1s cap. Changing any of the three alters every store-only
/// deployment's wait economics, so the exact schedule is asserted here.
#[test]
fn backoff_schedule_has_25ms_floor_doubling_to_1s_cap() {
    assert_eq!(AWAIT_BACKOFF_MIN, Duration::from_millis(25));
    assert_eq!(AWAIT_BACKOFF_MAX, Duration::from_secs(1));

    let mut backoff = AWAIT_BACKOFF_MIN;
    let mut schedule = vec![backoff];
    while backoff < AWAIT_BACKOFF_MAX {
        backoff = next_backoff(backoff);
        schedule.push(backoff);
    }
    assert_eq!(
        schedule,
        [25, 50, 100, 200, 400, 800, 1000]
            .into_iter()
            .map(Duration::from_millis)
            .collect::<Vec<_>>(),
        "the backoff doubles from the 25ms floor and saturates at the 1s cap"
    );
    assert_eq!(
        next_backoff(AWAIT_BACKOFF_MAX),
        AWAIT_BACKOFF_MAX,
        "the cap is absorbing"
    );
}

/// ADR 0017: the decorator delegates `prune_terminal_processes` without a
/// hub bump — pruned rows are terminal, so their waiters resolved long ago
/// and a tick would only wake unrelated subscribers spuriously.
#[tokio::test]
async fn prune_through_decorator_does_not_bump_the_hub() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let (registry, hub) = watched_parts(watch_process_registry(raw));
    registry
        .register_process(registration("proc-terminal"))
        .await
        .expect("register terminal");
    registry
        .complete_process(
            "proc-terminal",
            success(serde_json::json!("done")),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");
    registry
        .register_process(registration("proc-live"))
        .await
        .expect("register live");

    // Subscribe after the mutations above so only post-subscription bumps
    // are observable.
    let mut terminal_rx = hub.subscribe("proc-terminal");
    let mut live_rx = hub.subscribe("proc-live");
    terminal_rx.mark_unchanged();
    live_rx.mark_unchanged();

    let report = registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune");
    assert_eq!(report.pruned_processes, 1, "the terminal process pruned");

    assert!(
        !terminal_rx.has_changed().expect("terminal sender open"),
        "prune must not bump the pruned process's hub entry"
    );
    assert!(
        !live_rx.has_changed().expect("live sender open"),
        "prune must not bump surviving processes' hub entries"
    );
}

#[tokio::test]
async fn hub_subscribe_then_notify_wakes_and_gc_drops_empty_entry() {
    let hub = ProcessChangeHub::new();
    let mut rx = hub.subscribe("proc");
    hub.notify("proc");
    tokio::time::timeout(Duration::from_millis(100), rx.changed())
        .await
        .expect("notify should wake")
        .expect("sender remains open");

    drop(rx);
    hub.notify("proc");
    assert_eq!(hub.tracked_processes(), 0);
}

#[tokio::test]
async fn await_event_returns_historical_event_immediately() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let (registry, hub) = watched_parts(watch_process_registry(raw));
    registry
        .register_process(registration("proc"))
        .await
        .expect("register");
    let appended = registry
        .append_event(
            "proc",
            ProcessEventAppendRequest::cancel_requested("proc", Some("stop".to_string())),
        )
        .await
        .expect("append");

    let event = NativeProcessAwaiter::new(Arc::clone(&registry), hub)
        .await_event("proc", "process.cancel_requested", 0)
        .await
        .expect("await event");
    assert_eq!(event.sequence, appended.event.sequence);
}

#[tokio::test]
async fn await_terminal_unknown_process_errors() {
    let registry = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let err = NativeProcessAwaiter::for_registry(registry)
        .await_terminal("missing")
        .await
        .expect_err("unknown process should error");
    assert!(
        matches!(
            err,
            PluginError::ProcessUnknown { ref process_id } if process_id == "missing"
        ),
        "unknown process should return ProcessUnknown, got: {err:?}"
    );
}

#[tokio::test]
async fn await_terminal_propagates_process_store_read_errors() {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    registry
        .set_process_read_error(Some(PluginError::Session(
            "process store read failed".to_string(),
        )))
        .await;
    let err = NativeProcessAwaiter::for_registry(registry)
        .await_terminal("unreadable")
        .await
        .expect_err("store read failure should surface");
    assert!(
        matches!(
            err,
            PluginError::Session(ref message) if message == "process store read failed"
        ),
        "store read failure should remain a session error, got: {err:?}"
    );
}

#[tokio::test]
async fn polling_awaiter_resolves_via_backoff() {
    let registry = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    registry
        .register_process(registration("proc"))
        .await
        .expect("register");
    let writer = Arc::clone(&registry);
    crate::task::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        writer
            .complete_process(
                "proc",
                success(serde_json::json!({ "ok": true })),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete");
    });

    let output = tokio::time::timeout(
        Duration::from_secs(1),
        NativeProcessAwaiter::for_registry(registry).await_terminal("proc"),
    )
    .await
    .expect("polling await timeout")
    .expect("await terminal");
    assert_eq!(output, success(serde_json::json!({ "ok": true })));
}

#[tokio::test]
async fn watched_awaiter_observes_terminal_without_lost_wakeup() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let (registry, hub) = watched_parts(watch_process_registry(raw));
    registry
        .register_process(registration("proc"))
        .await
        .expect("register");
    let awaiter = NativeProcessAwaiter::new(Arc::clone(&registry), hub);
    let waiter = crate::task::spawn(async move { awaiter.await_terminal("proc").await });
    registry
        .complete_process(
            "proc",
            success(serde_json::json!("done")),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    let output = tokio::time::timeout(Duration::from_millis(200), waiter)
        .await
        .expect("watched await timeout")
        .expect("join")
        .expect("await terminal");
    assert_eq!(output, success(serde_json::json!("done")));
}

#[tokio::test]
async fn watched_registry_bumps_on_mutations() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let (registry, hub) = watched_parts(watch_process_registry(raw));
    let mut rx = hub.subscribe("proc");
    registry
        .register_process(registration("proc"))
        .await
        .expect("register");
    tokio::time::timeout(Duration::from_millis(100), rx.changed())
        .await
        .expect("register bump")
        .expect("sender remains open");

    registry
        .append_event(
            "proc",
            ProcessEventAppendRequest::cancel_requested("proc", None),
        )
        .await
        .expect("append");
    tokio::time::timeout(Duration::from_millis(100), rx.changed())
        .await
        .expect("append bump")
        .expect("sender remains open");
}

#[tokio::test]
async fn sink_receives_appended_events_in_order() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let sink = CollectingSink::default();
    let (registry, _hub) = watched_parts(watch_process_registry_with_sink(
        raw,
        Some(Arc::new(sink.clone())),
    ));
    registry
        .register_process(registration_with_events(
            "proc",
            &["producer.a", "producer.b"],
        ))
        .await
        .expect("register");
    registry
        .append_event(
            "proc",
            ProcessEventAppendRequest::new("producer.a", serde_json::json!({})),
        )
        .await
        .expect("append a");
    registry
        .append_event(
            "proc",
            ProcessEventAppendRequest::new("producer.b", serde_json::json!({})),
        )
        .await
        .expect("append b");

    let collected = sink.collected();
    assert_eq!(
        collected
            .iter()
            .map(|(event_type, _)| event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["producer.a", "producer.b"],
        "the sink must observe appended events after their write, in append order"
    );
    assert!(collected[0].1 < collected[1].1);
}

#[tokio::test]
async fn sink_absent_leaves_appends_unchanged() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let (registry, _hub) = watched_parts(watch_process_registry_with_sink(raw, None));
    registry
        .register_process(registration_with_events("proc", &["producer.a"]))
        .await
        .expect("register");
    let appended = registry
        .append_event(
            "proc",
            ProcessEventAppendRequest::new("producer.a", serde_json::json!({})),
        )
        .await
        .expect("append succeeds with no sink installed");
    assert!(appended.event.sequence > 0);
}

#[tokio::test]
async fn sink_receives_complete_process_terminal_append() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let sink = CollectingSink::default();
    let (registry, _hub) = watched_parts(watch_process_registry_with_sink(
        raw,
        Some(Arc::new(sink.clone())),
    ));
    registry
        .register_process(registration_with_events("proc", &["producer.a"]))
        .await
        .expect("register");
    registry
        .append_event(
            "proc",
            ProcessEventAppendRequest::new("producer.a", serde_json::json!({})),
        )
        .await
        .expect("explicit append");
    registry
        .complete_process(
            "proc",
            success(serde_json::json!("done")),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    let collected = sink.collected();
    assert_eq!(
        collected
            .iter()
            .map(|(event_type, _)| event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["producer.a", "process.completed"],
        "the sink must observe terminal events appended through completion verbs"
    );
    assert!(
        collected[0].1 < collected[1].1,
        "terminal event sequences must follow preceding appends"
    );
}

#[tokio::test]
async fn sink_receives_runtime_lifecycle_events_in_order() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let sink = CollectingSink::default();
    let (registry, _hub) = watched_parts(watch_process_registry_with_sink(
        raw,
        Some(Arc::new(sink.clone())),
    ));
    let mut lifecycle_registration = ProcessRegistration::new(
        "proc",
        ProcessInput::Engine {
            kind: "test".to_string(),
            payload: serde_json::json!({}),
        },
        crate::RecoveryContract::Rerunnable,
        ProcessProvenance::host(),
    );
    lifecycle_registration.env_ref = Some(crate::ProcessExecutionEnvRef::new("process-env:test"));
    registry
        .register_process(lifecycle_registration)
        .await
        .expect("register");
    registry
        .record_first_started(
            "proc",
            ProcessStarted {
                owner: crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .expect("record first start");
    let wait = WaitState {
        kind: crate::WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: "process:proc:signal.ready:1".to_string(),
            ordinal: 1,
        },
        since_ms: 2,
    };
    registry
        .set_process_wait("proc", wait)
        .await
        .expect("enter wait");
    registry
        .clear_process_wait("proc")
        .await
        .expect("clear wait");
    registry
        .set_external_ref(
            "proc",
            ProcessExternalRef {
                backend: "test".to_string(),
                id: "external".to_string(),
                metadata: None,
            },
        )
        .await
        .expect("set external ref");
    registry
        .request_process_abandon(
            "proc",
            AbandonRequest {
                requested_by: "test".to_string(),
                requested_at_ms: 3,
                reason: None,
            },
        )
        .await
        .expect("request abandon");

    let collected = sink.collected();
    assert_eq!(
        collected
            .iter()
            .map(|(event_type, _)| event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "process.first_started",
            "process.waiting",
            "process.resumed",
            "process.external_ref_set",
            "process.abandon_requested",
        ],
        "the sink must observe every runtime lifecycle append"
    );
    assert!(
        collected.windows(2).all(|events| events[0].1 < events[1].1),
        "runtime lifecycle event sequences must be strictly ordered"
    );
}

#[tokio::test]
async fn sink_present_still_bumps_hub_on_append() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let sink = CollectingSink::default();
    let (registry, hub) =
        watched_parts(watch_process_registry_with_sink(raw, Some(Arc::new(sink))));
    let mut rx = hub.subscribe("proc");
    registry
        .register_process(registration_with_events("proc", &["producer.a"]))
        .await
        .expect("register");
    tokio::time::timeout(Duration::from_millis(100), rx.changed())
        .await
        .expect("register bump")
        .expect("sender remains open");
    registry
        .append_event(
            "proc",
            ProcessEventAppendRequest::new("producer.a", serde_json::json!({})),
        )
        .await
        .expect("append");
    tokio::time::timeout(Duration::from_millis(100), rx.changed())
        .await
        .expect("append bump with a sink installed")
        .expect("sender remains open");
}

#[tokio::test]
async fn native_awaiter_returns_an_already_terminal_process() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let (registry, hub) = watched_parts(watch_process_registry(raw));
    let awaiter = NativeProcessAwaiter::new(Arc::clone(&registry), hub);
    registry
        .register_process(registration("proc"))
        .await
        .expect("register");
    registry
        .complete_process(
            "proc",
            success(serde_json::json!("ready")),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    let output = awaiter
        .await_terminal("proc")
        .await
        .expect("await terminal");
    assert_eq!(output, success(serde_json::json!("ready")));
}

/// A caller-departed row is refused because no writer can terminalize it.
#[tokio::test]
async fn native_awaiter_refuses_await_on_caller_departed_row() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let (registry, hub) = watched_parts(watch_process_registry(raw));
    let awaiter = NativeProcessAwaiter::new(Arc::clone(&registry), hub);
    registry
        .register_process(registration("proc"))
        .await
        .expect("register");
    registry
        .record_caller_departure("proc")
        .await
        .expect("record caller departure");

    let error = awaiter
        .await_terminal("proc")
        .await
        .expect_err("awaiting a caller-departed row must be refused, not parked");
    assert!(
        matches!(
            error,
            PluginError::ProcessCallerDeparted { ref process_id } if process_id == "proc"
        ),
        "unexpected refusal: {error}"
    );
}

/// FIG-1744 / ADR 0019: CallerDeparted refusal wins over a recorded terminal
/// outcome.
#[tokio::test]
async fn caller_departed_refuses_before_terminal_outcome() {
    let raw = Arc::new(TestLocalProcessRegistry::default());
    let (registry, hub) = watched_parts(watch_process_registry(
        Arc::clone(&raw) as Arc<dyn ProcessRegistry>
    ));
    let awaiter = NativeProcessAwaiter::new(Arc::clone(&registry), hub);

    registry
        .register_process(registration("proc-departed"))
        .await
        .expect("register");

    let mut record = registry
        .get_process("proc-departed")
        .await
        .expect("get_process")
        .expect("record exists");
    record.status = crate::ProcessStatus::CallerDeparted;
    record.outcome = Some(success(serde_json::json!("completed-value")));

    raw.set_process_read_override(record).await;
    let awaiter_err = awaiter
        .await_terminal("proc-departed")
        .await
        .expect_err("awaiter must refuse CallerDeparted even if outcome is present");
    assert!(
        matches!(
            awaiter_err,
            PluginError::ProcessCallerDeparted { ref process_id } if process_id == "proc-departed"
        ),
        "awaiter expected ProcessCallerDeparted refusal, got: {awaiter_err:?}"
    );
}

/// Sim-style race: many waiters attach to one process and completion fires
/// while they are mid-flight between their subscribe and their first read.
/// The change hub must resolve every one with identical output — no lost
/// wakeups, no divergent results (ADR 0016).
#[tokio::test]
async fn concurrent_waiters_all_resolve_with_identical_output_on_completion() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let (registry, hub) = watched_parts(watch_process_registry(raw));
    registry
        .register_process(registration("proc"))
        .await
        .expect("register");

    const WAITERS: usize = 16;
    let barrier = Arc::new(tokio::sync::Barrier::new(WAITERS + 1));
    let mut waiters = Vec::with_capacity(WAITERS);
    for _ in 0..WAITERS {
        let awaiter = NativeProcessAwaiter::new(Arc::clone(&registry), hub.clone());
        let barrier = Arc::clone(&barrier);
        waiters.push(crate::task::spawn(async move {
            barrier.wait().await;
            awaiter.await_terminal("proc").await
        }));
    }
    // Release every waiter, then complete at once so completion races their
    // first read and subscribe.
    barrier.wait().await;
    let output = success(serde_json::json!({ "raced": true }));
    registry
        .complete_process(
            "proc",
            output.clone(),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    for waiter in waiters {
        let resolved = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("each racing waiter resolves under 2s")
            .expect("join waiter")
            .expect("await terminal");
        assert_eq!(
            resolved, output,
            "every concurrent waiter resolves with identical terminal output"
        );
    }
}

/// Records seen vs. dropped emit sequences, dropping even sequences to model
/// best-effort push loss.
#[derive(Clone, Default)]
struct LossySink {
    seen: Arc<Mutex<Vec<u64>>>,
    dropped: Arc<Mutex<Vec<u64>>>,
}

#[async_trait::async_trait]
impl ProcessEventSink for LossySink {
    async fn emit(&self, event: &ProcessEvent) {
        if event.sequence.is_multiple_of(2) {
            self.dropped.lock_recover().push(event.sequence);
        } else {
            self.seen.lock_recover().push(event.sequence);
        }
    }
}

/// Sim-style sink loss: a sink that drops a fraction of emits still leaves
/// the durable log complete. Reconciling from `events_after` at terminal
/// recovers every event the push feed missed — ADR 0017's "push loss never
/// loses truth".
#[tokio::test]
async fn lossy_sink_still_reconciles_complete_log_from_events_after() {
    let raw = Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
    let sink = LossySink::default();
    let (registry, _hub) = watched_parts(watch_process_registry_with_sink(
        raw,
        Some(Arc::new(sink.clone())),
    ));
    registry
        .register_process(registration_with_events("proc", &["producer.step"]))
        .await
        .expect("register");

    const EVENTS: u64 = 6;
    for _ in 0..EVENTS {
        registry
            .append_event(
                "proc",
                ProcessEventAppendRequest::new("producer.step", serde_json::json!({})),
            )
            .await
            .expect("append");
    }
    // Terminal events remain durable for reconciliation if that push is dropped.
    registry
        .complete_process(
            "proc",
            success(serde_json::json!("done")),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    // The push feed genuinely lost some events...
    assert!(
        !sink.dropped.lock_recover().is_empty(),
        "the lossy sink must drop at least one emit for the scenario to be meaningful"
    );
    assert!(
        (sink.seen.lock_recover().len() as u64) < EVENTS,
        "the sink observed fewer events than were appended"
    );
    let reconciled = registry
        .events_after("proc", 0)
        .await
        .expect("events")
        .into_iter()
        .filter(|event| event.event_type == "producer.step")
        .map(|event| event.sequence)
        .collect::<Vec<_>>();
    assert_eq!(
        reconciled.len(),
        EVENTS as usize,
        "events_after reconciles the complete non-terminal log despite push loss"
    );
    assert!(
        reconciled.windows(2).all(|events| events[0] < events[1]),
        "the reconciled durable log must remain strictly ordered"
    );
}
