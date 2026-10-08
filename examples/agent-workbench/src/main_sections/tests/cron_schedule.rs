//! `cron.Schedule` ticked by the workbench (FIG-5394).
//!
//! Lash dispatches no trigger occurrence for its host: the workbench's own
//! cron timer reads the enabled registrations and emits each tick through
//! the host trigger emit, under a key naming the session, the source and the
//! tick. Every law runs the binary's own workbench over a SQLite memory store
//! whose wall clock stands still until the law moves it, so a tick happens
//! exactly when the law crosses its boundary.

use super::*;
use lash::SessionId;

/// Every two seconds, on the even seconds.
const EVERY_TWO_SECONDS: &str = "*/2 * * * * *";
/// The wall clock's start: half a second past an even second.
const START_MS: u64 = 2_000_000_000_500;
const TICK_MS: u64 = 2_000;
const SUBSCRIPTION_KEY: &str = "every-two-seconds";

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

/// The turn that registers the schedule, in time zone `tz` or with none:
/// each tick starts a process that emits its instant, which wakes the
/// session for one turn.
fn registration_cell(tz: Option<&str>) -> String {
    let tz = tz.map_or_else(String::new, |tz| format!(", tz: \"{tz}\""));
    format!(
        r#"<typescript>
const remember_tick = async (tick: unknown) => {{
  await processes.emit({{ value: {{ kind: "cron_tick", fired_at: tick.fired_at }} }});
  return {{ fired_at: tick.fired_at }};
}};
const handle = await triggers.register({{
  subscription_key: "{SUBSCRIPTION_KEY}",
  source: cron.Schedule({{ expr: "{EVERY_TWO_SECONDS}"{tz} }}),
  target: {{ definition: remember_tick }},
  inputs: (tick) => ({{ tick: tick }}),
  name: "cron smoke"
}});
finish("cron registered");
</typescript>"#
    )
}

/// A provider whose first call registers the schedule in `tz` and whose
/// every later call (a tick's wake turn) finishes at once.
fn cron_provider(tz: Option<&'static str>) -> ProviderHandle {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    lash::testing::TestProvider::builder()
        .kind("workbench-cron")
        .complete(move |_| {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                Ok(text_response(&if call == 0 {
                    registration_cell(tz)
                } else {
                    finish_cell("tick seen")
                }))
            }
        })
        .build()
        .into_handle()
}

/// A workbench on a frozen wall clock whose current session registered the
/// schedule with one turn.
struct CronFixture {
    workbench: Workbench,
    /// A second workbench over the same store set, when the law runs two
    /// hosts: each runs its own cron timer.
    second: Option<Workbench>,
    provider: ProviderHandle,
    clock: Arc<FrozenClock>,
    session: SessionId,
    /// The tick the clock was last moved to.
    tick_ms: u64,
}

impl CronFixture {
    async fn registered(provider: ProviderHandle) -> Self {
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
        let workbench = Workbench::builder(provider.clone())
            .stores(stores)
            .build()
            .await;
        let session = workbench.state.current_session_id();
        let output = workbench
            .state
            .create_or_open_session(&session, "test")
            .await
            .expect("open the session")
            .send(lash::TurnInput::text("tick every two seconds"))
            .output()
            .await
            .expect("the registering turn");
        assert_eq!(output.final_value(), Some(&json!("cron registered")));
        Self {
            workbench,
            second: None,
            provider,
            clock,
            session,
            tick_ms: START_MS - START_MS % TICK_MS,
        }
    }

    fn state(&self) -> &AppState {
        &self.workbench.state
    }

    /// Start a second workbench over the same store set: from now on two
    /// cron timers emit every tick.
    async fn with_second_host(mut self) -> Self {
        self.second = Some(
            Workbench::builder(self.provider.clone())
                .stores(Arc::clone(&self.workbench.stores))
                .build()
                .await,
        );
        self
    }

    async fn shutdown(self) {
        if let Some(second) = self.second {
            second.shutdown().await;
        }
        self.workbench.shutdown().await;
    }

    fn query(&self) -> Query<SessionQuery> {
        Query(SessionQuery {
            session_id: Some(self.session.clone()),
        })
    }

    /// The session's one schedule registration.
    async fn subscription(&self) -> lash::triggers::TriggerSubscriptionRecord {
        let records = self
            .state()
            .trigger_store
            .list_subscriptions(lash::triggers::TriggerSubscriptionFilter::for_session(
                &self.session,
            ))
            .await
            .expect("list the session's subscriptions");
        let [record] = records.as_slice() else {
            panic!("the session holds one registration: {records:#?}");
        };
        assert_eq!(record.source_type, CRON_SCHEDULE_SOURCE_TYPE);
        record.clone()
    }

