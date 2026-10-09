//! The host primitives the workbench demonstrates, each across the crash or
//! restart it exists for: a parked approval outlives its body bound and a
//! restart and is resolved by the key `parked` lists, a decision that crashed
//! before its resolve is resolved at boot, a registration or removal that
//! crashed half done is finished at boot, an occurrence selects its
//! recipients in its own transaction, a trigger delivery that crashed between
//! start and bind starts one process and binds at boot, a delivery that
//! cannot start is given up, settled occurrences are pruned, and a retried
//! process-end notice is one input.

use super::*;
use lash::SessionId;

/// The wall clock's start.
const START_MS: u64 = 2_000_000_000_500;

/// A wall clock that stands still until the law sets it; monotonic reads
/// and sleeps are the system's.
#[derive(Debug)]
struct FrozenClock {
    epoch_ms: std::sync::atomic::AtomicU64,
}

impl FrozenClock {
    fn set(&self, epoch_ms: u64) {
        self.epoch_ms
            .store(epoch_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl lash::runtime::Clock for FrozenClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let ms = self.epoch_ms.load(std::sync::atomic::Ordering::SeqCst);
        chrono::DateTime::from_timestamp_millis(i64::try_from(ms).expect("an epoch in range"))
            .expect("a representable instant")
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

/// The turn that registers a process on the mail source.
const MAIL_REGISTRATION: &str = r#"<typescript>
const on_mail = await processes.create({ dialect: "typescript", source: `
const on_mail = async (event: unknown) => {
  return true;
};
` });
await workbench.register_trigger({
  source: { kind: "mail" },
  definition: on_mail,
  event_arg: "event",
  name: "law watcher"
});
finish("registered");
</typescript>"#;

/// A provider whose call `n` answers `cells[n]`, and every later call
/// `finish("noted")`.
fn cells_then_noted(cells: Vec<String>) -> ProviderHandle {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete(move |_| {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let cell = cells
                .get(call)
                .cloned()
                .unwrap_or_else(|| finish_cell("noted"));
            async move { Ok(text_response(&cell)) }
        })
        .build()
        .into_handle()
}

/// `session_id`'s committed `(role, text)` messages.
async fn committed_messages(state: &AppState, session_id: &SessionId) -> Vec<(String, String)> {
    state
        .core
        .session(session_id.clone())
        .durable()
        .await
        .expect("bind the durable session")
        .read()
        .await
        .expect("read the committed session")
        .map(|committed| {
            committed
                .messages()
                .iter()
                .map(|message| (message_role(message).to_string(), message_text(message)))
                .collect()
        })
        .unwrap_or_default()
}

/// Wait until `session_id` committed a message `matches` accepts.
async fn await_committed(
    state: &AppState,
    session_id: &SessionId,
    what: &str,
    matches: impl Fn(&(String, String)) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(60), async {
        while !committed_messages(state, session_id)
            .await
            .iter()
            .any(&matches)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} was never committed"));
}

/// An approval whose wait is resolved after its body's execution bound has
/// passed, by a workbench that restarted while it was parked, completes the
/// call once: the turn commits its one answer, the ledger row is deleted
/// once the resolve answered, and a repeated decision finds nothing pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_resolved_after_its_body_bound_across_a_restart_completes_once() {
    let clock = Arc::new(FrozenClock {
        epoch_ms: std::sync::atomic::AtomicU64::new(START_MS),
    });
    let stores: Arc<dyn lash::StoreSet> = Arc::new(
        lash::sqlite::SqliteStoreSet::memory_with_clock(
            Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>
        )
        .await
        .expect("open a SQLite memory store set on the frozen clock"),
    );
    let commits = RecordedCommits::over(Arc::clone(&stores));
    let approvals = approvals::WorkbenchApprovals::in_memory().expect("open the approval ledger");
    let first = Workbench::builder(cells_then_noted(vec![
        "<typescript>\nconst applied = await ops.apply_change({ target: \"db\", change: \"migrate\" });\nfinish(applied.status);\n</typescript>".to_string(),
    ]))
    .stores(Arc::clone(&commits.stores))
    .approvals(approvals.clone())
    .build()
    .await;
    let session_id = first.state.current_session_id();
    started_turn_id(
        &send_text(&first.state, None, "apply the migration")
            .await
            .expect("the send is admitted"),
    );
    let approval = the_parked_approval(&first.state, &commits).await;
    assert_eq!(approval.requesting_session, session_id.as_str());

    // The body's 30 s bound passes while the call is parked.
    clock.set(START_MS + 60_000);
    first.shutdown().await;
    let second = Workbench::builder(cells_then_noted(Vec::new()))
        .stores(stores)
        .approvals(approvals.clone())
        .build()
        .await;
    let state = &second.state;
    let Json(decided) = approve_wait(State(state.clone()), AxumPath(approval.key.clone()))
        .await
        .expect("approve the parked call");
    assert_eq!(decided["outcome"], json!("Resolved"));
    await_committed(state, &session_id, "the approved answer", |(role, text)| {
        role == "assistant" && text == "applied"
    })
    .await;
    assert!(
        approvals
            .requests()
            .expect("read the approval ledger")
            .is_empty(),
        "the decided row is deleted once the resolve answered"
    );
    approve_wait(State(state.clone()), AxumPath(approval.key.clone()))
        .await
        .expect_err("a repeated approval finds nothing pending");
    let answers = committed_messages(state, &session_id)
        .await
        .into_iter()
        .filter(|(role, text)| role == "assistant" && text == "applied")
        .count();
    assert_eq!(answers, 1, "the call completes once");
    assert!(
        pending_approvals(state)
            .await
            .expect("list the parked approvals")
            .is_empty(),
        "a resolved call is no longer parked"
    );
    second.shutdown().await;
}

/// The processes deliveries started for `session_id`'s registrations.
async fn delivered_processes(state: &AppState, session_id: &SessionId) -> Vec<lash::ProcessId> {
    let filter = lash::persistence::ProcessListFilter {
        status: lash::process::ProcessStatusFilter::Any,
        originator: Some(lash::process::ProcessOriginatorFilter::Host {
            scope: Some(format!("workbench-trigger:{session_id}")),
        }),
        ..lash::persistence::ProcessListFilter::default()
    };
    let mut continuation = None;
    let mut processes = Vec::new();
    loop {
        let page = state
            .core
            .processes()
            .list(&filter, std::num::NonZeroUsize::MIN, continuation)
            .await
            .expect("list the delivered processes");
        processes.extend(page.processes.into_iter().map(|process| process.process_id));
        continuation = page.continuation;
        if continuation.is_none() {
            return processes;
        }
    }
}

/// A delivery whose process started but whose bind was lost to a crash
/// starts exactly one process: the next workbench's boot pass starts it again
/// under the same host start key, which answers the first process, and binds
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delivery_that_crashed_between_start_and_bind_starts_one_process_and_binds_at_boot() {
    let triggers = host_triggers::HostTriggers::in_memory().expect("open the trigger tables");
    let first = Workbench::builder(cells_then_noted(vec![MAIL_REGISTRATION.to_string()]))
        .host_triggers(triggers.clone())
        .build()
        .await;
    let session_id = first.state.current_session_id();
    run_turn(&first.state, "watch the mail").await;

    // The occurrence and its delivery are recorded; the process starts; the
    // workbench dies before it binds the process id.
    triggers
        .record(
            "mail:crash-law",
            &json!(mail::MailDelivery {
                account: "work".to_string(),
                title: "crash".to_string(),
                text: "between start and bind".to_string(),
            }),
            host_triggers::Recipients::Mail,
        )
        .expect("record the occurrence");
    let [delivery] = triggers
        .unbound()
        .expect("read the unbound deliveries")
        .try_into()
        .expect("the occurrence has one delivery");
    let started = triggers.start(&delivery).await.expect("start the delivery");
    let stores = Arc::clone(&first.stores);
    first.shutdown().await;

    let second = Workbench::builder(cells_then_noted(Vec::new()))
        .stores(stores)
        .host_triggers(triggers.clone())
        .build()
        .await;
    let bound = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(bound) = triggers
                .bound_process(&delivery.occurrence_id, &delivery.subscription.id)
                .expect("read the delivery")
            {
                return bound;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the boot pass binds the delivery");
    assert_eq!(
        bound, started,
        "the boot pass binds the process the crashed start made"
    );
    assert_eq!(
        delivered_processes(&second.state, &session_id).await,
        vec![started],
        "the occurrence started exactly one process"
    );
    second.shutdown().await;
}

/// The notice that a delivered process ended reaches its session once, however
/// often it is sent: a notice pass whose cursor commit was lost reads the same
/// page again and resends, and the resend is the input the first send made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_process_end_notice_retried_after_a_lost_acknowledgement_reaches_the_session_once() {
    let workbench = Workbench::builder(cells_then_noted(vec![MAIL_REGISTRATION.to_string()]))
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    run_turn(state, "watch the mail").await;
    state
        .host_triggers
        .fire_mail(
            "mail:notice-law",
            &mail::MailDelivery {
                account: "work".to_string(),
                title: "notice".to_string(),
                text: "end me".to_string(),
            },
        )
        .expect("fire the mail source");
    let process_id = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let [process_id] = delivered_processes(state, &session_id).await.as_slice() {
                return process_id.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the delivery starts its process");
    tokio::time::timeout(
        Duration::from_secs(30),
        state.core.processes().await_output(&process_id),
    )
    .await
    .expect("the delivered process ends in time")
    .expect("the delivered process ends");
    let notice = format!("Triggered process {process_id} ended");
    await_committed(state, &session_id, "the notice's answer", |(role, text)| {
        role == "assistant" && text == "noted"
    })
    .await;

    // Two passes from a cursor that was never committed: each resends every
    // notice of the page.
    for _ in 0..2 {
        host_triggers::notify_ended_since(state, lash::process::ProcessChangeCursor::initial())
            .await
            .expect("resend the page's notices");
    }
    let messages = committed_messages(state, &session_id).await;
    let notices = messages
        .iter()
        .filter(|(role, text)| role == "user" && text.starts_with(&notice))
        .count();
    assert_eq!(
        notices, 1,
        "the notice reached the session once: {messages:?}"
    );
    let snapshot = read_state(state, Some(&session_id))
        .await
        .expect("read the session");
    assert!(
        snapshot.state.pending_turn_inputs.is_empty(),
        "a resent notice queues no second input: {:?}",
        snapshot.state.pending_turn_inputs
    );
    workbench.shutdown().await;
}

/// Tombstone `session_id` through lash alone: what a workbench that died
/// right after the tombstone leaves behind, its registrations still in the
/// trigger tables.
async fn tombstone(state: &AppState, session_id: &SessionId) {
    let administration = state.core.session_administration().await;
    let context = administration
        .delete_context(session_id)
        .expect("a delete context for the session");
    lash::LashCore::delete_session(context)
        .await
        .expect("request the session's close");
    state
        .core
        .await_session_deletion(session_id)
        .await
        .expect("the session is tombstoned");
}

/// A session tombstoned by a workbench that died before it removed the
/// session's registrations loses them at the next boot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_tombstoned_before_its_registrations_were_removed_loses_them_at_boot() {
    let triggers = host_triggers::HostTriggers::in_memory().expect("open the trigger tables");
    let first = Workbench::builder(cells_then_noted(vec![MAIL_REGISTRATION.to_string()]))
        .host_triggers(triggers.clone())
        .build()
        .await;
    let session_id = first.state.current_session_id();
    run_turn(&first.state, "watch the mail").await;
    assert_eq!(
        triggers
            .subscriptions(Some(&session_id))
            .expect("read the registrations")
            .len(),
        1
    );
    tombstone(&first.state, &session_id).await;
    let stores = Arc::clone(&first.stores);
    first.shutdown().await;

    let second = Workbench::builder(cells_then_noted(Vec::new()))
        .stores(stores)
        .host_triggers(triggers.clone())
        .build()
        .await;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !triggers
            .subscriptions(None)
            .expect("read the registrations")
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the boot sweep removes a tombstoned session's registrations");
    second.shutdown().await;
}

/// A delivery whose start cannot succeed, here because its subscriber is
/// tombstoned, is recorded as failed after a bounded number of attempts and
/// the pass stops retrying it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delivery_that_cannot_start_is_recorded_failed_and_not_retried() {
    let workbench = Workbench::builder(cells_then_noted(vec![MAIL_REGISTRATION.to_string()]))
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    run_turn(state, "watch the mail").await;
    tombstone(state, &session_id).await;
    state
        .host_triggers
        .fire_mail(
            "mail:failed-law",
            &mail::MailDelivery {
                account: "work".to_string(),
                title: "nobody".to_string(),
                text: "is home".to_string(),
            },
        )
        .expect("fire the mail source");
    let mut retrying = true;
    for _ in 0..16 {
        retrying = state.host_triggers.deliver_unbound().await;
        if !retrying {
            break;
        }
    }
    assert!(!retrying, "the pass gives a delivery that cannot start up");
    assert!(
        state
            .host_triggers
            .unbound()
            .expect("read the unbound deliveries")
            .is_empty(),
        "a failed delivery is no longer due"
    );
    let [delivery] = state
        .host_triggers
        .deliveries()
        .expect("read the deliveries")
        .try_into()
        .expect("the occurrence has one delivery");
    assert_eq!(delivery.occurrence_id, "mail:failed-law");
    assert_eq!(delivery.attempts, host_triggers::DELIVERY_ATTEMPTS);
    assert!(delivery.failure.is_some(), "the last error is recorded");
    assert!(
        delivered_processes(state, &session_id).await.is_empty(),
        "a tombstoned session's registration starts nothing"
    );
    workbench.shutdown().await;
}

/// The turn that registers two processes on the mail source.
const TWO_MAIL_REGISTRATIONS: &str = r#"<typescript>
const on_mail = await processes.create({ dialect: "typescript", source: `
const on_mail = async (event: unknown) => {
  return true;
};
` });
await workbench.register_trigger({
  source: { kind: "mail" },
  definition: on_mail,
  event_arg: "event",
  name: "first watcher"
});
await workbench.register_trigger({
  source: { kind: "mail" },
  definition: on_mail,
  event_arg: "event",
  name: "second watcher"
});
finish("registered");
</typescript>"#;

/// The turn that registers a process on a cron source that ticks at every
/// even hour.
const CRON_REGISTRATION: &str = r#"<typescript>
const on_tick = await processes.create({ dialect: "typescript", source: `
const on_tick = async (event: unknown) => {
  return true;
};
` });
await workbench.register_trigger({
  source: { kind: "cron", expr: "0 0 */2 * * *" },
  definition: on_tick,
  event_arg: "event",
  name: "law tick"
});
finish("registered");
</typescript>"#;

/// The first even hour after [`START_MS`].
const TICK_MS: u64 = 2_000_001_600_000;

fn law_mail(title: &str) -> mail::MailDelivery {
    mail::MailDelivery {
        account: "work".to_string(),
        title: title.to_string(),
        text: "a law's mail".to_string(),
    }
}

/// A SQLite memory store set on a wall clock the law sets.
async fn frozen_stores() -> (Arc<FrozenClock>, Arc<dyn lash::StoreSet>) {
    let clock = Arc::new(FrozenClock {
        epoch_ms: std::sync::atomic::AtomicU64::new(START_MS),
    });
    let stores: Arc<dyn lash::StoreSet> = Arc::new(
        lash::sqlite::SqliteStoreSet::memory_with_clock(
            Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>
        )
        .await
        .expect("open a SQLite memory store set on the frozen clock"),
    );
    (clock, stores)
}

/// Wait until `ready` answers.
async fn eventually<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(value) = ready() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} never happened"))
}

