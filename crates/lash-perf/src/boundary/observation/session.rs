//! Session observation: the live replay's publish and subscribe path, its
//! fan-out to observers, cursor resume, store-wide invalidation and the
//! window between a commit and its publication.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt as _;
use lash::observe::{SessionObservationStream, SessionObservationStreamItem, SessionResume};
use lash_core::SessionObservationEventPayload;

use super::super::{Args, Case, Meter, Receipt, facade};
use super::{Flavor, Fleet, Node, within};

pub(super) async fn run(args: &Args) -> Result<Receipt> {
    let meter = Meter::new(args.ledger_cap);
    let fleet = Fleet::open(args, &meter, 1, Flavor::Chat, |builder, _| Ok(builder)).await?;
    let result = match args.case {
        Case::SessionReplay => replay(args, &fleet.nodes[0], &meter).await,
        Case::SessionResume => resume(args, &fleet.nodes[0], &meter).await,
        other => Err(anyhow::anyhow!(
            "{other:?} is not a session observation workload"
        )),
    };
    let store = fleet.store;
    fleet.close().await?;
    let (counters, evidence) = result?;
    Ok(Receipt::measured(
        args.case,
        "facade-send+session-feed",
        store,
        args.operations,
        &meter,
        counters,
        evidence,
    ))
}

type Outcome = Result<(serde_json::Value, serde_json::Value)>;

/// What one session feed delivered.
#[derive(Default, serde::Serialize)]
struct Delivered {
    events: BTreeMap<&'static str, usize>,
    gaps: usize,
    /// When each `Committed` arrived, in order.
    #[serde(skip)]
    commits: Vec<Instant>,
}

fn kind(payload: &SessionObservationEventPayload) -> &'static str {
    match payload {
        SessionObservationEventPayload::LanguageExecution(_) => "language_execution",
        SessionObservationEventPayload::TurnActivity(_) => "turn_activity",
        SessionObservationEventPayload::Committed { .. } => "committed",
        SessionObservationEventPayload::ResidentChanged => "resident_changed",
        SessionObservationEventPayload::AgentFrameSwitched { .. } => "agent_frame_switched",
        SessionObservationEventPayload::QueueChanged { .. } => "queue_changed",
        SessionObservationEventPayload::ProcessChanged { .. } => "process_changed",
    }
}

/// Read `feed` until it has delivered `commits` commits or `gaps` gaps.
async fn follow(
    feed: &mut SessionObservationStream,
    meter: &Meter,
    delivered: &mut Delivered,
    commits: usize,
    gaps: usize,
) -> Result<()> {
    let (commits, gaps) = (delivered.commits.len() + commits, delivered.gaps + gaps);
    while delivered.commits.len() < commits || delivered.gaps < gaps {
        let start = Instant::now();
        let item = within("session feed item", feed.next())
            .await?
            .context("the session feed ended")??;
        meter.operation(
            "session.feed.next",
            match &item {
                SessionObservationStreamItem::Event(event) => event.cursor.as_str().to_owned(),
                SessionObservationStreamItem::Gap { .. } => "feed-gap".into(),
            },
            "ok",
            start,
        );
        match item {
            SessionObservationStreamItem::Event(event) => {
                *delivered.events.entry(kind(&event.payload)).or_default() += 1;
                if matches!(
                    event.payload,
                    SessionObservationEventPayload::Committed { .. }
                ) {
                    delivered.commits.push(Instant::now());
                }
            }
            SessionObservationStreamItem::Gap { .. } => delivered.gaps += 1,
        }
    }
    Ok(())
}

async fn open(node: &Node, name: &str) -> Result<(lash::DurableSession, lash::LashSession)> {
    let name = super::unique(name);
    let durable = facade::create(&node.core, &name).await?;
    let session = node
        .core
        .session(lash::SessionId::try_from(name)?)
        .open()
        .await?;
    Ok((durable, session))
}

