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
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Cut off from the database's lease: every heartbeat fails before it
    /// enters the store, while every other call gets through.
    partitioned: AtomicBool,
    /// A pause holds the node's runner until [`Self::release_runner`], and
    /// each read yields before it answers.
    activations_first: AtomicBool,
    /// The node's runner is not polled, while its activations run on.
    runner_held: watch::Sender<bool>,
}

impl NodeLife {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: watch::Sender::new(Life::Running),
            partitioned: AtomicBool::new(false),
            activations_first: AtomicBool::new(false),
            runner_held: watch::Sender::new(false),
        })
    }

    pub(crate) fn run_activations_first(&self) {
        self.activations_first.store(true, Ordering::SeqCst);
    }

    /// Whether the node runs its activations first: see
    /// [`SimNodes::run_activations_first`](crate::SimNodes::run_activations_first).
    pub(crate) fn activations_first(&self) -> bool {
        self.activations_first.load(Ordering::SeqCst)
    }

    pub(crate) fn release_runner(&self) {
        self.runner_held.send_replace(false);
    }

    /// Run `runner`, polling it only while it is not held.
    pub(crate) async fn runner<F: std::future::Future>(&self, runner: F) -> F::Output {
        if !self.activations_first() {
            return runner.await;
        }
        let mut held = self.runner_held.subscribe();
        tokio::pin!(runner);
        loop {
            if *held.borrow_and_update() {
                let _ = held.wait_for(|held| !*held).await;
                continue;
            }
            tokio::select! {
                biased;
                output = &mut runner => return output,
                _ = held.changed() => {}
            }
        }
    }

    pub(crate) fn partition(&self, partitioned: bool) {
        self.partitioned.store(partitioned, Ordering::SeqCst);
    }

    pub(crate) fn partitioned(&self) -> bool {
        self.partitioned.load(Ordering::SeqCst)
    }

    pub(crate) fn get(&self) -> Life {
        *self.state.borrow()
    }

    pub(crate) fn pause(&self) {
        let paused = self.state.send_if_modified(|life| {
            let paused = *life == Life::Running;
            if paused {
                *life = Life::Paused;
            }
            paused
        });
        if paused && self.activations_first() {
            self.runner_held.send_replace(true);
        }
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

    /// Whether the node runs with its runner polled: a call it waits to
    /// make can go now.
    pub(crate) fn live(&self) -> bool {
        *self.state.borrow() == Life::Running && !*self.runner_held.borrow()
    }

    /// Resolve at the node's first change of life or runner hold after this
    /// call.
    pub(crate) fn changes(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut state = self.state.subscribe();
        let mut held = self.runner_held.subscribe();
        async move {
            let changed = tokio::select! {
                changed = state.changed() => changed,
                changed = held.changed() => changed,
            };
            if changed.is_err() {
                std::future::pending::<()>().await;
            }
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