/// The one registration in `triggers`, which must be active.
fn the_registration(triggers: &host_triggers::HostTriggers) -> String {
    let [(id, state)] = triggers
        .lifecycle()
        .expect("read the registrations")
        .try_into()
        .expect("one registration");
    assert_eq!(state, host_triggers::SubscriptionState::Pinned);
    id
}

/// A registration whose row was recorded but not pinned when the workbench
/// died is not listed and receives no delivery; the next boot takes hold of
/// its definition, and from then on it dispatches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registration_left_pending_dispatches_nothing_and_is_pinned_at_boot() {
    let triggers = host_triggers::HostTriggers::in_memory().expect("open the trigger tables");
    let first = Workbench::builder(cells_then_noted(vec![MAIL_REGISTRATION.to_string()]))
        .host_triggers(triggers.clone())
        .build()
        .await;
    let session_id = first.state.current_session_id();
    run_turn(&first.state, "watch the mail").await;
    let id = the_registration(&triggers);
    triggers
        .leave_in(&id, host_triggers::SubscriptionState::Pending)
        .expect("leave the row pending");
    triggers
        .record(
            "mail:pending-law",
            &json!(law_mail("early")),
            host_triggers::Recipients::Mail,
        )
        .expect("record the occurrence");
    assert!(
        triggers
            .deliveries()
            .expect("read the deliveries")
            .is_empty(),
        "a pending registration receives no delivery"
    );
    assert!(
        triggers
            .subscriptions(None)
            .expect("read the registrations")
            .is_empty(),
        "a pending registration is not listed"
    );
    let stores = Arc::clone(&first.stores);
    first.shutdown().await;

    let second = Workbench::builder(cells_then_noted(Vec::new()))
        .stores(stores)
        .host_triggers(triggers.clone())
        .build()
        .await;
    eventually("the boot recovery pinning the registration", || {
        (triggers.lifecycle().expect("read the registrations")
            == vec![(id.clone(), host_triggers::SubscriptionState::Pinned)])
        .then_some(())
    })
    .await;
    triggers
        .fire_mail("mail:pinned-law", &law_mail("late"))
        .expect("fire the mail source");
    let bound = eventually("the pinned registration's delivery", || {
        triggers
            .bound_process("mail:pinned-law", &id)
            .expect("read the delivery")
    })
    .await;
    assert_eq!(
        delivered_processes(&second.state, &session_id).await,
        vec![bound]
    );
    second.shutdown().await;
}

