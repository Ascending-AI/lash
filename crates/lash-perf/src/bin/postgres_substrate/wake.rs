//! Broadcast wake observations on one dedicated connection per simulated node.

use super::{Distribution, connect, emit, micros, sql};
use anyhow::{Result, ensure};
use futures_util::future::try_join_all;
use sqlx::{
    PgConnection,
    postgres::{PgConnectOptions, PgListener},
};
use std::time::{Duration, Instant};
use tokio::time::{MissedTickBehavior, interval_at, sleep, timeout};

#[derive(Clone, Copy)]
enum Mode {
    NotifyInTransaction,
    NotifyAfterCommit,
    Poll(u64),
}

impl Mode {
    fn name(self) -> String {
        match self {
            Self::NotifyInTransaction => "wake_notify_in_transaction".into(),
            Self::NotifyAfterCommit => "wake_notify_after_commit".into(),
            Self::Poll(ms) => format!("wake_poll_{ms}ms"),
        }
    }
}

async fn observe_notify(mut listener: PgListener, events: usize) -> Result<Vec<Instant>> {
    let mut seen = Vec::with_capacity(events);
    for seq in 0..events {
        let notification = listener.recv().await?;
        ensure!(
            notification.payload().parse::<usize>()? == seq,
            "notification missing or reordered"
        );
        seen.push(Instant::now());
    }
    Ok(seen)
}

async fn observe_poll(
    mut conn: PgConnection,
    events: usize,
    ms: u64,
    node: usize,
) -> Result<Vec<Instant>> {
    let mut seen = Vec::with_capacity(events);
    // Independent phases prevent a fleet from polling in lockstep.
    let phase = (node as u64 * 37) % ms;
    let mut ticker = interval_at(
        tokio::time::Instant::now() + Duration::from_millis(phase),
        Duration::from_millis(ms),
    );
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    while seen.len() < events {
        ticker.tick().await;
        let last = i64::try_from(seen.len())? - 1;
        let rows: Vec<(i64,)> = sqlx::query_as(sql::POLL)
            .bind(last)
            .fetch_all(&mut conn)
            .await?;
        let observed = Instant::now();
        for (seq,) in rows {
            ensure!(seq == i64::try_from(seen.len())?, "durable wake gap");
            seen.push(observed);
        }
    }
    Ok(seen)
}

async fn one_mode(
    options: &PgConnectOptions,
    schema: &str,
    nodes: usize,
    events: usize,
    mode: Mode,
) -> Result<()> {
    let mut producer = connect(options, schema).await?;
    sqlx::raw_sql("TRUNCATE wake_events")
        .execute(&mut producer)
        .await?;
    let channel = format!("{schema}_ready");
    let mut listeners = Vec::new();
    let mut polls = Vec::new();
    for _ in 0..nodes {
        match mode {
            Mode::Poll(_) => polls.push(connect(options, schema).await?),
            _ => {
                let mut listener = PgListener::connect_with(&producer_pool(options).await?).await?;
                listener.listen(&channel).await?;
                listeners.push(listener);
            }
        }
    }
    let start = Instant::now();
    let writer = async {
        let mut sent = Vec::with_capacity(events);
        let mut admission = Vec::with_capacity(events);
        let mut notify_rtt = Vec::new();
        let mut notify_sent = Vec::new();
        for seq in 0..events {
            // Deterministic variable gaps sample many polling phases, avoiding phase-locked arrivals.
            sleep(Duration::from_millis(5 + ((seq * 17) % 41) as u64)).await;
            let before = Instant::now();
            sent.push(before);
            match mode {
                Mode::NotifyInTransaction => {
                    notify_sent.push(before);
                    sqlx::query(sql::WAKE_NOTIFY)
                        .bind(i64::try_from(seq)?)
                        .bind(&channel)
                        .execute(&mut producer)
                        .await?;
                }
                _ => {
                    sqlx::query(sql::WAKE)
                        .bind(i64::try_from(seq)?)
                        .execute(&mut producer)
                        .await?;
                }
            }
            admission.push(micros(before.elapsed()));
            if matches!(mode, Mode::NotifyAfterCommit) {
                let notify_start = Instant::now();
                notify_sent.push(notify_start);
                sqlx::query(sql::NOTIFY)
                    .bind(&channel)
                    .bind(seq.to_string())
                    .execute(&mut producer)
                    .await?;
                notify_rtt.push(micros(notify_start.elapsed()));
            }
        }
        Ok::<_, anyhow::Error>((sent, admission, notify_rtt, notify_sent))
    };
    let observers = async {
        match mode {
            Mode::Poll(ms) => {
                try_join_all(
                    polls
                        .into_iter()
                        .enumerate()
                        .map(|(node, conn)| observe_poll(conn, events, ms, node)),
                )
                .await
            }
            _ => {
                try_join_all(
                    listeners
                        .into_iter()
                        .map(|listener| observe_notify(listener, events)),
                )
                .await
            }
        }
    };
    let ((sent, admission, notify_rtt, notify_sent), seen) =
        timeout(Duration::from_secs(30), async {
            tokio::try_join!(writer, observers)
        })
        .await??;
    let elapsed = start.elapsed().as_secs_f64();
    let mut delivery = Vec::new();
    let mut by_node = Vec::new();
    let mut notify_delivery = Vec::new();
    for node_seen in seen {
        for (&received, &sent) in node_seen.iter().zip(&notify_sent) {
            notify_delivery.push(micros(received.duration_since(sent)));
        }
        let latencies: Vec<_> = node_seen
            .into_iter()
            .zip(&sent)
            .map(|(received, sent)| micros(received.duration_since(*sent)))
            .collect();
        by_node.push(Distribution::new(latencies.clone())?);
        delivery.extend(latencies);
    }
    emit(&serde_json::json!({"operation":mode.name(),"nodes":nodes,
        "elapsed_s":elapsed, "deliveries_per_s":delivery.len() as f64/elapsed,
        "latency_origin":"before durable admission statement; includes commit and post-commit notify when used",
        "delivery":Distribution::new(delivery)?, "per_node_delivery":by_node,
        "admission":Distribution::new(admission)?,
        "notify_delivery":if notify_delivery.is_empty() {None} else {Some(Distribution::new(notify_delivery)?)} ,
        "notify_round_trip":if notify_rtt.is_empty() {None} else {Some(Distribution::new(notify_rtt)?)} }))
}

async fn producer_pool(options: &PgConnectOptions) -> Result<sqlx::PgPool> {
    Ok(sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await?)
}

pub(super) async fn measure(
    options: &PgConnectOptions,
    schema: &str,
    nodes: usize,
    events: usize,
) -> Result<()> {
    for mode in [
        Mode::NotifyInTransaction,
        Mode::NotifyAfterCommit,
        Mode::Poll(250),
        Mode::Poll(1000),
    ] {
        one_mode(options, schema, nodes, events, mode).await?;
    }
    Ok(())
}
