//! The core's node (ADR 0132 §3): the session actors of the core's backend
//! run on it, their turns in runtimes this core opens.
//!
//! The node starts with the core, on the runtime the core is built in (or on
//! the first session the core opens, when it was built outside one), and
//! stops when the core shuts down or its last clone is dropped. Its name is
//! the core's stable owner id: a new boot of the same owner fences the old.

use std::sync::{Arc, Mutex};

use lash_core::facade_support::LashRuntime;
use lash_core::runtime::durable::node::{NodeServe, serve};
use lash_core::runtime::durable::services::{RuntimeTurnServices, SessionRuntimes};
use lash_core::runtime::durable::session::{SessionActivation, TurnError, TurnServices};
use lash_core::{ExecutionBudgets, LiveReplayStore, SessionId};
use tokio_util::sync::CancellationToken;

use super::LashCore;

/// The core's one node, shared by its clones.
pub(crate) struct NodeSlot {
    stop: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl NodeSlot {
    pub(crate) fn new() -> Self {
        Self {
            stop: CancellationToken::new(),
            task: Mutex::new(None),
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
        let Ok(mut task) = self.task.lock() else {
            return;
        };
        if task.is_some() || self.stop.is_cancelled() {
            return;
        }
        // The node opens runtimes through a clone of the core that holds no
        // node of its own, so the node never keeps its core alive.
        let backend = core.backend.clone();
        let sessions = Arc::new(SessionActivation::new(
            backend.clone(),
            core.turn_services(),
            Arc::new(lash_core::durable_port::NoProbe),
        ));
        let node = lash_core::durable_port::NodeId::new(core.runtime_owner.owner_id.clone());
        let stop = self.stop.clone();
        *task = Some(runtime.spawn(async move {
            let served = serve(
                &backend,
                NodeServe {
                    node,
                    // The core stops its node on shutdown; it has no drain
                    // lever of its own.
                    drain: lash_core::durable_port::runner::Drain::default(),
                    sessions,
                    processes: None,
                },
                stop.cancelled_owned(),
            )
            .await;
            match served {
                Ok(stopped) => tracing::debug!(?stopped, "the core's node stopped"),
                Err(error) => tracing::error!(%error, "the core's node stopped"),
            }
        }));
    }

    /// Stop the node and wait for it.
    pub(crate) async fn stop(&self) {
        self.stop.cancel();
        let task = self.task.lock().ok().and_then(|mut task| task.take());
        if let Some(task) = task {
            let _ = task.await;
        }
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
}

impl LashCore {
    /// The turn services a node runs this core's sessions' turns with.
    pub(crate) fn turn_services(&self) -> Arc<dyn TurnServices> {
        Arc::new(RuntimeTurnServices::new(Arc::new(CoreRuntimes(
            self.detached(),
        ))))
    }

    /// This core without its node: what the node opens runtimes through.
    fn detached(&self) -> Self {
        Self {
            node: Arc::new(NodeSlot::detached()),
            ..self.clone()
        }
    }
}