/// A pending registration whose pin can no longer take hold of its
/// definition never existed for anybody: the boot recovery removes its row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_registration_whose_definition_cannot_be_held_is_removed_at_boot() {
    let triggers = host_triggers::HostTriggers::in_memory().expect("open the trigger tables");
    let first = Workbench::builder(cells_then_noted(vec![MAIL_REGISTRATION.to_string()]))
        .host_triggers(triggers.clone())
        .build()
        .await;
    run_turn(&first.state, "watch the mail").await;
    let id = the_registration(&triggers);
    let (pin, _) = triggers
        .held(&id)
        .expect("read the registration")
        .expect("the registration names a pin");
    triggers
        .leave_in(&id, host_triggers::SubscriptionState::Pending)
        .expect("leave the row pending");
    // An ended pin holds nothing from here on.
    first
        .state
        .core
        .host_artifacts()
        .release(pin)
        .await
        .expect("end the pin");
    let stores = Arc::clone(&first.stores);
    first.shutdown().await;

    let second = Workbench::builder(cells_then_noted(Vec::new()))
        .stores(stores)
        .host_triggers(triggers.clone())
        .build()
        .await;
    eventually("the boot recovery removing the registration", || {
        triggers
            .lifecycle()
            .expect("read the registrations")
            .is_empty()
            .then_some(())
    })
    .await;
    second.shutdown().await;
}

