//! The populations scheduled arrivals are sent to.
//!
//! Both send through `send()` and follow the outcome; neither drives a turn.
//! Arrivals are spread over the sessions by ordinal, and a session runs one
//! turn at a time, so an arrival that lands on a busy session queues in the
//! session's durable input queue with its clock already running.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use lash_sansio::SessionId;

use super::ledger::{Arrival, ServiceMarks};
use super::{Marks, Population};
use crate::latency::FollowerMode;
use crate::latency::runner::{
    CaseTopology, LaneSession, PollMarks, STORE_POLL_CEILING, Topology, create_lane_session,
    observer_poll_store, poll_marks, status_name,
};
use crate::runtime_perf::{HighTrafficMix, HighTrafficPopulation, high_traffic_prompt};

pub(super) const MARKS: Marks = Marks {
    clock: "monotonic, in the generator process",
    scheduled: "the arrival's due instant, ordinal / rate after the window opened",
    sent: "the generator calls send(), after any wait for an in-flight slot",
    admitted: "a store poller first reads the input as taken by a run; observed up to one \
               poll interval (2 ms to 50 ms) late",
    settled: "the store poller first reads the run's terminal; observed up to one poll \
              interval late",
    completed: "the send's outcome returns to the caller",
};

/// How long a completed send waits for its poller's last marks.
const POLL_GRACE: Duration = Duration::from_millis(4 * STORE_POLL_CEILING.as_millis() as u64);

/// Teardown bound for a session a failed step left busy.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

struct Target {
    session: LaneSession,
    session_id: SessionId,
    poll_store: Arc<dyn lash::persistence::RuntimeStore>,
}

enum Owner {
    HighTraffic(Box<HighTrafficPopulation>),
    CrossWorker(Box<CaseTopology>),
}

pub(super) struct Opened {
    owner: Owner,
    targets: Arc<Vec<Target>>,
    mix: Option<HighTrafficMix>,
}

impl Opened {
    pub(super) async fn open(
        population: Population,
        store_dir: &Path,
        sessions: usize,
        mix: &HighTrafficMix,
    ) -> Result<Self> {
        std::fs::create_dir(store_dir)
            .with_context(|| format!("create fresh {}", store_dir.display()))?;
        let mut targets = Vec::with_capacity(sessions);
        match population {
            Population::HighTraffic => {
                let (population, live) =
                    HighTrafficPopulation::open(store_dir.to_path_buf(), sessions).await?;
                // The poller reads through the serving core's own store.
                let observer = population.core();
                for session in live {
                    let session_id = session.session_id();
                    targets.push(Target {
                        poll_store: observer_poll_store(&observer, &session_id).await?,
                        session: LaneSession::Live(session),
                        session_id,
                    });
                }
                Ok(Self {
                    owner: Owner::HighTraffic(Box::new(population)),
                    targets: Arc::new(targets),
                    mix: Some(mix.clone()),
                })
            }
            Population::CrossWorker => {
                let topology =
                    CaseTopology::cross_worker(store_dir, FollowerMode::Standard).await?;
                for index in 0..sessions {
                    let session_id = SessionId::fixture(format!("offered-load-{index}"));
                    targets.push(Target {
                        session: create_lane_session(&topology, Topology::CrossWorker, &session_id)
                            .await?,
                        poll_store: observer_poll_store(&topology.observer, &session_id).await?,
                        session_id,
                    });
                }
                Ok(Self {
                    owner: Owner::CrossWorker(Box::new(topology)),
                    targets: Arc::new(targets),
                    mix: None,
                })
            }
        }
    }

    /// One send-and-follow per arrival, on the arrival's session.
    pub(super) fn service(
        &self,
    ) -> impl Fn(Arrival) -> BoxFuture<'static, Result<ServiceMarks>> + Send + Sync + 'static {
        let targets = Arc::clone(&self.targets);
        let mix = self.mix.clone();
        move |arrival| {
            let targets = Arc::clone(&targets);
            let mix = mix.clone();
            Box::pin(async move {
                let target = &targets[arrival.ordinal % targets.len()];
                let text = match &mix {
                    Some(mix) => high_traffic_prompt(
                        mix.operation_kind(arrival.ordinal),
                        arrival.ordinal,
                        &target.session_id,
                    ),
                    None => format!("offered-load {}", arrival.key),
                };
                send_and_follow(target, text).await
            })
        }
    }

    pub(super) async fn close(self) -> Result<()> {
        // Teardown only. Operations the window left unfinished may still
        // hold a session; their sessions end with the core.
        if let Ok(targets) = Arc::try_unwrap(self.targets) {
            for target in targets {
                let _ = tokio::time::timeout(CLOSE_TIMEOUT, target.session.close()).await;
            }
        }
        match self.owner {
            Owner::HighTraffic(population) => population.close().await,
            Owner::CrossWorker(topology) => topology.shutdown().await,
        }
    }
}

/// Aborts the store poller when its send is dropped at window close.
struct Poller(tokio::task::JoinHandle<PollMarks>);

impl Drop for Poller {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn send_and_follow(target: &Target, text: String) -> Result<ServiceMarks> {
    let sent = Instant::now();
    let handle = target
        .session
        .send(lash::TurnInput::text(text))
        .into_future()
        .await
        .map_err(anyhow::Error::from)?;
    let mut poller = Poller(tokio::spawn(poll_marks(
        Arc::clone(&target.poll_store),
        target.session_id.clone(),
        handle.input_id().clone(),
        sent,
    )));
    let outcome = handle.outcome().await.map_err(anyhow::Error::from)?;
    let completed = sent.elapsed();
    let status = outcome.status();
    anyhow::ensure!(
        matches!(status, lash::TurnStatus::Answered),
        "the turn ended {}",
        status_name(&status)
    );
    let marks = match tokio::time::timeout(POLL_GRACE, &mut poller.0).await {
        Ok(Ok(marks)) => marks,
        _ => PollMarks::default(),
    };
    let after_send = |ms: Option<f64>| ms.map(|ms| Duration::from_secs_f64(ms.max(0.0) / 1_000.0));
    Ok(ServiceMarks {
        admitted_after_send: after_send(marks.admission_ms),
        settled_after_send: after_send(marks.settled_ms),
        completed_after_send: Some(completed),
    })
}
