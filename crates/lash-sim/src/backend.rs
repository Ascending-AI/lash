//! The backends the simulator drives its runtimes over.
//!
//! A simulated turn runs where a deployment runs one: inside a handler of
//! lash-restate's engine, on the in-process Restate server double
//! ([`SimEngine`]), over a SQLite memory store set; SQLite is storage only.
//! When the simulator records the checkpoint writes a run commits, it wraps
//! the engine backend's session factory in an observer, and every other port
//! stays the backend's.

use std::sync::Arc;

use lash_core::Backend;
use lash_core::sync::MutexExt as _;

use crate::runner::FixedScriptRunnerError;
use crate::store::{CheckpointWriteCollector, ObservedSessionStoreFactory};

/// Where the simulator runs turns: lash-restate's engine on a fresh
/// in-process Restate server double under the scenario's seed, with serial
/// scheduling, over a SQLite memory store set.
///
/// A turn is sent to the session and the engine's session drive runs it
/// ([`run_turn`](Self::run_turn)); a core that starts processes serves their
/// segments through [`serve_processes`](Self::serve_processes).
#[derive(Clone, Debug)]
pub struct SimEngine {
    restate: lash_restate_test::RestateTestBackend,
}

/// Builds the send a turn starts from. The input is accepted on the
/// session and the engine's session drive runs it, on the server double.
pub type SimTurnBuild =
    Arc<dyn Fn(&lash::LashSession) -> lash::Result<lash::SendBuilder> + Send + Sync>;

impl SimEngine {
    /// A fresh engine on a server double under `seed`, scheduled serially:
    /// one attempt runs at a time, and one seed grants the turn in one order
    /// on a current-thread runtime.
    pub async fn new(seed: u64) -> Result<Self, FixedScriptRunnerError> {
        Self::scheduled(seed, lash_restate_test::Scheduling::Serial).await
    }

    /// A fresh engine on a server double under `seed` whose live attempts
    /// run whenever Tokio polls them, as `restate-server` does: the
    /// generated search lane's cross-session concurrency. A scenario that
    /// stops a running turn from outside it needs this too: a host-local stop
    /// is a durable request on the turn's cancellation gate, an ingress call,
    /// and serial scheduling lands ingress only between attempts, so it would
    /// wait on the very attempt it stops (FIG-3672 P9).
    pub async fn concurrent(seed: u64) -> Result<Self, FixedScriptRunnerError> {
        Self::scheduled(seed, lash_restate_test::Scheduling::Concurrent).await
    }