/// A removal that marked its row and died before it released the pin
/// dispatches nothing from the mark on; the next boot releases the pin and
/// deletes the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_that_crashed_before_its_release_dispatches_nothing_and_finishes_at_boot() {
    let triggers = host_triggers::HostTriggers::in_memory().expect("open the trigger tables");
    let first = Workbench::builder(cells_then_noted(vec![MAIL_REGISTRATION.to_string()]))
        .host_triggers(triggers.clone())
        .build()
        .await;
    run_turn(&first.state, "watch the mail").await;
    let id = the_registration(&triggers);
    let (pin, definition) = triggers
        .held(&id)
        .expect("read the registration")
        .expect("the registration names a pin");
    assert_eq!(
        triggers
            .mark_deleting(Some(&id), None)
            .expect("mark the removal"),
        1
    );
    triggers
        .record(
            "mail:deleting-law",
            &json!(law_mail("after the mark")),
            host_triggers::Recipients::Mail,
        )
        .expect("record the occurrence");
    assert!(
        triggers
            .deliveries()
            .expect("read the deliveries")
            .is_empty(),
        "a registration being removed receives no delivery"
    );
    let stores = Arc::clone(&first.stores);
    first.shutdown().await;

    let second = Workbench::builder(cells_then_noted(Vec::new()))
        .stores(stores)
        .host_triggers(triggers.clone())
        .build()
        .await;
    eventually("the boot recovery finishing the removal", || {
        triggers
            .lifecycle()
            .expect("read the registrations")
            .is_empty()
            .then_some(())
    })
    .await;
    second
        .state
        .core
        .host_artifacts()
        .pin_definition(&pin, &definition.id)
        .await
        .expect_err("the removed registration's pin was released");
    second.shutdown().await;
}

