//! [`ProcessRegistry`] conformance for the coordination seams: awaiter
//! waits over the change hub, watched-decorator semantics, and terminal
//! retention (`prune_terminal_processes`). Split from `process_registry.rs`
//! to keep each file within the test-support size budget; the cases run from
//! the same `process_registry` entry points.

use super::process_registry::{
    plain_event_type, process_lease_owner, registration, wake_event_type,
};
use super::*;

/// A hub-backed [`ProcessAwaiter`](crate::ProcessAwaiter) resolves a terminal
/// wait promptly when completion fires from another task, and yields exactly the
/// output [`complete_process`](ProcessRegistry::complete_process) recorded. This
/// pins ADR 0016's "waits live on the awaiter over the change hub" claim for
/// every store: waiting is store-agnostic point reads plus in-process wakeups.
pub(super) async fn awaiter_cross_task_completion_resolves_promptly(
    registry: Arc<dyn ProcessRegistry>,
) {
    let (registry, hub) = crate::watch_process_registry(registry);
    registry
        .register_process(registration("proc-await-cross-task"))
        .await
        .expect("register");
    let awaiter = crate::ProcessAwaiter::new(Arc::clone(&registry), hub);
    let waiter =
        crate::task::spawn(async move { awaiter.await_terminal("proc-await-cross-task").await });

    // Complete from this task through the decorated handle so the hub bumps; the
    // waiter may still be between its subscribe and first read, which the awaiter
    // tolerates (it re-reads terminal state after subscribing).
    let output = ProcessAwaitOutput::Success {
        value: serde_json::json!({ "cross": "task", "n": 7 }),
        control: None,
    };
    registry
        .complete_process(
            "proc-await-cross-task",
            output.clone(),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");

    let resolved = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
        .await
        .expect("a cross-task terminal await must resolve comfortably under 2s")
        .expect("join awaiter task")
        .expect("await terminal output");
    assert_eq!(
        resolved, output,
        "the awaiter must resolve with exactly the output complete_process recorded"
    );
}

/// [`ProcessAwaiter::await_event`](crate::ProcessAwaiter::await_event) never
/// resolves against an event at or before its cursor: it returns the first
/// matching event strictly after the cursor, whether that event already exists
/// (history-before-wait) or arrives later. ADR 0016 moves event waits onto the
/// awaiter, so this cursor contract must hold identically for every store.
pub(super) async fn awaiter_await_event_never_returns_events_at_or_before_cursor(
    registry: Arc<dyn ProcessRegistry>,
) {
    let (registry, hub) = crate::watch_process_registry(registry);
    let awaiter = crate::ProcessAwaiter::new(Arc::clone(&registry), hub);
    registry
        .register_process(
            registration("proc-await-cursor")
                .with_extra_event_types([plain_event_type("signal.ping")]),
        )
        .await
        .expect("register");
    for _ in 0..2 {
        registry
            .append_event(
                "proc-await-cursor",
                ProcessEventAppendRequest::new("signal.ping", serde_json::json!({})),
            )
            .await
            .expect("append ping");
    }

    // History-before-wait: seq 1 is at the cursor and must be skipped; the first
    // match strictly after cursor 1 already exists at seq 2 and resolves at once.
    let existing = awaiter
        .await_event("proc-await-cursor", "signal.ping", 1)
        .await
        .expect("existing matching event after the cursor resolves");
    assert_eq!(
        existing.sequence, 2,
        "await_event returns the first matching event strictly after the cursor, never at it"
    );

    // Cursor at the latest sequence (2): no event at or before it may resolve, so
    // the wait stays pending until a later matching event arrives.
    let waiter_awaiter = awaiter.clone();
    let mut waiter = crate::task::spawn(async move {
        waiter_awaiter
            .await_event("proc-await-cursor", "signal.ping", 2)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        !waiter.is_finished(),
        "events at or before the cursor must never resolve await_event"
    );

    registry
        .append_event(
            "proc-await-cursor",
            ProcessEventAppendRequest::new("signal.ping", serde_json::json!({})),
        )
        .await
        .expect("append later ping");
    let later = tokio::time::timeout(std::time::Duration::from_secs(2), &mut waiter)
        .await
        .expect("the first event after the cursor resolves within bound")
        .expect("join awaiter task")
        .expect("await event output");
    assert_eq!(
        later.sequence, 3,
        "the first matching event strictly after the cursor resolves the pending wait"
    );
}

/// The [`watch_process_registry`](crate::watch_process_registry) decorator is a
/// transparent state pass-through: ADR 0016 says it "publishes in-process change
/// ticks without changing the registry trait". Every mutation made through the
/// decorated handle is visible unchanged on the raw inner handle, and reads
/// agree through both — proven per store so decoration cannot silently alter
/// state semantics on any backend.
pub(super) async fn watched_decorator_preserves_registry_semantics(
    registry: Arc<dyn ProcessRegistry>,
) {
    let raw = registry;
    let (decorated, _hub) = crate::watch_process_registry(Arc::clone(&raw));
    let scope = SessionScope::new("decorator-owner");
    let record_json =
        |record: &ProcessRecord| serde_json::to_value(record).expect("serialize process record");

    // register: the decorator writes straight through to the inner store.
    let registered = decorated
        .register_process(
            registration("proc-decorated")
                .with_extra_event_types([plain_event_type("producer.tick")]),
        )
        .await
        .expect("register via decorator");
    assert_eq!(
        record_json(
            &raw.get_process("proc-decorated")
                .await
                .expect("inner record")
        ),
        record_json(&registered),
        "a register through the decorator is visible unchanged on the inner handle"
    );

    // append: the same event row lands in the inner log, and reads agree.
    let appended = decorated
        .append_event(
            "proc-decorated",
            ProcessEventAppendRequest::new("producer.tick", serde_json::json!({})),
        )
        .await
        .expect("append via decorator");
    assert_eq!(
        serde_json::to_value(
            raw.events_after("proc-decorated", 0)
                .await
                .expect("inner events")
        )
        .expect("serialize inner events"),
        serde_json::to_value(vec![appended.event.clone()]).expect("serialize appended event"),
        "an append through the decorator lands unchanged in the inner event log"
    );
    assert_eq!(
        serde_json::to_value(
            decorated
                .events_after("proc-decorated", 0)
                .await
                .expect("decorated events")
        )
        .expect("serialize decorated events"),
        serde_json::to_value(
            raw.events_after("proc-decorated", 0)
                .await
                .expect("inner events")
        )
        .expect("serialize inner events"),
        "event reads are identical through the decorated and raw handles"
    );

    // grant: addressability recorded through the decorator is real store state.
    decorated
        .grant_handle(
            &scope,
            "proc-decorated",
            ProcessHandleDescriptor::new(Some("test"), Some("decorated")),
        )
        .await
        .expect("grant via decorator");
    assert!(
        raw.has_handle_grant(&scope, "proc-decorated")
            .await
            .expect("inner grant check"),
        "a grant through the decorator is visible on the inner handle"
    );

    // lease claim: the decorator persists the lease, so a competing owner on the
    // inner handle is fenced by the very lease the decorator wrote.
    let claimed = decorated
        .claim_process_lease(
            "proc-decorated",
            &process_lease_owner("decorator-owner-a"),
            60_000,
        )
        .await
        .expect("claim via decorator")
        .acquired()
        .expect("decorator claim acquired");
    match raw
        .claim_process_lease(
            "proc-decorated",
            &process_lease_owner("decorator-owner-b"),
            60_000,
        )
        .await
        .expect("competing claim resolves")
    {
        crate::ProcessLeaseClaimOutcome::Busy { holder } => assert_eq!(
            holder.lease_token, claimed.lease_token,
            "the inner handle observes the lease the decorator wrote"
        ),
        crate::ProcessLeaseClaimOutcome::Acquired(_) => {
            panic!(
                "a lease claimed through the decorator must fence a competing owner on the inner handle"
            )
        }
    }

    // complete: the terminal write is visible through the inner handle, and both
    // handles agree the process left the non-terminal worklist.
    decorated
        .complete_process(
            "proc-decorated",
            ProcessAwaitOutput::Success {
                value: serde_json::json!("done"),
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete via decorator");
    assert!(
        raw.get_process("proc-decorated")
            .await
            .expect("inner terminal record")
            .is_terminal(),
        "a completion through the decorator is visible as terminal on the inner handle"
    );
    assert!(
        decorated
            .list_non_terminal()
            .await
            .expect("decorated non-terminal")
            .is_empty()
            && raw
                .list_non_terminal()
                .await
                .expect("inner non-terminal")
                .is_empty(),
        "both handles agree the completed process left the non-terminal worklist"
    );
}

/// ADR 0017's prune contract across the full retention surface: pruning an old
/// terminal process physically deletes its events, wake deliveries, handle grants, and
/// lease rows, leaving a fresher terminal and every live process untouched. A
/// pruned id reads as unknown everywhere, list surfaces behave as if it never
/// existed, and prune is idempotent.
pub(super) async fn prune_respects_leases_grants_and_wake_deliveries(
    registry: Arc<dyn ProcessRegistry>,
) {
    let scope = SessionScope::new("prune-retention-owner");
    for id in ["proc-old", "proc-recent"] {
        registry
            .register_process(
                registration(id).with_extra_event_types([wake_event_type("producer.wake")]),
            )
            .await
            .expect("register wake process");
        registry
            .grant_handle(
                &scope,
                id,
                ProcessHandleDescriptor::new(Some("test"), Some(id)),
            )
            .await
            .expect("grant");
    }
    registry
        .register_process(registration("proc-live-retained"))
        .await
        .expect("register live");

    // proc-old accumulates two settled wake deliveries, a handle grant, and a
    // held lease before completing: one of every retention row type.
    registry
        .append_event(
            "proc-old",
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({ "wake_input": "old one" }),
            )
            .with_wake_target_scope(scope.clone()),
        )
        .await
        .expect("append old wake one");
    registry
        .append_event(
            "proc-old",
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({ "wake_input": "old two" }),
            )
            .with_wake_target_scope(scope.clone()),
        )
        .await
        .expect("append old wake two");
    for delivery in registry
        .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
        .await
        .expect("old pending wakes")
        .into_iter()
        .filter(|delivery| delivery.wake.process_id == "proc-old")
    {
        registry
            .mark_wake_enqueued(&delivery.delivery_id)
            .await
            .expect("settle old wake delivery");
    }
    let pre_prune_fencing = registry
        .claim_process_lease(
            "proc-old",
            &process_lease_owner("prune-lease-holder"),
            60_000,
        )
        .await
        .expect("claim old lease")
        .acquired()
        .expect("old lease acquired")
        .fencing_token;
    registry
        .complete_process(
            "proc-old",
            ProcessAwaitOutput::Success {
                value: serde_json::json!({ "n": 1 }),
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete old");
    let old_updated = registry
        .get_process("proc-old")
        .await
        .expect("old record")
        .updated_at_ms;
    let old_events = registry
        .events_after("proc-old", 0)
        .await
        .expect("old events")
        .len();

    // A short real delay so the two terminal timestamps are distinct.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    registry
        .append_event(
            "proc-recent",
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({ "wake_input": "recent" }),
            )
            .with_wake_target_scope(scope.clone()),
        )
        .await
        .expect("append recent wake");
    registry
        .complete_process(
            "proc-recent",
            ProcessAwaitOutput::Success {
                value: serde_json::json!({ "n": 2 }),
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete recent");
    let recent_updated = registry
        .get_process("proc-recent")
        .await
        .expect("recent record")
        .updated_at_ms;
    assert!(
        old_updated < recent_updated,
        "terminal timestamps must be distinct for a meaningful cutoff (old {old_updated}, recent {recent_updated})"
    );

    // Before pruning, proc-old retains its two settled delivery rows.
    assert_eq!(
        registry
            .list_wake_deliveries(Some(crate::WakeDeliveryState::Enqueued))
            .await
            .expect("old deliveries before prune")
            .into_iter()
            .filter(|delivery| delivery.wake.process_id == "proc-old")
            .count(),
        2,
        "both proc-old wake deliveries are durably settled before prune"
    );

    let report = registry
        .prune_terminal_processes(recent_updated, None, None)
        .await
        .expect("prune");
    assert_eq!(
        report.pruned_processes, 1,
        "only proc-old is terminal and older than the cutoff"
    );
    assert_eq!(
        report.pruned_events, old_events,
        "the report counts proc-old's physically deleted event rows"
    );

    // proc-old reads as if it never existed across every retention surface.
    assert!(
        registry.get_process("proc-old").await.is_none(),
        "a pruned process is unknown"
    );
    assert!(
        registry.events_after("proc-old", 0).await.is_err(),
        "event reads for a pruned process are unknown, not empty"
    );
    assert_eq!(
        registry
            .list_handle_grants(&scope)
            .await
            .expect("grants after prune")
            .into_iter()
            .map(|(grant, _)| grant.process_id)
            .collect::<Vec<_>>(),
        vec!["proc-recent".to_string()],
        "the pruned process's grant is gone; the fresher terminal's grant survives"
    );
    assert_eq!(
        registry
            .list_non_terminal()
            .await
            .expect("non-terminal after prune")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec!["proc-live-retained".to_string()],
        "list_non_terminal is unaffected — only the live process remains"
    );

    // The fresher terminal keeps its wake bookkeeping intact.
    assert_eq!(
        registry
            .list_wake_deliveries(Some(crate::WakeDeliveryState::Pending))
            .await
            .expect("recent wakes survive")
            .into_iter()
            .filter(|delivery| delivery.wake.process_id == "proc-recent")
            .count(),
        1,
        "proc-recent's pending wake survives a prune bounded to older terminals"
    );

    // Prune is idempotent: a second call over the same cutoff removes nothing.
    assert_eq!(
        registry
            .prune_terminal_processes(recent_updated, None, None)
            .await
            .expect("second prune"),
        crate::ProcessPruneReport::default(),
        "a repeated prune over the same cutoff reports zeros"
    );

    // proc-old's lease row was physically deleted: re-registering the id and
    // claiming with a fresh owner acquires (no surviving lease fences it) and
    // restarts the fencing token at the baseline rather than advancing a
    // retained one.
    registry
        .register_process(registration("proc-old"))
        .await
        .expect("re-register pruned id");
    let reclaimed = registry
        .claim_process_lease(
            "proc-old",
            &process_lease_owner("prune-lease-successor"),
            60_000,
        )
        .await
        .expect("claim after prune")
        .acquired()
        .expect("no surviving lease fences the fresh claim");
    assert_eq!(
        reclaimed.fencing_token, pre_prune_fencing,
        "a pruned lease row is physically gone, so the fresh claim restarts the fencing token"
    );
}

pub(super) async fn prune_respects_filter(registry: Arc<dyn ProcessRegistry>) {
    const FAR_FUTURE_CUTOFF: u64 = i64::MAX as u64;

    registry
        .register_process(
            registration("proc-filter-prune")
                .with_process_provenance(
                    ProcessProvenance::session(SessionScope::new("filter-prune-session"))
                        .with_caused_by(Some(CausalRef::TriggerOccurrence {
                            occurrence_id: "occurrence-prune".to_string(),
                            subscription_id: Some("subscription-prune".to_string()),
                            subscription_incarnation: None,
                            subscription_revision: None,
                        })),
                )
                .with_identity(ProcessIdentity::new("prunable-kind")),
        )
        .await
        .expect("register prunable");
    registry
        .register_process(
            registration("proc-filter-keep").with_identity(ProcessIdentity::new("retained-kind")),
        )
        .await
        .expect("register retained");
    for process_id in ["proc-filter-prune", "proc-filter-keep"] {
        registry
            .complete_process(
                process_id,
                ProcessAwaitOutput::Success {
                    value: serde_json::Value::Null,
                    control: None,
                },
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete process");
    }
    let pruned_events = registry
        .events_after("proc-filter-prune", 0)
        .await
        .expect("events")
        .len();

    let report = registry
        .prune_terminal_processes(
            FAR_FUTURE_CUTOFF,
            Some(ProcessListFilter {
                status: ProcessStatusFilter::Any,
                caused_by_subscription_id: Some("subscription-prune".to_string()),
                ..ProcessListFilter::default()
            }),
            None,
        )
        .await
        .expect("filtered prune");
    assert_eq!(report.pruned_processes, 1);
    assert_eq!(report.pruned_events, pruned_events);
    assert!(registry.get_process("proc-filter-prune").await.is_none());
    assert!(
        registry
            .get_process("proc-filter-keep")
            .await
            .expect("filtered-out process survives")
            .is_terminal()
    );
}

pub(super) async fn prune_respects_watermark(registry: Arc<dyn ProcessRegistry>) {
    const FAR_FUTURE_CUTOFF: u64 = i64::MAX as u64;

    registry
        .register_process(registration("proc-watermark-before"))
        .await
        .expect("register before");
    registry
        .register_process(registration("proc-watermark-after"))
        .await
        .expect("register after");
    registry
        .complete_process(
            "proc-watermark-before",
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete before");
    let (_records, cursor_after_before) = registry
        .processes_changed_since(ProcessChangeCursor::initial(), 100)
        .await
        .expect("read change feed");
    let before_events = registry
        .events_after("proc-watermark-before", 0)
        .await
        .expect("before events")
        .len();
    registry
        .complete_process(
            "proc-watermark-after",
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete after");

    let report = registry
        .prune_terminal_processes(FAR_FUTURE_CUTOFF, None, Some(cursor_after_before))
        .await
        .expect("watermark prune");
    assert_eq!(report.pruned_processes, 1);
    assert_eq!(report.pruned_events, before_events);
    assert!(
        registry
            .get_process("proc-watermark-before")
            .await
            .is_none()
    );
    assert!(
        registry
            .get_process("proc-watermark-after")
            .await
            .expect("post-watermark terminal survives")
            .is_terminal()
    );
}

/// ADR 0017: prune "never touches non-terminal rows". Even with a cutoff far in
/// the future — older than every row — a running process survives while a
/// terminal one is deleted, proving the terminal-status guard, not just the
/// timestamp bound, gates deletion.
pub(super) async fn prune_never_touches_non_terminal_rows(registry: Arc<dyn ProcessRegistry>) {
    // `i64::MAX` keeps the cutoff positive after the stores' `as i64` cast, so it
    // exceeds every real `updated_at_ms` without wrapping negative.
    const FAR_FUTURE_CUTOFF: u64 = i64::MAX as u64;

    registry
        .register_process(registration("proc-live-old"))
        .await
        .expect("register live");
    registry
        .register_process(registration("proc-term-old"))
        .await
        .expect("register terminal");
    registry
        .complete_process(
            "proc-term-old",
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete terminal");

    let report = registry
        .prune_terminal_processes(FAR_FUTURE_CUTOFF, None, None)
        .await
        .expect("prune with a far-future cutoff");
    assert_eq!(
        report.pruned_processes, 1,
        "only the terminal process is pruned, even though both rows are older than the cutoff"
    );
    assert!(
        registry.get_process("proc-term-old").await.is_none(),
        "the terminal process older than the cutoff is pruned"
    );
    assert!(
        !registry
            .get_process("proc-live-old")
            .await
            .expect("live process survives a far-future cutoff")
            .is_terminal(),
        "a non-terminal row is never pruned, no matter how old relative to the cutoff"
    );
}

/// Host-scheduled retention: `prune_terminal_processes` physically deletes
/// terminal rows (and their events, wake deliveries, grants, leases) older than a
/// cutoff, leaving fresher terminals and every live process untouched.
pub(super) async fn prune_removes_terminal_processes_older_than_cutoff(
    registry: Arc<dyn ProcessRegistry>,
) {
    let scope = SessionScope::new("prune-owner");
    for id in ["proc-prune-old", "proc-prune-fresh", "proc-prune-live"] {
        registry
            .register_process(registration(id))
            .await
            .expect("register prune process");
    }
    registry
        .grant_handle(
            &scope,
            "proc-prune-old",
            ProcessHandleDescriptor::new(Some("test"), Some("old")),
        )
        .await
        .expect("grant old");

    // Complete the old process, then — after a short real delay so the terminal
    // timestamps are distinct even on the in-memory backend — the fresh one.
    registry
        .complete_process(
            "proc-prune-old",
            ProcessAwaitOutput::Success {
                value: serde_json::json!({ "n": 1 }),
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete old");
    let old_updated = registry
        .get_process("proc-prune-old")
        .await
        .expect("old record")
        .updated_at_ms;
    let old_events = registry
        .events_after("proc-prune-old", 0)
        .await
        .expect("old events")
        .len();

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    registry
        .complete_process(
            "proc-prune-fresh",
            ProcessAwaitOutput::Success {
                value: serde_json::json!({ "n": 2 }),
                control: None,
            },
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete fresh");
    let fresh_updated = registry
        .get_process("proc-prune-fresh")
        .await
        .expect("fresh record")
        .updated_at_ms;
    assert!(
        old_updated < fresh_updated,
        "terminal timestamps must be distinct for a meaningful cutoff (old {old_updated}, fresh {fresh_updated})"
    );

    // Cutoff between the two terminals: prunes the old terminal, keeps the fresh
    // terminal and the live process (`updated_at_ms < cutoff` is exclusive).
    let report = registry
        .prune_terminal_processes(fresh_updated, None, None)
        .await
        .expect("prune terminal processes");
    assert_eq!(
        report.pruned_processes, 1,
        "only the terminal process older than the cutoff is pruned"
    );
    assert_eq!(
        report.pruned_events, old_events,
        "the report's event count matches the pruned process's log"
    );

    // The old terminal row, its events, and its grant are gone.
    assert!(
        registry.get_process("proc-prune-old").await.is_none(),
        "a pruned process must read as unknown"
    );
    assert!(
        registry.events_after("proc-prune-old", 0).await.is_err(),
        "events for a pruned process must read as unknown"
    );
    assert!(
        !registry
            .has_handle_grant(&scope, "proc-prune-old")
            .await
            .expect("grant check for pruned process"),
        "the pruned process's handle grant must be deleted"
    );
    assert!(
        registry
            .handle_grants_for_process("proc-prune-old")
            .await
            .is_err(),
        "grants for a pruned process must read as unknown"
    );

    // The fresh terminal and the live process are intact.
    assert!(
        registry
            .get_process("proc-prune-fresh")
            .await
            .expect("fresh terminal survives")
            .is_terminal(),
        "the fresh terminal process must be preserved"
    );
    assert_eq!(
        registry
            .events_after("proc-prune-fresh", 0)
            .await
            .expect("fresh events")
            .len(),
        1,
        "the fresh terminal keeps its event log"
    );
    assert!(
        !registry
            .get_process("proc-prune-live")
            .await
            .expect("live process survives")
            .is_terminal(),
        "the live process must be untouched"
    );
}
