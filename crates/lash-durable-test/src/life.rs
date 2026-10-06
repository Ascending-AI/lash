//! A simulated node's life: running, paused or dead.
//!
//! A pause models a process stopped whole (a long GC or a `SIGSTOP`): none of
//! its store calls enters the store and none of its timers fires until it
//! resumes, and then it carries on with whatever it held in memory. A crash
//! models a dead process: nothing it started runs again, and none of its
//! later calls enters the store.

use crate::clock::SimClock;
use lash_core_ids::clock::Clock;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Life {
    Running,
    Paused,
    Dead,
}

/// One node incarnation's life, shared by its fault store, its clock and
/// the deployment that runs it.
#[derive(Debug)]
pub(crate) struct NodeLife {
    state: watch::Sender<Life>,
}

impl NodeLife {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: watch::Sender::new(Life::Running),
        })
    }

    pub(crate) fn get(&self) -> Life {
        *self.state.borrow()
    }

    pub(crate) fn pause(&self) {
        self.state.send_if_modified(|life| {
            let paused = *life == Life::Running;
            if paused {
                *life = Life::Paused;
            }
            paused
        });
    }

    pub(crate) fn resume(&self) {
        self.state.send_if_modified(|life| {
            let resumed = *life == Life::Paused;
            if resumed {
                *life = Life::Running;
            }
            resumed
        });
    }

    pub(crate) fn kill(&self) {
        self.state.send_replace(Life::Dead);
    }

    /// Return once the node runs; never return when it is dead, so the
    /// caller's task stays parked until the deployment drops it.
    pub(crate) async fn running(&self) {
        let mut life = self.state.subscribe();
        if life.wait_for(|life| *life != Life::Paused).await.is_err() {
            return std::future::pending().await;
        }
        if *life.borrow() == Life::Dead {
            std::future::pending::<()>().await;
        }
    }

    /// Return once the node is dead.
    pub(crate) async fn dead(&self) {
        let mut life = self.state.subscribe();
        let _ = life.wait_for(|life| *life == Life::Dead).await;
    }
}

/// The clock one node's runtime sees: the deployment's virtual time, whose
/// timers do not fire while the node is paused or dead.
#[derive(Debug)]
pub(crate) struct NodeClock {
    sim: Arc<SimClock>,
    life: Arc<NodeLife>,
}

impl NodeClock {
    pub(crate) fn new(sim: Arc<SimClock>, life: Arc<NodeLife>) -> Arc<Self> {
        Arc::new(Self { sim, life })
    }
}

#[async_trait::async_trait]
impl Clock for NodeClock {
    fn now(&self) -> Instant {
        self.sim.now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        self.sim.timestamp_datetime()
    }

    async fn sleep(&self, duration: Duration) {
        self.sim.sleep(duration).await;
        self.life.running().await;
    }

    async fn sleep_until(&self, deadline: Instant) {
        self.sim.sleep_until(deadline).await;
        self.life.running().await;
    }
}