/// An occurrence recorded while one of its subscriptions is being removed is
/// still recorded, and delivers to the others: the recipients are selected in
/// the occurrence's own transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_occurrence_recorded_while_a_subscription_is_removed_delivers_to_the_others() {
    let workbench = Workbench::builder(cells_then_noted(vec![TWO_MAIL_REGISTRATIONS.to_string()]))
        .build()
        .await;
    let triggers = &workbench.state.host_triggers;
    run_turn(&workbench.state, "watch the mail twice").await;
    let [(removed, _), (kept, _)] = triggers
        .lifecycle()
        .expect("read the registrations")
        .try_into()
        .expect("two registrations");
    triggers
        .mark_deleting(Some(&removed), None)
        .expect("mark the removal");
    triggers
        .record(
            "mail:atomic-law",
            &json!(law_mail("during a removal")),
            host_triggers::Recipients::Mail,
        )
        .expect("record the occurrence");
    assert_eq!(
        triggers.occurrences().expect("read the occurrences"),
        vec!["mail:atomic-law".to_string()]
    );
    assert_eq!(
        triggers
            .deliveries()
            .expect("read the deliveries")
            .into_iter()
            .map(|delivery| (delivery.occurrence_id, delivery.subscription_id))
            .collect::<Vec<_>>(),
        vec![("mail:atomic-law".to_string(), kept)]
    );
    workbench.shutdown().await;
}

