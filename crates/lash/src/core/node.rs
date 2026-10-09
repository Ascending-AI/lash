//! The core's node (ADR 0132 §3): the session and process actors of the
//! core's backend run on it, their turns in runtimes this core opens.
//!
//! Its processes run on the core's durable process worker: a `SessionTurn`
//! process mails its turn to its child session, and an engine process's
//! steps run the process's catalog tools and its engine's own bodies
//! (`ProcessSteps`).
//! The node advances every engine the core registers, a plugin's as a
//! host's.
//!
//! The node starts with the core, on the runtime the core is built in (or on
//! the first session the core opens, when it was built outside one), and
//! stops when the core shuts down or its last clone is dropped. Its name is
//! the core's stable owner id: a new boot of the same owner fences the old.
//!
//! The host drains it by release (ADR 0106 §1) with [`LashCore::drain`]: the
//! node claims nothing more, hands each actor it owns to the next build at a
//! committed phase, and stops. A drained core never starts a node again.

use std::sync::{Arc, Mutex};

use lash_core::durable_port::DurableError;
use lash_core::durable_port::DurableProbe;
use lash_core::durable_port::runner::{Activation, Drain, Stopped};
use lash_core::facade_support::LashRuntime;
use lash_core::runtime::durable::ProcessActivation;
use lash_core::runtime::durable::node::{NodeServe, serve};
use lash_core::runtime::durable::services::{RuntimeTurnServices, SessionRuntimes};
use lash_core::runtime::durable::session::{SessionActivation, TurnError, TurnServices};
use lash_core::{ExecutionBudgets, LiveReplayStore, SessionId};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::LashCore;
use super::drain::{NodeDrainError, NodeDrainReport};

/// How the node's serving ended.
type Served = Result<Stopped, DurableError>;

/// The core's one node, shared by its clones.
pub(crate) struct NodeSlot {
    stop: CancellationToken,
    /// The node's drain switch; started, the node never starts again.
    drain: Drain,
    /// Whether the node started.
    started: Mutex<bool>,
    /// How the node's serving ended, once it has.
    served: watch::Sender<Option<Served>>,
}

impl NodeSlot {
    pub(crate) fn new() -> Self {
        Self {
            stop: CancellationToken::new(),
            drain: Drain::default(),
            started: Mutex::new(false),
            served: watch::Sender::new(None),
        }
    }

    /// A slot that never starts a node: a core's that serves no sessions,
    /// and the one the node's own core clone holds.
    pub(crate) fn detached() -> Self {
        let slot = Self::new();
        slot.stop.cancel();
        slot
    }

    /// Start `core`'s node once, on the current runtime; without one it
    /// starts on the next call made on a runtime.
    pub(crate) fn ensure(&self, core: &LashCore) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Ok(mut started) = self.started.lock() else {
            return;
        };
        if *started || self.stop.is_cancelled() || self.drain.started() {
            return;
        }
        let NodeActivations {
            backend,
            sessions,
            processes,
        } = core.node_activations(Arc::new(lash_core::durable_port::NoProbe));
        let processes = match processes {
            Ok(processes) => Some(processes),
            Err(error) => {
                tracing::error!(%error, "the core's node serves no processes");
                None
            }
        };
        let node = lash_core::durable_port::NodeId::new(core.runtime_owner.owner_id.clone());
        let drain = self.drain.clone();
        let stop = self.stop.clone();
        let served = self.served.clone();
        *started = true;
        runtime.spawn(async move {
            let outcome = serve(
                &backend,
                NodeServe {
                    node,
                    drain,
                    sessions,
                    processes,
                },
                stop.cancelled_owned(),
            )
            .await;
            match &outcome {
                Ok(stopped) => tracing::debug!(?stopped, "the core's node stopped"),
                Err(error) => tracing::error!(%error, "the core's node stopped"),
            }
            served.send_replace(Some(outcome));
        });
    }

    /// How the node's serving ended, once it has; `None` when it never
    /// started.
    async fn ended(&self) -> Option<Served> {
        if !self.started.lock().is_ok_and(|started| *started) {
            return None;
        }
        let mut served = self.served.subscribe();
        let ended = served.wait_for(Option::is_some).await;
        ended.ok().and_then(|ended| ended.clone())
    }

    /// Drain `core`'s node by release and wait until it stops.
    pub(crate) async fn drain(&self, core: &LashCore) -> Result<NodeDrainReport, NodeDrainError> {
        // A core built outside a runtime starts its node now, so a drain
        // always meets a node that registered.
        self.ensure(core);
        self.drain.start();
        match self.ended().await {
            Some(Ok(Stopped::Drained)) => Ok(NodeDrainReport::of(&self.drain.released())),
            Some(Ok(stopped)) => Err(NodeDrainError::Stopped(stopped)),
            Some(Err(error)) => Err(NodeDrainError::Store(error)),
            None => Err(NodeDrainError::NotServing),
        }
    }

    /// How `core`'s node stopped, once it has; `None` when it runs none.
    pub(crate) async fn stopped(&self, core: &LashCore) -> Option<Served> {
        // A core built outside a runtime starts its node now.
        self.ensure(core);
        self.ended().await
    }

    /// Stop the node and wait for it.
    pub(crate) async fn stop(&self) {
        self.stop.cancel();
        let _ = self.ended().await;
    }
}