    async fn scheduled(
        seed: u64,
        scheduling: lash_restate_test::Scheduling,
    ) -> Result<Self, FixedScriptRunnerError> {
        let mut config = lash_restate_test::ServerConfig::default().scheduling(scheduling);
        // A crashed attempt is retried at once: retry timing is no contract,
        // and a simulated world crashes an attempt on every durable effect.
        config.retry.initial_interval = std::time::Duration::from_millis(1);
        config.retry.max_interval = std::time::Duration::from_millis(10);
        lash_restate_test::backend(seed, config)
            .await
            .map(|restate| Self { restate })
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))
    }

    /// The server double and the engine wired to it.
    pub fn restate(&self) -> &lash_restate_test::RestateTestBackend {
        &self.restate
    }

    /// The backend a core runs on: the engine's own backend, with its
    /// Lashlang artifacts in the engine's store set.
    pub fn backend(&self) -> Backend {
        DecoratedBackend::over_engine(self).into()
    }

    /// Serve process segments with `core`'s durable worker, as a Restate
    /// deployment does.
    pub fn serve_processes(&self, core: &lash::LashCore) -> Result<(), FixedScriptRunnerError> {
        let config = core
            .durable_process_worker_config()
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        self.restate.install_process_worker(
            lash::durability::DurableProcessWorker::new(config)
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?,
        );
        Ok(())
    }

    /// Run one turn of `session` on `prompt`, named `turn_id`, keeping only
    /// its output.
    pub async fn run_text_turn(
        &self,
        session: &lash::LashSession,
        turn_id: impl Into<lash::TurnId>,
        prompt: impl Into<String>,
    ) -> Result<lash::Result<lash::TurnOutput>, FixedScriptRunnerError> {
        let prompt = prompt.into();
        self.run_turn(
            session,
            turn_id,
            Arc::new(DiscardedTurnActivity),
            Arc::new(move |session: &lash::LashSession| {
                Ok(session.send(lash::TurnInput::text(prompt.clone())))
            }),
        )
        .await
    }

    /// Send one turn of `session`, named `turn_id`, and wait for the
    /// engine's session drive to settle it on the server double, streaming
    /// its activity to `events`. The host never drives the turn (D5): it
    /// accepts the input and waits. The outer result is the harness's; the
    /// inner one is the turn's own.
    pub async fn run_turn(
        &self,
        session: &lash::LashSession,
        turn_id: impl Into<lash::TurnId>,
        events: Arc<dyn lash::TurnActivitySink>,
        build: SimTurnBuild,
    ) -> Result<lash::Result<lash::TurnOutput>, FixedScriptRunnerError> {
        self.run_turn_releasing(session, turn_id, events, build, None)
            .await
    }

    /// [`run_turn`](Self::run_turn) on a session whose drive `hold` holds:
    /// the hold is released once the input is accepted, so the drive admits
    /// it together with whatever the hold kept pending — one root, as a turn
    /// sent while those inputs wait is admitted.
    pub async fn run_turn_releasing(
        &self,
        session: &lash::LashSession,
        turn_id: impl Into<lash::TurnId>,
        events: Arc<dyn lash::TurnActivitySink>,
        build: SimTurnBuild,
        hold: Option<lash_restate_test::Hold>,
    ) -> Result<lash::Result<lash::TurnOutput>, FixedScriptRunnerError> {
        let turn_id = turn_id.into();
        let collected = CollectedTurnActivity {
            live: Some(events),
            activities: std::sync::Mutex::new(Vec::new()),
        };
        let accepted = match build(session) {
            Ok(send) => send.id(turn_id).await,
            Err(err) => Err(err),
        };
        drop(hold);
        let report = match accepted {
            Ok(handle) => {
                self.await_input_drive(session, handle.input_id()).await;
                handle.output_into(&collected).await
            }
            Err(err) => Err(err),
        };
        let activities = std::mem::take(&mut *collected.activities.lock_recover());
        Ok(report.map(|result| lash::TurnOutput { result, activities }))
    }

    /// Wait for the drive `input`'s acceptance scheduled to stop: by then the
    /// root that took the input has settled on the engine, and this
    /// process's driver has deposited its report. The handle read after it
    /// answers from that report at once, so a harness waiting on a turn
    /// makes no request of its own to the server while the turn runs, and
    /// the server's grant order stays a function of the seed. A drive the
    /// engine refused ends the wait too; the handle then reports why.
    async fn await_input_drive(&self, session: &lash::LashSession, input: &lash::InputId) {
        let request = lash_core::engine::DriveRequestId::new(input.to_string());
        let session_id = session.session_id();
        loop {
            match self
                .restate
                .attach_drive(&session_id, request.clone())
                .await
            {
                Err(error) if error.is_timeout() => continue,
                Ok(_) | Err(_) => return,
            }
        }
    }

    /// Wait until the engine has no drive of `session` in flight: every
    /// `LashSession` invocation for it has completed. A turn sent while the
    /// session's last drive is still winding down (its closing admission
    /// answering idle) would race that admission, and which drive admits the
    /// new input would then depend on task timing; a world that wants one
    /// grant order per seed sends into a settled session.
    pub async fn settle_session_drive(&self, session: &lash::LashSession) {
        let prefix = format!(
            "{}/{}/",
            lash_restate_test::SESSION_DRIVER_SERVICE,
            session.session_id()
        );
        let server = self.restate.server();
        while server
            .invocations()
            .iter()
            .any(|view| view.target.starts_with(&prefix) && view.status != "completed")
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// Hold the engine's drive of `session` on the server double
    /// ([`RestateTestBackend::hold_session_drive`](lash_restate_test::RestateTestBackend::hold_session_drive)):
    /// what is sent there meanwhile stays pending until the hold is
    /// released. The world asserts what is still pending this way; it never
    /// drives a turn itself.
    pub async fn hold_session_drive(&self, session: &lash::LashSession) -> lash_restate_test::Hold {
        self.restate.hold_session_drive(&session.session_id()).await
    }
}

