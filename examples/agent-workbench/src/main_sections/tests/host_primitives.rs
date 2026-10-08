//! The host primitives the workbench demonstrates, each across the crash or
//! restart it exists for: a parked approval outlives its body bound and a
//! restart, a trigger delivery that crashed between start and bind starts one
//! process and binds at boot, and a retried process-end notice is one input.

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
/// call once: the turn commits its one answer, and a repeated decision
/// changes nothing.
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
    let approvals = approvals::WorkbenchApprovals::in_memory().expect("open the approval ledger");
    let first = Workbench::builder(cells_then_noted(vec![
        "<typescript>\nconst applied = await ops.apply_change({ target: \"db\", change: \"migrate\" });\nfinish(applied.status);\n</typescript>".to_string(),
    ]))
    .stores(Arc::clone(&stores))
    .approvals(approvals.clone())
    .build()
    .await;
    let session_id = first.state.current_session_id();
    started_turn_id(
        &send_text(&first.state, None, "apply the migration")
            .await
            .expect("the send is admitted"),
    );
    let pending = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let pending = pending_approvals(&first.state)
                .await
                .expect("list the parked approvals");
            if !pending.is_empty() {
                return pending;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the approval call parks");
    let [approval] = pending.as_slice() else {
        panic!("one call parks: {pending:?}");
    };
    assert_eq!(approval.requesting_session, session_id.as_str());

    // The body's 30 s bound passes while the call is parked.
    clock.set(START_MS + 60_000);
    first.shutdown().await;
    let second = Workbench::builder(cells_then_noted(Vec::new()))
        .stores(stores)
        .approvals(approvals)
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
    let Json(again) = approve_wait(State(state.clone()), AxumPath(approval.key.clone()))
        .await
        .expect("a repeated approval is idempotent");
    assert_eq!(again["outcome"], json!("AlreadyResolved"));
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
    state
        .core
        .processes()
        .list(&lash::process::ProcessListFilter {
            status: lash::process::ProcessStatusFilter::Any,
            originator: Some(lash::process::ProcessOriginatorFilter::Host {
                scope: Some(format!("workbench-trigger:{session_id}")),
            }),
            ..lash::process::ProcessListFilter::default()
        })
        .await
        .expect("list the delivered processes")
        .into_iter()
        .map(|process| process.process_id)
        .collect()
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
            |source, _| matches!(source, host_triggers::TriggerSource::Mail),
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