/// A retired session's registrations are removed with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retired_session_leaves_no_registration() {
    let workbench = Workbench::builder(cells_then_noted(vec![MAIL_REGISTRATION.to_string()]))
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    run_turn(state, "watch the mail").await;
    the_registration(&state.host_triggers);
    retire_session(state, &session_id)
        .await
        .expect("retire the session");
    assert!(
        state
            .host_triggers
            .lifecycle()
            .expect("read the registrations")
            .is_empty(),
        "the session's registrations went with it"
    );
    workbench.shutdown().await;
}

/// A settled occurrence and its deliveries are pruned once the occurrence is
/// older than its retention, and not before. The delivered process stays, so
/// a source that fires the pruned occurrence again starts nothing new: its
/// start key answers the process the first delivery started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settled_occurrence_is_pruned_after_its_retention_and_its_process_stays() {
    let (clock, stores) = frozen_stores().await;
    let workbench = Workbench::builder(cells_then_noted(vec![MAIL_REGISTRATION.to_string()]))
        .stores(stores)
        .build()
        .await;
    let state = &workbench.state;
    let triggers = &state.host_triggers;
    let session_id = state.current_session_id();
    run_turn(state, "watch the mail").await;
    let id = the_registration(triggers);
    triggers
        .fire_mail("mail:prune-law", &law_mail("keep me a while"))
        .expect("fire the mail source");
    let bound = eventually("the delivery's bind", || {
        triggers
            .bound_process("mail:prune-law", &id)
            .expect("read the delivery")
    })
    .await;
    await_committed(state, &session_id, "the notice's answer", |(role, text)| {
        role == "assistant" && text == "noted"
    })
    .await;
    assert_eq!(
        triggers.prune_settled().expect("prune"),
        0,
        "a bound delivery's occurrence is kept for its retention"
    );

    let retention_ms = u64::try_from(host_triggers::OCCURRENCE_RETENTION.as_millis())
        .expect("a retention in range");
    clock.set(START_MS + retention_ms);
    assert_eq!(triggers.prune_settled().expect("prune"), 1);
    assert!(
        triggers
            .occurrences()
            .expect("read the occurrences")
            .is_empty()
    );
    assert!(
        triggers
            .deliveries()
            .expect("read the deliveries")
            .is_empty(),
        "an occurrence's deliveries are pruned with it"
    );

    triggers
        .fire_mail("mail:prune-law", &law_mail("keep me a while"))
        .expect("fire the pruned occurrence again");
    let again = eventually("the second delivery's bind", || {
        triggers
            .bound_process("mail:prune-law", &id)
            .expect("read the delivery")
    })
    .await;
    assert_eq!(again, bound, "the start key answers the first process");
    assert_eq!(
        delivered_processes(state, &session_id).await,
        vec![bound],
        "the occurrence started exactly one process"
    );
    workbench.shutdown().await;
}