/// The activity a turn streamed, kept for its output and forwarded live.
struct CollectedTurnActivity {
    live: Option<Arc<dyn lash::TurnActivitySink>>,
    activities: std::sync::Mutex<Vec<lash::TurnActivity>>,
}

#[async_trait::async_trait]
impl lash::TurnActivitySink for CollectedTurnActivity {
    async fn emit(&self, activity: lash::TurnActivity) {
        self.activities.lock_recover().push(activity.clone());
        if let Some(live) = &self.live {
            live.emit(activity).await;
        }
    }

    async fn emit_for_turn(&self, turn_id: &lash::TurnId, activity: lash::TurnActivity) {
        self.activities.lock_recover().push(activity.clone());
        if let Some(live) = &self.live {
            live.emit_for_turn(turn_id, activity).await;
        }
    }
}

/// A turn's live activity nobody watches; its output keeps the activity.
pub struct DiscardedTurnActivity;

#[async_trait::async_trait]
impl lash::TurnActivitySink for DiscardedTurnActivity {
    async fn emit(&self, _activity: lash::TurnActivity) {}
}

/// The engine's backend with its session factory decorated.
///
/// A checkpoint-write observer over the session factory forwards every
/// binding to the factory it wraps, so the backend's ports still meet each
/// other exactly as the undecorated backend's do. The decorated backend
/// keeps the inner backend's Lashlang artifacts.
pub struct DecoratedBackend {
    layered: lash_core::testing::runtime_helpers::LayeredBackend,
}

impl DecoratedBackend {
    /// `inner`, undecorated.
    pub fn over(inner: Backend) -> Self {
        Self {
            layered: lash_core::testing::runtime_helpers::LayeredBackend::over(inner),
        }
    }

    /// `engine`'s backend, undecorated: it reaches the server double only
    /// through its connection, so a core over it never keeps the server
    /// alive.
    ///
    /// The session-work port is the engine's minus its wall-clock
    /// reconcile interval: the sim's serial scheduling pins one grant
    /// order per seed, and a sweep that ticks on wall time would land its
    /// drive asks — each named for a per-process nonce — wherever its
    /// store reads happened to finish. A scenario reconciles explicitly
    /// through `SessionDriver::reconcile` when it wants a pass.
    pub fn over_engine(engine: &SimEngine) -> Self {
        Self {
            layered: lash_core::testing::runtime_helpers::LayeredBackend::over(
                engine.restate.lash_backend(),
            )
            .with_session_work(Some(engine.restate.explicit_reconcile_session_work())),
        }
    }

    /// Observe the commits made through the session factory into
    /// `collector`.
    pub fn observing(self, collector: CheckpointWriteCollector) -> Self {
        Self {
            layered: self.layered.map_session_store_factory(|factory| {
                Arc::new(ObservedSessionStoreFactory::new(factory, collector))
            }),
        }
    }

    /// Wrap the effect host in `layer`, once: every controller this
    /// backend's host lends or routes then crosses the layer. One wrapper for
    /// the backend's lifetime, since a runtime installs its tool-child host
    /// get-or-init and holds the installing host weakly.
    pub fn with_effect_layer(self, layer: Arc<dyn lash_core::testing::EffectLayer>) -> Self {
        Self {
            layered: self.layered.map_effect_host(|host| {
                Arc::new(lash_core::testing::LayeredEffectHost::new(host, layer))
            }),
        }
    }
}

impl From<DecoratedBackend> for Backend {
    fn from(backend: DecoratedBackend) -> Self {
        backend.layered.into_backend()
    }
}
