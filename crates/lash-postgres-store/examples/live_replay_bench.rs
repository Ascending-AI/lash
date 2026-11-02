//! A small measured benchmark: simulated streaming runs publish one frame
//! every 50 ms on replica A while a subscriber per run follows it on
//! replica B. It reports publish-to-deliver latency and what the database
//! spent. Run the `live_replay_bench` example; it asserts nothing about speed. It is a
//! closed-loop service diagnostic, not an offered-load measurement.

#![expect(
    clippy::expect_used,
    reason = "the diagnostic fixture must provision its database and observations"
)]
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::{LiveReplayEventDraft, SessionRevision};
use lash_kernel_value::identity::SessionId;
use lash_postgres_store::testing::{IsolatedDatabase, required_database_url};
use sqlx::Row as _;

use lash::postgres::{PostgresEndpoints, PostgresHostConfig, PostgresLiveReplayStore};
use lash_core::{
    LiveReplayStore, LiveReplayStoreError, LiveReplaySubscribeOutcome, LiveReplaySubscription,
    SessionObservationEvent, SessionObservationEventPayload, TurnActivity, TurnActivityId,
    TurnEvent,
};
use lash_protocol_types::llm::types::{StreamBlockEvent, StreamBlockKind};
/// Connect a store from synchronous code, as the law factories are.
fn connect(url: &str, config: PostgresHostConfig) -> Arc<dyn LiveReplayStore> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| {
        handle.block_on(async {
            Arc::new(
                PostgresLiveReplayStore::connect(&endpoints(url), &config)
                    .await
                    .expect("connect the PostgreSQL live replay store"),
            ) as Arc<dyn LiveReplayStore>
        })
    })
}

fn fresh_schema() -> String {
    format!("live_replay_{}", uuid::Uuid::new_v4().simple())
}

fn label(event: &SessionObservationEvent) -> String {
    match &event.payload {
        SessionObservationEventPayload::TurnActivity(TurnActivity {
            event:
                TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    text,
                    ..
                }),
            ..
        }) => text.to_string(),
        other => format!("{other:?}"),
    }
}

/// The next item of a live replay subscription.
async fn next_item(
    subscription: &mut LiveReplaySubscription,
) -> Option<Result<Arc<SessionObservationEvent>, LiveReplayStoreError>> {
    use lash::observe::Stream as _;
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *subscription).poll_next(cx)).await
}

fn subscribed(
    outcome: Result<LiveReplaySubscribeOutcome, LiveReplayStoreError>,
) -> LiveReplaySubscription {
    match outcome.expect("subscribe") {
        LiveReplaySubscribeOutcome::Subscribed(subscription) => subscription,
        LiveReplaySubscribeOutcome::Gap(reason) => {
            panic!("expected a subscription, got {reason:?}")
        }
    }
}

fn text(id: &str, text: &str) -> SessionObservationEventPayload {
    SessionObservationEventPayload::TurnActivity(TurnActivity {
        id: TurnActivityId::new(id),
        correlation_id: TurnActivityId::new("text:0"),
        event: TurnEvent::StreamBlock(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            text: text.into(),
            block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
        }),
    })
}

fn endpoints(url: &str) -> PostgresEndpoints {
    PostgresEndpoints::from_url(url).expect("the database URL parses")
}

/// Streaming runs (`LIVE_REPLAY_BENCH_RUNS`, default 100), the frame
/// interval each publishes at, and how long they stream.
#[expect(
    clippy::disallowed_methods,
    reason = "benchmark fixture: the run count is a measurement parameter"
)]
fn runs() -> usize {
    std::env::var("LIVE_REPLAY_BENCH_RUNS")
        .ok()
        .and_then(|runs| runs.parse().ok())
        .unwrap_or(100)
}
const FRAME: Duration = Duration::from_millis(50);
const STREAM_FOR: Duration = Duration::from_secs(10);
/// Bytes of text per frame: a 50 ms frame of a fast model's tokens.
const FRAME_TEXT: usize = 120;