impl Drop for NodeSlot {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// The runtimes a core's node runs its sessions' turns in.
struct CoreRuntimes(LashCore);

#[async_trait::async_trait]
impl SessionRuntimes for CoreRuntimes {
    async fn open(&self, session: &SessionId) -> Result<LashRuntime, TurnError> {
        match self.0.session(session.clone()).open_runtime().await {
            Ok(runtime) => Ok(runtime),
            Err(crate::EmbedError::Runtime(error)) => Err(TurnError::Runtime(error)),
            Err(error) => Err(TurnError::Exec(format!(
                "session {session} did not open: {error}"
            ))),
        }
    }

    fn live_replay(&self) -> Arc<dyn LiveReplayStore> {
        Arc::clone(&self.0.live_replay_store)
    }

    fn execution_budgets(&self) -> ExecutionBudgets {
        self.0.env.core.control.execution_budgets.clone()
    }

    fn tracing(&self) -> &lash_core::runtime::TraceRuntime {
        &self.0.env.core.tracing
    }
}

/// What the core's node serves.
pub(crate) struct NodeActivations {
    /// The core's backend, advancing every engine the core registers.
    pub(crate) backend: lash_core::Backend,
    /// Runs the claimed sessions' turns.
    pub(crate) sessions: Arc<SessionActivation>,
    /// Runs the claimed processes on the core's process worker, or why the
    /// worker did not build.
    pub(crate) processes: crate::Result<Arc<dyn Activation>>,
}

impl LashCore {
    /// The backend and activations the core's node serves, each reporting
    /// to `probe`. The node opens runtimes through a clone of the core that
    /// holds no node of its own, so the node never keeps its core alive.
    pub(crate) fn node_activations(&self, probe: Arc<dyn DurableProbe>) -> NodeActivations {
        let backend = self
            .backend
            .with_process_engines(self.host_process_engines.engines().cloned());
        let sessions = Arc::new(SessionActivation::new(
            backend.clone(),
            self.turn_services(),
            Arc::clone(&probe),
        ));
        let processes = self.process_worker().map(|worker| {
            let worker = Arc::new(worker);
            Arc::new(
                ProcessActivation::new(
                    backend.clone(),
                    lash_core_worker::process_steps(&worker),
                    probe,
                )
                .with_tracing(self.env.core.tracing.clone())
                .with_session_turns(worker)
                .with_process_events(self.substrate_slot.setup.process.watched().clone()),
            ) as Arc<dyn Activation>
        });
        NodeActivations {
            backend,
            sessions,
            processes,
        }
    }

    /// The hub that ticks when a commit grew a process's log, on this
    /// core's node or, through its node wakes, on another.
    #[cfg(test)]
    pub(crate) fn process_changes(&self) -> &lash_core::runtime::ProcessChangeHub {
        self.substrate_slot.setup.process.watched().hub()
    }

    /// The hub that ticks once this core's sinks, its observation
    /// dispatcher among them, were handed what a commit appended to a
    /// process's log, and when another node appended to it.
    pub(crate) fn process_emissions(&self) -> &lash_core::runtime::ProcessChangeHub {
        self.substrate_slot.setup.process.watched().emitted_hub()
    }

    /// The worker the core's node runs its processes on.
    fn process_worker(&self) -> crate::Result<lash_core_worker::DurableProcessWorker> {
        Ok(lash_core_worker::DurableProcessWorker::new(
            self.durable_process_worker_config()?,
        ))
    }

    /// The turn services a node runs this core's sessions' turns with.
    pub(crate) fn turn_services(&self) -> Arc<dyn TurnServices> {
        Arc::new(RuntimeTurnServices::new(
            Arc::new(CoreRuntimes(self.detached())),
            Arc::clone(&self.published_heads),
        ))
    }

    /// This core without its node: what the node opens runtimes through.
    fn detached(&self) -> Self {
        Self {
            node: Arc::new(NodeSlot::detached()),
            ..self.clone()
        }
    }
}