/// A cron tick is recorded once: the registration is ticked through it in the
/// tick's own transaction, so a workbench that restarts after the tick's
/// occurrence was pruned does not fire the tick again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cron_tick_is_recorded_once_across_a_restart_after_its_occurrence_was_pruned() {
    let (clock, stores) = frozen_stores().await;
    // Cross the tick without expiring a running node's lease: the store
    // uses this wall clock too, while heartbeats run on monotonic time.
    clock.set(TICK_MS - 1);
    let triggers = host_triggers::HostTriggers::in_memory().expect("open the trigger tables");
    let first = Workbench::builder(cells_then_noted(vec![CRON_REGISTRATION.to_string()]))
        .stores(Arc::clone(&stores))
        .host_triggers(triggers.clone())
        .build()
        .await;
    let session_id = first.state.current_session_id();
    run_turn(&first.state, "tick every other hour").await;
    let id = the_registration(&triggers);
    let occurrence = format!("cron:{id}:{TICK_MS}");

    clock.set(TICK_MS);
    let bound = eventually("the tick's delivery", || {
        triggers
            .bound_process(&occurrence, &id)
            .expect("read the delivery")
    })
    .await;
    await_committed(
        &first.state,
        &session_id,
        "the notice's answer",
        |(role, text)| role == "assistant" && text == "noted",
    )
    .await;
    first.shutdown().await;
    let retention_ms = u64::try_from(host_triggers::OCCURRENCE_RETENTION.as_millis())
        .expect("a retention in range");
    // Let retention elapse while no node is running on the frozen clock.
    clock.set(TICK_MS + retention_ms);
    assert_eq!(triggers.prune_settled().expect("prune"), 1);

    let second = Workbench::builder(cells_then_noted(Vec::new()))
        .stores(stores)
        .host_triggers(triggers.clone())
        .build()
        .await;
    // Several passes of the restarted timer.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let [subscription] = triggers
        .subscriptions(None)
        .expect("read the registrations")
        .try_into()
        .expect("one registration");
    assert_eq!(
        subscription.ticked_through_ms,
        Some(i64::try_from(TICK_MS).expect("a tick in range"))
    );
    assert!(
        triggers
            .occurrences()
            .expect("read the occurrences")
            .is_empty(),
        "the restarted timer fires no tick it was ticked through"
    );
    assert_eq!(
        delivered_processes(&second.state, &session_id).await,
        vec![bound]
    );
    second.shutdown().await;
}

/// A store set whose durable commits a law reads back: the binary's commit
/// ledger over a law's stores, appending to a file of its own.
struct RecordedCommits {
    stores: Arc<dyn lash::StoreSet>,
    ledger: tempfile::NamedTempFile,
}