/// CPU seconds the database server's processes have used: the postmaster
/// and every child of it, read from `/proc` when the server shares this
/// host, as the hermetic test server does.
#[expect(
    clippy::disallowed_methods,
    reason = "benchmark fixture: it reads the database server's /proc accounting"
)]
fn server_cpu_seconds(postmaster: u32) -> Option<f64> {
    // `USER_HZ` is 100 on every Linux the pool runs.
    const TICKS: f64 = 100.0;
    let stat = |pid: &str| -> Option<(u32, u64)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let fields = stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        let parent = fields.get(1)?.parse().ok()?;
        let user: u64 = fields.get(11)?.parse().ok()?;
        let system: u64 = fields.get(12)?.parse().ok()?;
        Some((parent, user + system))
    };
    let (_, own) = stat(&postmaster.to_string())?;
    let children = std::fs::read_dir("/proc")
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| stat(entry.file_name().to_str()?))
        .filter(|(parent, _)| *parent == postmaster)
        .map(|(_, ticks)| ticks)
        .sum::<u64>();
    Some((own + children) as f64 / TICKS)
}

#[expect(
    clippy::disallowed_methods,
    reason = "benchmark fixture: it reads the database server's /proc accounting"
)]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    let database = IsolatedDatabase::create(&required_database_url()).await;
    // Both replicas run the production defaults.
    let production = lash::postgres::PostgresHostConfig {
        live_replay: Some(lash::postgres::LiveReplayPolicy {
            data: lash::postgres::ReplayDataPolicy {
                schema: fresh_schema(),
                schema_mode: lash::postgres::ReplaySchemaMode::Install,
                ..lash::postgres::ReplayDataPolicy::default()
            },
            ..lash::postgres::LiveReplayPolicy::default()
        }),
        ..lash::postgres::PostgresHostConfig::default()
    };
    let a = connect(database.url(), production.clone());
    let b = connect(database.url(), production.clone());

    let mut probe = <sqlx::PgConnection as sqlx::Connection>::connect(database.url())
        .await
        .expect("connect the probe");
    let backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut probe)
        .await
        .expect("read the probe's backend");
    let postmaster = parent_of(u32::try_from(backend).expect("a pid"));
    let statements_available = sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .execute(&mut probe)
        .await
        .is_ok()
        && sqlx::query("SELECT pg_stat_statements_reset()")
            .execute(&mut probe)
            .await
            .is_ok();
    let commits = |row: sqlx::postgres::PgRow| row.get::<i64, _>("xact_commit");
    let commits_sql = "SELECT xact_commit FROM pg_stat_database WHERE datname = current_database()";

    let sent: Arc<Mutex<HashMap<String, Instant>>> = Arc::default();
    let latencies: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let publish_latencies: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let frames_per_run = (STREAM_FOR.as_millis() / FRAME.as_millis()) as usize;
    let runs = runs();
    let mut subscribers = Vec::new();
    for run in 0..runs {
        let session = SessionId::fixture(format!("bench-run-{run}"));
        let start = b.current_cursor(&session, SessionRevision::new(1));
        let mut subscription = subscribed(b.subscribe_after_cursor(&start).await);
        let sent = Arc::clone(&sent);
        let latencies = Arc::clone(&latencies);
        subscribers.push(tokio::spawn(async move {
            let mut received = 0;
            while received < frames_per_run {
                let Some(Ok(event)) = next_item(&mut subscription).await else {
                    panic!("run {run}'s subscription ended after {received} frames");
                };
                let key = label(&event);
                let at = sent.lock().expect("sent map").remove(&key);
                if let Some(at) = at {
                    latencies.lock().expect("latencies").push(at.elapsed());
                }
                received += 1;
            }
        }));
    }

    let commits_before = commits(
        sqlx::query(commits_sql)
            .fetch_one(&mut probe)
            .await
            .expect("read commits"),
    );
    let cpu_before = postmaster.and_then(server_cpu_seconds);
    let began = Instant::now();
    let publishers = (0..runs)
        .map(|run| {
            let a = Arc::clone(&a);
            let sent = Arc::clone(&sent);
            let publish_latencies = Arc::clone(&publish_latencies);
            tokio::spawn(async move {
                let session = SessionId::fixture(format!("bench-run-{run}"));
                // Spread the runs across one frame: streaming runs do not
                // tick in step.
                tokio::time::sleep(FRAME * run as u32 / runs as u32).await;
                let mut ticker = tokio::time::interval(FRAME);
                for frame in 0..frames_per_run {
                    ticker.tick().await;
                    let body = format!("{run}:{frame}:{}", "x".repeat(FRAME_TEXT));
                    let at = Instant::now();
                    sent.lock().expect("sent map").insert(body.clone(), at);
                    a.publish(
                        &session,
                        SessionRevision::new(1),
                        vec![LiveReplayEventDraft::new(
                            None::<lash_vocabulary::identity::TurnId>,
                            text(&format!("bench:{run}#{frame}"), &body),
                        )],
                    )
                    .await
                    .expect("publish a frame");
                    publish_latencies
                        .lock()
                        .expect("publish latencies")
                        .push(at.elapsed());
                }
            })
        })
        .collect::<Vec<_>>();
    for publisher in publishers {
        publisher.await.expect("join a publisher");
    }
    for subscriber in subscribers {
        tokio::time::timeout(Duration::from_secs(30), subscriber)
            .await
            .expect("every subscriber receives every frame")
            .expect("join a subscriber");
    }
    let elapsed = began.elapsed();
    let cpu = postmaster
        .and_then(server_cpu_seconds)
        .zip(cpu_before)
        .map(|(after, before)| after - before);
    let commits_after = commits(
        sqlx::query(commits_sql)
            .fetch_one(&mut probe)
            .await
            .expect("read commits"),
    );

    let mut latencies = latencies.lock().expect("latencies").clone();
    latencies.sort_unstable();
    let mut publish_latencies = publish_latencies.lock().expect("publish latencies").clone();
    publish_latencies.sort_unstable();
    let events = runs * frames_per_run;
    let report = serde_json::json!({
        // Each run publishes its next frame when the last publish returns and
        // times from that send, so a late frame hides its own wait.
        "load_model": "service-diagnostic",
        "runs": runs,
        "frame_ms": FRAME.as_millis(),
        "frames_per_run": frames_per_run,
        "events": events,
        "measured": latencies.len(),
        "elapsed_s": elapsed.as_secs_f64(),
        "events_per_s": events as f64 / elapsed.as_secs_f64(),
        "publish_tick_ms": lash::postgres::ReplayDataPolicy::default().publish_tick.as_millis(),
        "transactions_per_s": (commits_after - commits_before) as f64 / elapsed.as_secs_f64(),
        "p50_ms": percentile(&latencies, 0.50).as_secs_f64() * 1e3,
        "p99_ms": percentile(&latencies, 0.99).as_secs_f64() * 1e3,
        "max_ms": latencies.last().map_or(0.0, |max| max.as_secs_f64() * 1e3),
        "publish_p50_ms": percentile(&publish_latencies, 0.50).as_secs_f64() * 1e3,
        "publish_p99_ms": percentile(&publish_latencies, 0.99).as_secs_f64() * 1e3,
        "publish_concurrency": lash::postgres::ReplayDataPolicy::default().publish_concurrency,
        "cpus": std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        "db_cpu_s": cpu,
        "db_cpu_cores": cpu.map(|cpu| cpu / elapsed.as_secs_f64()),
    });
    println!("LIVE_REPLAY_BENCH {report}");
    if statements_available {
        for row in sqlx::query(
            "SELECT calls, total_exec_time, mean_exec_time, left(regexp_replace(query, '\\s+', ' ', 'g'), 90) AS query \
             FROM pg_stat_statements ORDER BY total_exec_time DESC LIMIT 10",
        )
        .fetch_all(&mut probe)
        .await
        .expect("read statement statistics")
        {
            println!(
                "LIVE_REPLAY_BENCH_STATEMENT calls={} total_ms={:.0} mean_ms={:.3} {}",
                row.get::<i64, _>("calls"),
                row.get::<f64, _>("total_exec_time"),
                row.get::<f64, _>("mean_exec_time"),
                row.get::<String, _>("query"),
            );
        }
    }
    assert_eq!(latencies.len(), events, "every frame was measured");
}