    /// Every tick's occurrence, oldest first, as the instant it names.
    async fn fired(&self) -> Vec<String> {
        let mut occurrences = self
            .state()
            .trigger_store
            .list_occurrences(lash::triggers::TriggerOccurrenceFilter {
                source_type: Some(CRON_SCHEDULE_SOURCE_TYPE.to_owned()),
                ..Default::default()
            })
            .await
            .expect("list the schedule's occurrences")
            .into_iter()
            .map(|occurrence| {
                occurrence.payload["fired_at"]
                    .as_str()
                    .expect("a tick names its instant")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        occurrences.sort();
        occurrences
    }

    /// Every delivery of the registration, with the process it started.
    async fn deliveries(&self, subscription_id: &str) -> Vec<lash::ProcessId> {
        self.state()
            .trigger_store
            .list_deliveries_by_subscription_id(subscription_id)
            .await
            .expect("list the registration's deliveries")
            .into_iter()
            .map(|delivery| match delivery.outcome {
                lash::triggers::TriggerDeliveryEmitOutcome::Started { process_id } => process_id,
                outcome => panic!("a tick's delivery starts its process: {outcome:?}"),
            })
            .collect()
    }

    /// The session's committed turns.
    async fn turns(&self) -> usize {
        self.state()
            .core
            .session(self.session.clone())
            .durable()
            .await
            .expect("the session's durable handle")
            .read()
            .await
            .expect("read the session")
            .map_or(0, |view| view.turn_index())
    }

    /// Move the wall clock onto the schedule's next tick.
    fn cross_boundary(&mut self) -> String {
        self.tick_ms += TICK_MS;
        self.clock.set(self.tick_ms);
        chrono::DateTime::from_timestamp_millis(
            i64::try_from(self.tick_ms).expect("an epoch in range"),
        )
        .expect("a representable instant")
        .to_rfc3339()
    }

    /// Cross the next boundary and wait for its tick: one occurrence naming
    /// it, one delivery whose process ends, and one more committed turn,
    /// which the process's wake ran.
    async fn tick(&mut self, subscription_id: &str) {
        let fired_before = self.fired().await;
        let delivered_before = self.deliveries(subscription_id).await.len();
        let turns_before = self.turns().await;
        let instant = self.cross_boundary();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while self.deliveries(subscription_id).await.len() == delivered_before {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the tick at {instant} never fired"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut fired = fired_before;
        fired.push(instant);
        assert_eq!(self.fired().await, fired, "one occurrence per tick");
        let deliveries = self.deliveries(subscription_id).await;
        assert_eq!(
            deliveries.len(),
            delivered_before + 1,
            "one delivery per tick"
        );
        let started = deliveries.last().expect("the tick's delivery");
        tokio::time::timeout(
            Duration::from_secs(30),
            self.state().core.processes().await_output(started),
        )
        .await
        .expect("the tick's process ends in time")
        .expect("the tick's process ends");
        while self.turns().await < turns_before + 1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the tick's wake turn never committed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The wake runs one turn, never a second.
        self.settle().await;
        assert_eq!(self.turns().await, turns_before + 1, "one turn per tick");
    }

    /// Cross the next boundary and see nothing fire there.
    async fn silent_boundary(&mut self, subscription_id: Option<&str>) {
        let fired = self.fired().await;
        let turns = self.turns().await;
        let delivered = match subscription_id {
            Some(subscription_id) => self.deliveries(subscription_id).await.len(),
            None => 0,
        };
        self.cross_boundary();
        self.settle().await;
        assert_eq!(self.fired().await, fired, "nothing fires at the boundary");
        assert_eq!(self.turns().await, turns, "no turn runs at the boundary");
        if let Some(subscription_id) = subscription_id {
            assert_eq!(self.deliveries(subscription_id).await.len(), delivered);
        }
    }

    /// Long enough for every claim the moved clock causes (claim polls are
    /// a quarter second) and for any turn it would wake.
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
    }

    async fn set_enabled(&self, enabled: bool) {
        let Json(response) = set_trigger_enabled(
            AxumPath(SUBSCRIPTION_KEY.to_owned()),
            State(self.state().clone()),
            self.query(),
            Json(TriggerEnabledRequest { enabled }),
        )
        .await
        .expect("the trigger route changes the registration");
        assert!(response.changed);
    }
}

/// CRON1: a `cron.Schedule` registered from a turn, ticked by two workbench
/// hosts over one store, fires one occurrence, one wake delivery and one
/// completed turn per tick; a disabled schedule stays silent at its next
/// boundary, re-enabling resumes ticks without a second subscription, and a
/// deleted one stays silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registered_cron_schedule_fires_one_turn_per_tick_and_obeys_disable_enable_and_delete() {
    let mut fixture = CronFixture::registered(cron_provider(Some("UTC")))
        .await
        .with_second_host()
        .await;
    let registered = fixture.subscription().await;
    assert_eq!(registered.subscription_key, SUBSCRIPTION_KEY);
    assert_eq!(fixture.turns().await, 1);
    assert!(
        fixture.fired().await.is_empty(),
        "no tick before the first boundary"
    );

    fixture.tick(&registered.subscription_id).await;
    fixture.tick(&registered.subscription_id).await;

    fixture.set_enabled(false).await;
    fixture
        .silent_boundary(Some(&registered.subscription_id))
        .await;
    fixture
        .silent_boundary(Some(&registered.subscription_id))
        .await;

    fixture.set_enabled(true).await;
    let resumed = fixture.subscription().await;
    assert_eq!(
        resumed.subscription_id, registered.subscription_id,
        "re-enabling keeps the one subscription"
    );
    fixture.tick(&registered.subscription_id).await;

    let Json(deleted) = delete_trigger(
        AxumPath(SUBSCRIPTION_KEY.to_owned()),
        State(fixture.state().clone()),
        fixture.query(),
    )
    .await
    .expect("the trigger route deletes the registration");
    assert!(deleted.changed);
    fixture
        .silent_boundary(Some(&registered.subscription_id))
        .await;
    fixture
        .silent_boundary(Some(&registered.subscription_id))
        .await;
    assert_eq!(fixture.fired().await.len(), 3, "three ticks fired in all");
    fixture.shutdown().await;
}

/// A schedule disabled while its tick's turn still runs never fires again:
/// the turn finishes, and the boundaries after it stay silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cron_schedule_disabled_while_its_tick_turn_runs_never_fires_again() {
    let mut gated = GatedProvider::replying(|call| {
        if call == 0 {
            registration_cell(Some("UTC"))
        } else {
            finish_cell("tick seen")
        }
    });
    gated.release(1);
    let mut fixture = CronFixture::registered(gated.provider.clone()).await;
    assert_eq!(gated.next_call().await, 0);
    let registered = fixture.subscription().await;