impl RecordedCommits {
    fn over(stores: Arc<dyn lash::StoreSet>) -> Self {
        let file = tempfile::NamedTempFile::new().expect("create the commit ledger");
        let ledger = crate::e2e_commit_ledger::CommitLedger::open(file.path(), "law", Vec::new())
            .expect("open the commit ledger");
        Self {
            stores: crate::e2e_commit_ledger::ledger_stores(stores, ledger),
            ledger: file,
        }
    }

    /// Whether the store applied a commit of `session`'s actor under `label`.
    fn applied(&self, session: &SessionId, label: lash::durable::CommitLabel) -> bool {
        let actor = lash::durable::ActorKey::session(session.as_str())
            .expect("a session actor")
            .to_string();
        std::fs::read_to_string(self.ledger.path())
            .expect("read the commit ledger")
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|commit| {
                commit["label"] == label.as_str()
                    && commit["actor"] == actor.as_str()
                    && commit["applied"] == true
            })
    }
}

/// The approval call the current session of `state` parked, once its park is
/// durable in the stores `commits` records.
///
/// `Completions::parked` lists a call from its admission, which pins the
/// completion wait before the body runs, and the body records the ledger row
/// before it parks. The call is parked only once its `Waiting` outcome
/// commits, under `round.outcome`: a workbench stopped before that leaves a
/// started `Once` call, which the next owner records `Interrupted`
/// (ADR 0132 §5) whatever resolved its wait. Nothing a host reads says the
/// park committed, so the law reads the commit itself; it is the turn's
/// first `round.outcome`, the cell's one call.
async fn the_parked_approval(
    state: &AppState,
    commits: &RecordedCommits,
) -> approvals::PendingApproval {
    let session_id = state.current_session_id();
    let pending = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let pending = pending_approvals(state)
                .await
                .expect("list the parked approvals");
            if !pending.is_empty()
                && commits.applied(&session_id, lash::durable::CommitLabel::ROUND_OUTCOME)
            {
                return pending;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the approval call parks");
    let [approval] = pending
        .try_into()
        .unwrap_or_else(|pending| panic!("one call parks: {pending:?}"));
    approval
}

/// A decision recorded by a workbench that died before it resolved the call
/// is resolved at the next boot, with the key `Completions::parked` lists for
/// the call, and its ledger row is deleted once the resolve answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_decision_that_crashed_before_its_resolve_is_resolved_at_boot_by_the_parked_key() {
    let stores: Arc<dyn lash::StoreSet> = Arc::new(
        lash::sqlite::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    let commits = RecordedCommits::over(Arc::clone(&stores));
    let approvals = approvals::WorkbenchApprovals::in_memory().expect("open the approval ledger");
    let first = Workbench::builder(cells_then_noted(vec![
        "<typescript>\nconst applied = await ops.apply_change({ target: \"db\", change: \"migrate\" });\nfinish(applied.status);\n</typescript>".to_string(),
    ]))
    .stores(Arc::clone(&commits.stores))
    .approvals(approvals.clone())
    .build()
    .await;
    let session_id = first.state.current_session_id();
    started_turn_id(
        &send_text(&first.state, None, "apply the migration")
            .await
            .expect("the send is admitted"),
    );
    let approval = the_parked_approval(&first.state, &commits).await;
    assert_eq!(
        approvals
            .decide(&approval.key, approvals::ApprovalDecision::Approved)
            .expect("record the decision"),
        approvals::ApprovalDecision::Approved
    );
    first.shutdown().await;

    let second = Workbench::builder(cells_then_noted(Vec::new()))
        .stores(stores)
        .approvals(approvals.clone())
        .build()
        .await;
    await_committed(
        &second.state,
        &session_id,
        "the approved answer",
        |(role, text)| role == "assistant" && text == "applied",
    )
    .await;
    assert!(
        approvals
            .requests()
            .expect("read the approval ledger")
            .is_empty(),
        "the decided row is deleted once the resolve answered"
    );
    second.shutdown().await;
}