/// One session settles `--operations` sends while `--callers` observers
/// follow its feed. The receipt states the store's publications, what each
/// observer was delivered, and how long after a send settled its `Committed`
/// reached an observer.
async fn replay(args: &Args, node: &Node, meter: &Meter) -> Outcome {
    let (durable, session) = open(node, "replay").await?;
    let mut observers = Vec::new();
    for _ in 0..args.callers {
        let observed = session.observe();
        let start = Instant::now();
        let snapshot = within("session snapshot", observed.snapshot()).await??;
        meter.operation("session.feed.snapshot", session.session_id(), "ok", start);
        let mut feed = observed.subscribe_and_recover(snapshot.cursor);
        let (meter, operations) = (meter.clone(), args.operations);
        observers.push(tokio::spawn(async move {
            let mut delivered = Delivered::default();
            follow(&mut feed, &meter, &mut delivered, operations, 0).await?;
            anyhow::Ok(delivered)
        }));
    }
    let window = Instant::now();
    let mut settled = Vec::new();
    for n in 0..args.operations {
        facade::send(&durable, &format!("replay-{n}"), meter).await?;
        settled.push(Instant::now());
    }
    let mut delivered = Vec::new();
    for observer in observers {
        delivered.push(observer.await??);
    }
    let (mut events, mut published_first) = (0, 0);
    for (observer_index, observer) in delivered.iter().enumerate() {
        events += observer.events.values().sum::<usize>();
        ensure!(observer.gaps == 0, "an attached observer gapped");
        for (index, (commit, settle)) in observer.commits.iter().zip(&settled).enumerate() {
            match commit.checked_duration_since(*settle) {
                Some(_) => meter.interval(
                    "session.commit_to_publication",
                    format!(
                        "session:{}/turn:replay-{index}/observer:{observer_index}",
                        session.session_id()
                    ),
                    "delivered_after_settle",
                    *settle,
                    *commit,
                ),
                None => published_first += 1,
            }
        }
    }
    meter.window("session.replay.deliveries", events, window);
    let counts = node.live_replay.counts();
    meter.window(
        "session.replay.publications",
        counts.published as usize,
        window,
    );
    Ok((
        serde_json::json!({
            "events_delivered": events,
            "events_delivered_per_observer": events as f64 / args.callers as f64,
            "commits_published_before_send_settled": published_first,
            "commits_published_after_send_settled":
                args.callers * args.operations - published_first,
            "live_replay": counts,
            "observers": delivered,
        }),
        serde_json::json!({
            "observers": args.callers,
            "settled_sends": args.operations,
            "commits_delivered_per_observer": args.operations,
        }),
    ))
}

/// One session settles `--operations` sends and keeps the cursor of each
/// revision; each cursor is then resumed. `--callers` sessions with one
/// attached observer each then lose continuity to one store-wide
/// invalidation, and a further send shows each feed continuing past its gap.
async fn resume(args: &Args, node: &Node, meter: &Meter) -> Outcome {
    let mut sessions = Vec::new();
    for n in 0..args.callers {
        sessions.push(open(node, &format!("resume-{n}")).await?);
    }
    let (durable, session) = &sessions[0];
    let observed = session.observe();
    let mut cursors = vec![observed.snapshot().await?.cursor];
    for n in 0..args.operations {
        facade::send(durable, &format!("resume-0-{n}"), meter).await?;
        cursors.push(observed.snapshot().await?.cursor);
    }
    let window = Instant::now();
    let (mut replayed, mut replayed_events, mut gapped) = (0, 0, 0);
    for cursor in &cursors {
        let start = Instant::now();
        match within("cursor resume", observed.resume_from_cursor(cursor)).await?? {
            SessionResume::Replayed { events } => {
                replayed += 1;
                replayed_events += events.len();
                meter.operation(
                    "session.resume.replayed",
                    format!(
                        "session:{}/cursor:{}",
                        session.session_id(),
                        cursor.as_str()
                    ),
                    "replayed",
                    start,
                );
            }
            SessionResume::Gap { .. } => {
                gapped += 1;
                meter.operation(
                    "session.resume.gap",
                    format!(
                        "session:{}/cursor:{}",
                        session.session_id(),
                        cursor.as_str()
                    ),
                    "gap",
                    start,
                );
            }
        }
    }
    meter.window("session.resume.cursors", cursors.len(), window);

    let mut feeds = Vec::new();
    for (_, session) in &sessions {
        let observed = session.observe();
        let snapshot = within("session snapshot", observed.snapshot()).await??;
        let mut feed = observed.subscribe_and_recover(snapshot.cursor);
        // The feed subscribes at its first poll; nothing is pending yet.
        ensure!(
            tokio::time::timeout(std::time::Duration::from_millis(200), feed.next())
                .await
                .is_err(),
            "an idle session feed delivered an item"
        );
        feeds.push(feed);
    }
    let store: Arc<dyn lash_core::LiveReplayStore> = node.live_replay.clone();
    let start = Instant::now();
    store.invalidate_all().await?;
    meter.operation("session.invalidate_all", "session-population", "ok", start);
    let mut delivered = Vec::new();
    for mut feed in feeds {
        let mut observer = Delivered::default();
        follow(&mut feed, meter, &mut observer, 0, 1).await?;
        meter.operation(
            "session.invalidate.gap_delivery",
            format!("observer:{}", delivered.len()),
            "gap",
            start,
        );
        delivered.push((feed, observer));
    }
    let gaps: usize = delivered.iter().map(|(_, observer)| observer.gaps).sum();
    let start = Instant::now();
    for (n, ((durable, _), (feed, observer))) in sessions.iter().zip(&mut delivered).enumerate() {
        facade::send(durable, &format!("resume-{n}-after-gap"), meter).await?;
        follow(feed, meter, observer, 1, 0).await?;
    }
    meter.aggregate(
        "session.invalidate.recovered",
        sessions.len(),
        "session-population",
        "ok",
        start,
    );
    Ok((
        serde_json::json!({
            "cursors_resumed": cursors.len(),
            "resumes_replayed": replayed,
            "resumes_gapped": gapped,
            "events_replayed": replayed_events,
            "invalidate_all_calls": 1,
            "observers_gapped": gaps,
            "observers_continued_after_gap": delivered.len(),
            "live_replay": node.live_replay.counts(),
        }),
        serde_json::json!({
            "sessions": args.callers,
            "revisions_resumed": cursors.len(),
            "observers_gapped": gaps,
        }),
    ))
}