    let instant = fixture.cross_boundary();
    assert_eq!(
        gated.next_call().await,
        1,
        "the tick's wake turn calls the model"
    );
    assert_eq!(fixture.fired().await, vec![instant]);
    fixture.set_enabled(false).await;
    gated.release(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while fixture.turns().await < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the tick's turn never committed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    fixture
        .silent_boundary(Some(&registered.subscription_id))
        .await;
    fixture
        .silent_boundary(Some(&registered.subscription_id))
        .await;
    assert_eq!(
        fixture.deliveries(&registered.subscription_id).await.len(),
        1
    );
    fixture.workbench.shutdown().await;
}

/// A deleted session's schedule never fires: its close deletes the session's
/// registrations, so the timer reads none. Its schedule names no time zone,
/// and its tick still starts its delivery: the occurrence's source omits `tz`
/// rather than sending `null`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_sessions_cron_schedule_never_fires() {
    let mut fixture = CronFixture::registered(cron_provider(None)).await;
    let registered = fixture.subscription().await;
    fixture.tick(&registered.subscription_id).await;

    tombstone_session(fixture.state(), &fixture.session.clone()).await;
    let fired = fixture.fired().await;
    fixture.silent_boundary(None).await;
    fixture.silent_boundary(None).await;
    assert_eq!(
        fixture.fired().await,
        fired,
        "a deleted session fires nothing"
    );
    fixture.workbench.shutdown().await;
}

/// A workbench that boots after ticks passed with no workbench running fires
/// the latest of them once, and the boundaries after it as they come.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_booting_workbench_fires_the_latest_missed_tick_once() {
    let mut fixture = CronFixture::registered(cron_provider(Some("UTC"))).await;
    let registered = fixture.subscription().await;
    fixture.workbench.state.cron.stop();
    fixture.settle().await;
    // Two ticks pass while no timer runs.
    fixture.cross_boundary();
    let latest = fixture.cross_boundary();
    fixture.settle().await;
    assert!(fixture.fired().await.is_empty(), "no timer, no tick");

    // A booting workbench catches up the latest tick, once.
    fixture = fixture.with_second_host().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while fixture.fired().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the booting workbench never caught up"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    while fixture.turns().await < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the caught-up tick's wake turn never committed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    fixture.settle().await;
    assert_eq!(fixture.fired().await, vec![latest], "one tick, the latest");
    assert_eq!(fixture.turns().await, 2, "one turn for the caught-up tick");
    fixture.tick(&registered.subscription_id).await;
    fixture.shutdown().await;
}
